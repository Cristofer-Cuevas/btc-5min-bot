use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, error, info, warn};

use crate::types::{ClobBookLevel, ClobPriceChange, ClobSubscribe, ClobWsMessage, SharedMarketState, TokenBook};

const CLOB_WS_URL: &str = "wss://ws-subscriptions-clob.polymarket.com/ws/market";

/// Commands sent to the CLOB WebSocket task.
pub enum ClobCommand {
    Subscribe {
        up_token_id: String,
        down_token_id: String,
    },
}

/// Resolution event sent from the WebSocket to the main loop.
#[derive(Debug, Clone)]
pub struct ResolutionEvent {
    pub winning_outcome: String,
    pub winning_asset_id: String,
}

/// Run the CLOB market WebSocket with reconnection.
pub async fn run_clob_ws(
    market_state: SharedMarketState,
    mut cmd_rx: mpsc::Receiver<ClobCommand>,
    resolution_tx: mpsc::Sender<ResolutionEvent>,
    tape: Option<crate::recorder::TapeSender>,
) {
    let mut backoff_secs = 3u64;
    let mut dropped_events = 0usize;
    let shared_tokens: std::sync::Arc<tokio::sync::Mutex<Option<(String, String)>>> =
        std::sync::Arc::new(tokio::sync::Mutex::new(None));

    loop {
        info!("Connecting to CLOB WebSocket...");

        match connect_async(CLOB_WS_URL).await {
            Ok((ws_stream, _)) => {
                info!("CLOB WebSocket connected");
                crate::recorder::record(&tape, &mut dropped_events, r#"{"event_type":"capture_connected"}"#);
                backoff_secs = 3;

                let (write, mut read) = ws_stream.split();
                let write = std::sync::Arc::new(tokio::sync::Mutex::new(write));

                // The CLOB requires application-level text PING, not a WS ping.
                let ping_handle = {
                    let pw = write.clone();
                    tokio::spawn(async move {
                        let mut interval = tokio::time::interval(std::time::Duration::from_secs(10));
                        loop {
                            interval.tick().await;
                            let mut w = pw.lock().await;
                            if w.send(Message::Text("PING".into())).await.is_err() {
                                break;
                            }
                            debug!("CLOB WS ping sent");
                        }
                    })
                };

                // Re-subscribe to current tokens if we had any
                let current_tokens = shared_tokens.lock().await.clone();
                let mut subscribed = current_tokens.is_some();
                if let Some((ref up, ref down)) = current_tokens {
                    let sub = ClobSubscribe {
                        assets_ids: vec![up.clone(), down.clone()],
                        sub_type: "market".into(),
                        custom_feature_enabled: true,
                    };
                    if let Ok(msg) = serde_json::to_string(&sub) {
                        let mut w = write.lock().await;
                        let _ = w.send(Message::Text(msg)).await;
                        info!("Re-subscribed to tokens on reconnect");
                    }
                    // Reset trade counts and books to avoid phantom trades and
                    // stale prices from replayed book data after a reconnect.
                    {
                        let mut ms = market_state.write().await;
                        ms.up_trade_count = 0;
                        ms.down_trade_count = 0;
                        ms.up_book = Default::default();
                        ms.down_book = Default::default();
                        debug!("Reset trade counts and books on (re)subscribe");
                    }
                }

                // Use select! to handle both commands and messages without moving cmd_rx
                loop {
                    tokio::select! {
                        // Handle incoming WebSocket messages
                        ws_msg = tokio::time::timeout(std::time::Duration::from_secs(30), read.next()) => {
                            match ws_msg {
                                Ok(Some(Ok(Message::Text(text)))) => {
                                    if text != "PONG" {
                                        crate::recorder::record(&tape, &mut dropped_events, &text);
                                    }
                                    let tokens = shared_tokens.lock().await.clone();
                                    handle_clob_message(
                                        &text,
                                        &market_state,
                                        &resolution_tx,
                                        &tokens,
                                    )
                                    .await;
                                }
                                Ok(Some(Ok(Message::Ping(data)))) => {
                                    let mut w = write.lock().await;
                                    let _ = w.send(Message::Pong(data)).await;
                                }
                                Ok(Some(Ok(Message::Close(_)))) => {
                                    warn!("CLOB WebSocket closed");
                                    break;
                                }
                                Ok(Some(Err(e))) => {
                                    error!("CLOB WebSocket read error: {}", e);
                                    break;
                                }
                                Ok(None) => break,
                                Err(_) => {
                                    warn!("CLOB heartbeat timed out; clearing books and reconnecting");
                                    break;
                                }
                                _ => {}
                            }
                        }
                        // Handle commands from main loop
                        cmd = cmd_rx.recv() => {
                            match cmd {
                                Some(ClobCommand::Subscribe { up_token_id, down_token_id }) => {
                                    let sub = if subscribed {
                                        serde_json::json!({
                                            "assets_ids": [up_token_id.clone(), down_token_id.clone()],
                                            "operation": "subscribe", "custom_feature_enabled": true
                                        })
                                    } else {
                                        serde_json::json!({
                                            "assets_ids": [up_token_id.clone(), down_token_id.clone()],
                                            "type": "market", "custom_feature_enabled": true
                                        })
                                    };
                                    *shared_tokens.lock().await = Some((up_token_id, down_token_id));
                                    if let Ok(msg) = serde_json::to_string(&sub) {
                                        let mut w = write.lock().await;
                                        if let Err(e) = w.send(Message::Text(msg)).await {
                                            error!("Failed to send CLOB subscribe: {}", e);
                                            break;
                                        } else {
                                            info!("Subscribed to CLOB tokens");
                                            subscribed = true;
                                        }
                                    }
                                    // Reset trade counts on subscribe to avoid phantom trades.
                                    //
                                    // The books are cleared here too. Between window rotation
                                    // and this command being processed, `shared_tokens` still
                                    // held the OLD token ids, so late `book`/`price_change`
                                    // events for the previous window's tokens passed the
                                    // is_up/is_down check and repopulated the books that
                                    // rotation had just cleared. Clearing at the moment the new
                                    // subscription is issued discards that carry-over.
                                    //
                                    // Safe to clear: the server sends a fresh `book` snapshot
                                    // for the new tokens, and until it lands best_ask is None,
                                    // so evaluate_entry rejects with "no_ask" and
                                    // ask_depth_up_to returns 0.0 to fail the depth gate. A
                                    // brief no-trade gap is strictly better than quoting prices
                                    // for tokens that no longer exist.
                                    {
                                        let mut ms = market_state.write().await;
                                        ms.up_trade_count = 0;
                                        ms.down_trade_count = 0;
                                        ms.up_book = Default::default();
                                        ms.down_book = Default::default();
                                        debug!("Reset trade counts and books on (re)subscribe");
                                    }
                                }
                                None => {
                                    info!("Command channel closed, shutting down CLOB WS");
                                    ping_handle.abort();
                                    return;
                                }
                            }
                        }
                    }
                }

                ping_handle.abort();
                crate::recorder::record(&tape, &mut dropped_events, r#"{"event_type":"capture_disconnected"}"#);
                clear_books(&market_state).await;
                warn!("CLOB WebSocket disconnected, will reconnect");
            }
            Err(e) => {
                clear_books(&market_state).await;
                error!("CLOB WebSocket connection failed: {}", e);
            }
        }

        tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)).await;
        backoff_secs = (backoff_secs * 2).min(30);
    }
}

