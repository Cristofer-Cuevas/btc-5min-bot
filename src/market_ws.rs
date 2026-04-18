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
                    // Reset trade counts to avoid phantom trades from replayed book data
                    {
                        let mut ms = market_state.write().await;
                        ms.up_trade_count = 0;
                        ms.down_trade_count = 0;
                        debug!("Reset trade counts on (re)subscribe");
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
                                    // Reset trade counts on subscribe to avoid phantom trades
                                    {
                                        let mut ms = market_state.write().await;
                                        ms.up_trade_count = 0;
                                        ms.down_trade_count = 0;
                                        debug!("Reset trade counts on (re)subscribe");
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
            }
            "price_change" => {
                let mut state = market_state.write().await;
                let book = if is_up {
                    &mut state.up_book
                } else {
                    &mut state.down_book
                };
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
                let mut state = market_state.write().await;
                state.resolved = true;
                state.winning_outcome = msg.winning_outcome.clone();

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

