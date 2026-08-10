use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, error, info, warn};

use crate::types::{ClobBookLevel, ClobSubscribe, ClobWsMessage, SharedMarketState};

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
) {
    let mut backoff_secs = 3u64;
    let shared_tokens: std::sync::Arc<tokio::sync::Mutex<Option<(String, String)>>> =
        std::sync::Arc::new(tokio::sync::Mutex::new(None));

    loop {
        info!("Connecting to CLOB WebSocket...");

        match connect_async(CLOB_WS_URL).await {
            Ok((ws_stream, _)) => {
                info!("CLOB WebSocket connected");
                backoff_secs = 3;

                let (write, mut read) = ws_stream.split();
                let write = std::sync::Arc::new(tokio::sync::Mutex::new(write));

                // Spawn keepalive task — WS-level ping every 10s
                // TODO: if Polymarket still drops us, try sending {"type":"PING"} text instead
                let ping_handle = {
                    let pw = write.clone();
                    tokio::spawn(async move {
                        let mut interval = tokio::time::interval(std::time::Duration::from_secs(10));
                        loop {
                            interval.tick().await;
                            let mut w = pw.lock().await;
                            if w.send(Message::Ping(vec![])).await.is_err() {
                                break;
                            }
                            debug!("CLOB WS ping sent");
                        }
                    })
                };

                // Re-subscribe to current tokens if we had any
                let current_tokens = shared_tokens.lock().await.clone();
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
                        ws_msg = read.next() => {
                            match ws_msg {
                                Some(Ok(Message::Text(text))) => {
                                    let tokens = shared_tokens.lock().await.clone();
                                    handle_clob_message(
                                        &text,
                                        &market_state,
                                        &resolution_tx,
                                        &tokens,
                                    )
                                    .await;
                                }
                                Some(Ok(Message::Ping(data))) => {
                                    let mut w = write.lock().await;
                                    let _ = w.send(Message::Pong(data)).await;
                                }
                                Some(Ok(Message::Close(_))) => {
                                    warn!("CLOB WebSocket closed");
                                    break;
                                }
                                Some(Err(e)) => {
                                    error!("CLOB WebSocket read error: {}", e);
                                    break;
                                }
                                None => break,
                                _ => {}
                            }
                        }
                        // Handle commands from main loop
                        cmd = cmd_rx.recv() => {
                            match cmd {
                                Some(ClobCommand::Subscribe { up_token_id, down_token_id }) => {
                                    let sub = ClobSubscribe {
                                        assets_ids: vec![up_token_id.clone(), down_token_id.clone()],
                                        sub_type: "market".into(),
                                        custom_feature_enabled: true,
                                    };
                                    if let Ok(msg) = serde_json::to_string(&sub) {
                                        let mut w = write.lock().await;
                                        if let Err(e) = w.send(Message::Text(msg)).await {
                                            error!("Failed to send CLOB subscribe: {}", e);
                                        } else {
                                            info!("Subscribed to CLOB tokens");
                                        }
                                    }
                                    *shared_tokens.lock().await = Some((up_token_id, down_token_id));
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
                warn!("CLOB WebSocket disconnected, will reconnect");
            }
            Err(e) => {
                error!("CLOB WebSocket connection failed: {}", e);
            }
        }

        tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)).await;
        backoff_secs = (backoff_secs * 2).min(30);
    }
}

async fn handle_clob_message(
    text: &str,
    market_state: &SharedMarketState,
    resolution_tx: &mpsc::Sender<ResolutionEvent>,
    current_tokens: &Option<(String, String)>,
) {
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
        let asset_id = msg.asset_id.as_deref().unwrap_or("");
        let is_up = asset_id == up_token;
        let is_down = asset_id == down_token;

        if !is_up && !is_down && event_type != "market_resolved" {
            continue;
        }

        match event_type {
            "book" => {
                let mut state = market_state.write().await;
                let book = if is_up {
                    &mut state.up_book
                } else {
                    &mut state.down_book
                };
                book.best_bid = best_price(&msg.bids, true);
                book.best_ask = best_price(&msg.asks, false);

                // ADD: compute total depth
                book.ask_depth = msg.asks.as_ref().map(|levels| {
                    levels.iter()
                        .filter_map(|l| l.size.parse::<f64>().ok())
                        .sum()
                });
                book.bid_depth = msg.bids.as_ref().map(|levels| {
                    levels.iter()
                        .filter_map(|l| l.size.parse::<f64>().ok())
                        .sum()
                });

                // Full ask ladder sorted ascending by price, for fillable-depth
                // queries (ask_depth_up_to). Cleared and rebuilt on every book
                // snapshot; not maintained between snapshots.
                book.ask_levels = msg.asks.as_ref().map(|levels| {
                    let mut v: Vec<(f64, f64)> = levels.iter()
                        .filter_map(|l| {
                            let p = l.price.parse::<f64>().ok()?;
                            let s = l.size.parse::<f64>().ok()?;
                            Some((p, s))
                        })
                        .collect();
                    v.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
                    v
                }).unwrap_or_default();
            }
            "price_change" => {
                let mut state = market_state.write().await;
                let book = if is_up {
                    &mut state.up_book
                } else {
                    &mut state.down_book
                };
                // NOTE: best_bid/best_ask are refreshed here, but ask_levels
                // (and ask_depth) are NOT — they only update on full `book`
                // snapshots, so the ladder may lag best_ask between snapshots.
                if let Some(ref bid) = msg.best_bid {
                    book.best_bid = bid.parse().ok();
                }
                if let Some(ref ask) = msg.best_ask {
                    book.best_ask = ask.parse().ok();
                }
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
                    book.last_trade_price = price.parse().ok();
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

fn best_price(levels: &Option<Vec<ClobBookLevel>>, is_bid: bool) -> Option<f64> {
    levels.as_ref().and_then(|levels| {
        levels
            .iter()
            .filter_map(|l| l.price.parse::<f64>().ok())
            .reduce(if is_bid { f64::max } else { f64::min })
    })
}

#[cfg(test)]
mod resolution_scope_tests {
    use super::*;
    use crate::types::MarketState;
    use std::sync::Arc;
    use tokio::sync::RwLock;

    const UP: &str = "up-token-current";
    const DOWN: &str = "down-token-current";

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