async fn clear_books(market_state: &SharedMarketState) {
    let mut state = market_state.write().await;
    state.up_book = TokenBook::default();
    state.down_book = TokenBook::default();
}

async fn handle_clob_message(
    text: &str,
    market_state: &SharedMarketState,
    resolution_tx: &mpsc::Sender<ResolutionEvent>,
    current_tokens: &Option<(String, String)>,
) {
    if text == "PONG" {
        return;
    }
    // CLOB sends arrays of events
    let messages: Vec<ClobWsMessage> = match serde_json::from_str(text) {
        Ok(m) => m,
        Err(_) => {
            // Try single message
            match serde_json::from_str::<ClobWsMessage>(text) {
                Ok(m) => vec![m],
                Err(_) => {
                    debug!("Ignoring unparseable CLOB message");
                    return;
                }
            }
        }
    };

    let (up_token, down_token) = match current_tokens.as_ref() {
        Some(t) => t,
        None => return,
    };

    for msg in messages {
        let event_type = msg.event_type.as_deref().unwrap_or("");
        // Since September 2025 price_change identifies assets inside the
        // price_changes array. Filtering by the outer asset_id drops every
        // incremental update and leaves cancelled liquidity in the ladder.
        if event_type == "price_change" {
            let Some(timestamp) = event_timestamp(msg.timestamp.as_deref()) else { continue };
            let Some(changes) = msg.price_changes.as_ref() else { continue };
            let mut state = market_state.write().await;
            let up_changes: Vec<_> = changes.iter().filter(|c| c.asset_id == *up_token).collect();
            let down_changes: Vec<_> = changes.iter().filter(|c| c.asset_id == *down_token).collect();
            apply_changes(&mut state.up_book, &up_changes, timestamp);
            apply_changes(&mut state.down_book, &down_changes, timestamp);
            continue;
        }
        let asset_id = msg.asset_id.as_deref().unwrap_or("");
        let is_up = asset_id == up_token;
        let is_down = asset_id == down_token;

        if !is_up && !is_down && event_type != "market_resolved" {
            continue;
        }

        match event_type {
            "book" => {
                let Some(timestamp) = event_timestamp(msg.timestamp.as_deref()) else { continue };
                let mut state = market_state.write().await;
                let book = if is_up {
                    &mut state.up_book
                } else {
                    &mut state.down_book
                };
                if timestamp < book.last_update_ms {
                    continue;
                }
                let (Some(asks), Some(bids)) =
                    (parse_levels(msg.asks.as_deref()), parse_levels(msg.bids.as_deref()))
                else {
                    *book = TokenBook::default();
                    continue;
                };
                book.ask_levels = asks;
                book.bid_levels = bids;
                refresh_book(book, timestamp);
            }
            "last_trade_price" => {
                if let Some(ref price) = msg.price {
                    let mut state = market_state.write().await;
                    let book = if is_up {
                        state.up_trade_count += 1;    // ← ADD
                        &mut state.up_book
                    } else {
                        state.down_trade_count += 1;  // ← ADD
                        &mut state.down_book
                    };
                    book.last_trade_price = valid_price(price);
                }
            }
            "market_resolved" => {
                info!("Market resolved! outcome={:?}", msg.winning_outcome);

                // Does this resolution belong to the window we are CURRENTLY
                // trading? Resolutions arrive minutes after their window closed
                // (measured: ~145-160s), by which time the next window is well
                // under way. Setting the shared `resolved` flag from a previous
                // window's event marks the live market as resolved and blocks
                // every remaining entry in it.
                //
                // The flag is therefore only set for an event naming one of the
                // tokens we are subscribed to right now.
                let belongs_to_current = is_up
                    || is_down
                    || msg
                        .winning_asset_id
                        .as_deref()
                        .is_some_and(|a| a == up_token || a == down_token);

                if belongs_to_current {
                    let mut state = market_state.write().await;
                    state.resolved = true;
                    state.winning_outcome = msg.winning_outcome.clone();
                } else {
                    info!(
                        "Ignoring resolution for asset {:?} — not a token of the current \
                         window; forwarding for settlement only",
                        msg.winning_asset_id
                    );
                }

                // Forwarded either way: the main loop maps the winning asset
                // back to its own window via the token→window map to settle the
                // right trade, and late resolutions are exactly what it expects.
                if let (Some(outcome), Some(asset_id)) =
                    (msg.winning_outcome, msg.winning_asset_id)
                {
                    let _ = resolution_tx
                        .send(ResolutionEvent {
                            winning_outcome: outcome,
                            winning_asset_id: asset_id,
                        })
                        .await;
                }
            }
            _ => {}
        }
    }
}

