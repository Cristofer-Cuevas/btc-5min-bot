use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, error, info, warn};

use crate::types::{BinanceAggTrade, SharedBinancePrice};

const BINANCE_URL: &str = "wss://stream.binance.com:9443/ws/btcusdt@aggTrade";

/// Run the Binance WebSocket connection for BTC/USDT trade feed.
/// Reconnects automatically on disconnect with exponential backoff.
pub async fn run_binance_feed(binance_price: SharedBinancePrice) {
    let mut backoff_secs = 3u64;

    loop {
        info!("Connecting to Binance WebSocket...");
        match connect_and_listen(&binance_price).await {
            Ok(()) => {
                warn!("Binance WebSocket closed cleanly, reconnecting...");
                backoff_secs = 3;
            }
            Err(e) => {
                error!(
                    "Binance WebSocket error: {}, reconnecting in {}s",
                    e, backoff_secs
                );
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)).await;
        backoff_secs = (backoff_secs * 2).min(30);
    }
}

async fn connect_and_listen(binance_price: &SharedBinancePrice) -> Result<(), String> {
    let (ws_stream, _) = connect_async(BINANCE_URL)
        .await
        .map_err(|e| format!("Connection failed: {}", e))?;

    info!("Binance WebSocket connected");
    let (mut write, mut read) = ws_stream.split();

    while let Some(msg_result) = read.next().await {
        match msg_result {
            Ok(Message::Text(text)) => {
                handle_binance_message(&text, binance_price).await;
            }
            Ok(Message::Ping(data)) => {
                if let Err(e) = write.send(Message::Pong(data)).await {
                    error!("Binance pong send failed: {}", e);
                    break;
                }
            }
            Ok(Message::Close(_)) => {
                info!("Binance WebSocket received close frame");
                break;
            }
            Err(e) => {
                error!("Binance read error: {}", e);
                break;
            }
            _ => {}
        }
    }

    Ok(())
}

async fn handle_binance_message(text: &str, binance_price: &SharedBinancePrice) {
    let trade: BinanceAggTrade = match serde_json::from_str(text) {
        Ok(t) => t,
        Err(_) => {
            debug!("Ignoring unparseable Binance message");
            return;
        }
    };

    let price_str = match trade.price {
        Some(p) => p,
        None => return,
    };

    let value: f64 = match price_str.parse() {
        Ok(v) => v,
        Err(_) => {
            debug!("Could not parse Binance price: {}", price_str);
            return;
        }
    };

    let ts = trade.trade_time.unwrap_or(0);
    let mut state = binance_price.write().await;
    state.current_price = Some(value);
    state.last_update_ms = ts;

    // Maintain rolling 120s price buffer for trend strength calculation
    state.price_buffer.push_back((ts, value));
    let cutoff = ts.saturating_sub(120_000);
    while let Some(&(front_ts, _)) = state.price_buffer.front() {
        if front_ts < cutoff {
            state.price_buffer.pop_front();
        } else {
            break;
        }
    }

    debug!("Binance BTC/USDT: ${:.2} (ts={}, buf={})", value, ts, state.price_buffer.len());
}
