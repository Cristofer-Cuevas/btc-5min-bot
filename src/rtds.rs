use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, error, info, warn};

use crate::types::{RtdsMessage, RtdsSubscribe, RtdsSubscription, SharedBtcPrice};

const RTDS_URL: &str = "wss://ws-live-data.polymarket.com";
const PING_INTERVAL_SECS: u64 = 5;

/// Run the RTDS WebSocket connection for BTC/USD Chainlink price feed.
/// Reconnects automatically on disconnect with exponential backoff.
pub async fn run_rtds_feed(btc_price: SharedBtcPrice) {
    let mut backoff_secs = 3u64;

    loop {
        info!("Connecting to RTDS WebSocket...");
        match connect_and_listen(&btc_price).await {
            Ok(()) => {
                warn!("RTDS WebSocket closed cleanly, reconnecting...");
                backoff_secs = 3;
            }
            Err(e) => {
                error!("RTDS WebSocket error: {}, reconnecting in {}s", e, backoff_secs);
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)).await;
        backoff_secs = (backoff_secs * 2).min(30);
    }
}

async fn connect_and_listen(btc_price: &SharedBtcPrice) -> Result<(), String> {
    let (ws_stream, _) = connect_async(RTDS_URL)
        .await
        .map_err(|e| format!("Connection failed: {}", e))?;

    info!("RTDS WebSocket connected");
    let (mut write, mut read) = ws_stream.split();

    // Subscribe to crypto prices
    let subscribe = RtdsSubscribe {
        action: "subscribe".into(),
        subscriptions: vec![RtdsSubscription {
            topic: "crypto_prices_chainlink".into(),
            sub_type: "update".into(),
        }],
    };

    let sub_msg = serde_json::to_string(&subscribe)
        .map_err(|e| format!("Serialize error: {}", e))?;
    write
        .send(Message::Text(sub_msg))
        .await
        .map_err(|e| format!("Send error: {}", e))?;

    info!("Subscribed to crypto_prices_chainlink");

    // Spawn ping task
    let ping_write = std::sync::Arc::new(tokio::sync::Mutex::new(write));
    let ping_handle = {
        let pw = ping_write.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(PING_INTERVAL_SECS));
            loop {
                interval.tick().await;
                let mut w = pw.lock().await;
                if w.send(Message::Ping(vec![])).await.is_err() {
                    break;
                }
            }
        })
    };

    // Read messages
    while let Some(msg_result) = read.next().await {
        match msg_result {
            Ok(Message::Text(text)) => {
                handle_rtds_message(&text, btc_price).await;
            }
            Ok(Message::Ping(data)) => {
                let mut w = ping_write.lock().await;
                let _ = w.send(Message::Pong(data)).await;
            }
            Ok(Message::Close(_)) => {
                info!("RTDS WebSocket received close frame");
                break;
            }
            Err(e) => {
                error!("RTDS read error: {}", e);
                break;
            }
            _ => {}
        }
    }

    ping_handle.abort();
    Ok(())
}

async fn handle_rtds_message(text: &str, btc_price: &SharedBtcPrice) {
    let msg: RtdsMessage = match serde_json::from_str(text) {
        Ok(m) => m,
        Err(_) => {
            debug!("Ignoring unparseable RTDS message");
            return;
        }
    };

    // Only process crypto price updates for BTC/USD
    if msg.topic.as_deref() != Some("crypto_prices_chainlink") {
        return;
    }

    if let Some(payload) = &msg.payload {
        if payload.symbol.as_deref() != Some("btc/usd") {
            return;
        }

        if let Some(value) = payload.value {
            let ts = payload.timestamp.unwrap_or(0);
            let mut state = btc_price.write().await;
            state.current_price = Some(value);
            state.last_update_ms = ts;
            debug!("BTC/USD: ${:.2} (ts={})", value, ts);
        }
    }
}