fn event_timestamp(raw: Option<&str>) -> Option<u64> {
    let timestamp: u64 = raw?.parse().ok()?;
    let now = chrono::Utc::now().timestamp_millis() as u64;
    (timestamp > 0 && timestamp <= now.saturating_add(1_000)).then_some(timestamp)
}

fn valid_price(raw: &str) -> Option<f64> {
    let value: f64 = raw.parse().ok()?;
    (value.is_finite() && value > 0.0 && value < 1.0).then_some(value)
}

fn parse_levels(raw: Option<&[ClobBookLevel]>) -> Option<Vec<(f64, f64)>> {
    let mut levels = Vec::new();
    for level in raw? {
        let price = valid_price(&level.price)?;
        let size: f64 = level.size.parse().ok()?;
        if !size.is_finite() || size < 0.0 {
            return None;
        }
        if size > 0.0 {
            update_level(&mut levels, price, size);
        }
    }
    Some(levels)
}

fn update_level(levels: &mut Vec<(f64, f64)>, price: f64, size: f64) {
    levels.retain(|(p, _)| (*p - price).abs() > 1e-9);
    if size > 0.0 {
        levels.push((price, size));
        levels.sort_by(|a, b| a.0.total_cmp(&b.0));
    }
}

fn refresh_book(book: &mut TokenBook, timestamp: u64) {
    book.best_ask = book.ask_levels.first().map(|level| level.0);
    book.best_bid = book.bid_levels.last().map(|level| level.0);
    book.ask_depth = Some(book.ask_levels.iter().map(|level| level.1).sum());
    book.bid_depth = Some(book.bid_levels.iter().map(|level| level.1).sum());
    book.last_update_ms = timestamp;
    if book.spread().is_some_and(|spread| spread < 0.0) {
        // A crossed local book indicates a lost or inconsistent update.
        // Wait for a full snapshot before exposing any liquidity again.
        *book = TokenBook::default();
    }
}

fn apply_changes(book: &mut TokenBook, changes: &[&ClobPriceChange], timestamp: u64) {
    // A delta cannot reconstruct a complete book after reconnecting.
    if changes.is_empty() || book.last_update_ms == 0 || timestamp < book.last_update_ms {
        return;
    }
    // A frame may change several levels of one token. Apply it atomically:
    // intermediate ladders can disagree with the final BBO in that frame.
    for change in changes {
        let price = valid_price(&change.price);
        let size = change.size.parse::<f64>().ok().filter(|v| v.is_finite() && *v >= 0.0);
        let (Some(price), Some(size)) = (price, size) else {
            *book = TokenBook::default();
            return;
        };
        let levels = match change.side.as_str() {
            "SELL" => &mut book.ask_levels,
            "BUY" => &mut book.bid_levels,
            _ => {
                *book = TokenBook::default();
                return;
            }
        };
        update_level(levels, price, size);
    }
    refresh_book(book, timestamp);
    let change = changes.last().expect("nonempty changes");
    // The server's BBO is also a consistency check against missed levels.
    // Do not retain apparently fillable depth when that check fails.
    for (reported, actual) in [
        (change.best_ask.as_deref(), book.best_ask),
        (change.best_bid.as_deref(), book.best_bid),
    ] {
        if let Some(reported) = reported {
            if valid_price(reported) != actual {
                *book = TokenBook::default();
                return;
            }
        }
    }
}

#[cfg(test)]
mod resolution_scope_tests {
    use super::*;
    use crate::types::MarketState;
    use std::sync::Arc;
    use tokio::sync::RwLock;

    const UP: &str = "up-token-current";
    const DOWN: &str = "down-token-current";

    #[tokio::test]
    async fn nested_price_changes_update_both_ladders_and_delete_empty_levels() {
        let state: SharedMarketState = Arc::new(RwLock::new(MarketState::default()));
        let (tx, _) = mpsc::channel(4);
        let tokens = Some((UP.to_string(), DOWN.to_string()));
        let snapshot = serde_json::json!({"event_type":"book","asset_id":UP,"timestamp":"1000",
            "bids":[{"price":"0.48","size":"90"}],
            "asks":[{"price":"0.50","size":"80"},{"price":"0.51","size":"70"}]});
        handle_clob_message(&snapshot.to_string(), &state, &tx, &tokens).await;
        let changes = serde_json::json!({"event_type":"price_change","timestamp":"1001","price_changes":[
            {"asset_id":UP,"price":"0.50","size":"0","side":"SELL","best_bid":"0.49","best_ask":"0.51"},
            {"asset_id":UP,"price":"0.49","size":"60","side":"BUY","best_bid":"0.49","best_ask":"0.51"}]});
        handle_clob_message(&changes.to_string(), &state, &tx, &tokens).await;
        let book = state.read().await.up_book.clone();
        assert_eq!(book.best_ask, Some(0.51));
        assert_eq!(book.best_bid, Some(0.49));
        assert_eq!(book.ask_depth_up_to(0.50), 0.0);
        assert_eq!(book.bid_depth, Some(150.0));
        assert_eq!(book.last_update_ms, 1001);
        // An old snapshot must not resurrect deleted liquidity.
        handle_clob_message(&snapshot.to_string(), &state, &tx, &tokens).await;
        assert_eq!(state.read().await.up_book.best_ask, Some(0.51));
        clear_books(&state).await;
        handle_clob_message(&changes.to_string(), &state, &tx, &tokens).await;
        assert_eq!(state.read().await.up_book.last_update_ms, 0);
    }

    async fn feed(json: &str) -> (bool, Option<ResolutionEvent>) {
        let ms: SharedMarketState = Arc::new(RwLock::new(MarketState::default()));
        let (tx, mut rx) = mpsc::channel(4);
        let tokens = Some((UP.to_string(), DOWN.to_string()));
        handle_clob_message(json, &ms, &tx, &tokens).await;
        let resolved = ms.read().await.resolved;
        (resolved, rx.try_recv().ok())
    }

    /// The bug: a resolution for a PREVIOUS window arrives minutes late
    /// (measured ~145-160s into the next window) and must not mark the live
    /// market resolved — that blocks every remaining entry in it.
    #[tokio::test]
    async fn foreign_resolution_does_not_mark_current_window_resolved() {
        let (resolved, forwarded) = feed(
            r#"[{"event_type":"market_resolved","winning_outcome":"Up",
                 "winning_asset_id":"some-older-windows-token"}]"#,
        )
        .await;
        assert!(!resolved, "a previous window's resolution poisoned the live window");
        // Still forwarded so the main loop can settle the trade it belongs to.
        let ev = forwarded.expect("resolution must still be forwarded for settlement");
        assert_eq!(ev.winning_asset_id, "some-older-windows-token");
        assert_eq!(ev.winning_outcome, "Up");
    }

    /// A resolution for the window we are actually trading still sets the flag.
    #[tokio::test]
    async fn own_resolution_marks_current_window_resolved() {
        let json = format!(
            r#"[{{"event_type":"market_resolved","winning_outcome":"Down","winning_asset_id":"{}"}}]"#,
            DOWN
        );
        let (resolved, forwarded) = feed(&json).await;
        assert!(resolved, "own resolution must mark the window resolved");
        assert_eq!(forwarded.expect("forwarded").winning_asset_id, DOWN);
    }

    /// Matching on the event's own asset_id works too.
    #[tokio::test]
    async fn own_resolution_via_asset_id_field() {
        let json = format!(
            r#"[{{"event_type":"market_resolved","asset_id":"{}","winning_outcome":"Up",
                  "winning_asset_id":"{}"}}]"#,
            UP, UP
        );
        assert!(feed(&json).await.0);
    }
}
