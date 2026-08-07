use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, error, info, warn};

use crate::types::{format_e18, RtdsMessage, RtdsSubscribe, RtdsSubscription, SharedBtcPrice};

const RTDS_URL: &str = "wss://ws-live-data.polymarket.com";
const PING_INTERVAL_SECS: u64 = 5;

const SPOT_TOPIC: &str = "crypto_prices_chainlink";

/// Chainlink 30-second TWAP topic (the lookback window used by 5-minute
/// markets). Data collection only — no trading logic reads it.
const TWAP_TOPIC: &str = "crypto_prices_twap_thirty";

/// RTDS requires `filters` to be a JSON-encoded *string* in exactly this
/// compact form: one lowercase symbol, no spaces.
const TWAP_FILTER: &str = r#"{"symbol":"btc/usd"}"#;

const BTC_SYMBOL: &str = "btc/usd";

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

    // Subscribe to crypto prices (spot). Sent as its own frame — see below.
    let subscribe = RtdsSubscribe {
        action: "subscribe".into(),
        subscriptions: vec![RtdsSubscription {
            topic: SPOT_TOPIC.into(),
            sub_type: "update".into(),
            filters: None,
        }],
    };

    let sub_msg = serde_json::to_string(&subscribe)
        .map_err(|e| format!("Serialize error: {}", e))?;
    write
        .send(Message::Text(sub_msg))
        .await
        .map_err(|e| format!("Send error: {}", e))?;

    info!("Subscribed to {}", SPOT_TOPIC);

    // Subscribe to the 30s TWAP feed on the SAME socket, but in a SEPARATE
    // frame from the spot subscription above. RTDS accepts multiple
    // subscriptions per connection, and the docs warn that a TWAP subscription
    // can come back as "topic not found" before the feed is live. Keeping it in
    // its own frame means such a rejection can never take the spot
    // subscription — which the trading logic depends on — down with it.
    //
    // A rejected subscription is not retried on an open socket, so recovery is
    // deliberately left to the next reconnect, which re-sends both frames.
    let twap_subscribe = RtdsSubscribe {
        action: "subscribe".into(),
        subscriptions: vec![RtdsSubscription {
            topic: TWAP_TOPIC.into(),
            sub_type: "update".into(),
            filters: Some(TWAP_FILTER.into()),
        }],
    };

    match serde_json::to_string(&twap_subscribe) {
        Ok(msg) => {
            if let Err(e) = write.send(Message::Text(msg)).await {
                // Non-fatal: the spot feed is already subscribed and is what
                // trading depends on. Log and carry on.
                warn!("Failed to send TWAP subscription (spot feed unaffected): {}", e);
            } else {
                info!("Subscribed to {} filters={}", TWAP_TOPIC, TWAP_FILTER);
            }
        }
        Err(e) => warn!("Failed to serialize TWAP subscription: {}", e),
    }

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

    // Tracks whether this connection has ever produced a usable TWAP update, so
    // a silently-rejected subscription surfaces instead of looking like an idle
    // feed. Per-connection: reset every reconnect.
    let mut twap_seen = false;

    // Read messages
    while let Some(msg_result) = read.next().await {
        match msg_result {
            Ok(Message::Text(text)) => {
                handle_rtds_message(&text, btc_price, &mut twap_seen).await;
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

async fn handle_rtds_message(text: &str, btc_price: &SharedBtcPrice, twap_seen: &mut bool) {
    let msg: RtdsMessage = match serde_json::from_str(text) {
        Ok(m) => m,
        Err(_) => {
            report_twap_subscription_error(text);
            debug!("Ignoring unparseable RTDS message");
            return;
        }
    };

    match msg.topic.as_deref() {
        Some(SPOT_TOPIC) => handle_spot_update(&msg, btc_price).await,
        Some(TWAP_TOPIC) => handle_twap_update(&msg, btc_price, twap_seen).await,
        _ => {
            // Not a topic we subscribed to. Still worth checking for a
            // rejection notice naming the TWAP topic.
            report_twap_subscription_error(text);
        }
    }
}

async fn handle_spot_update(msg: &RtdsMessage, btc_price: &SharedBtcPrice) {
    let Some(payload) = &msg.payload else { return };

    // Only process crypto price updates for BTC/USD
    if payload.symbol.as_deref() != Some(BTC_SYMBOL) {
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

/// Record the latest 30s TWAP reading. DATA COLLECTION ONLY — this updates
/// shared state for logging and nothing else reads it.
async fn handle_twap_update(msg: &RtdsMessage, btc_price: &SharedBtcPrice, twap_seen: &mut bool) {
    let Some(payload) = &msg.payload else { return };

    if payload.symbol.as_deref() != Some(BTC_SYMBOL) {
        return;
    }

    // `full_accuracy_value` is the exact E18 fixed-point settlement value;
    // `value` is documented as display convenience only. Store the exact
    // string and never round a settlement price through f64.
    let Some(exact) = payload.full_accuracy_value.as_deref() else {
        warn!(
            "TWAP update missing full_accuracy_value (display value={:?}); \
             not recording — refusing to store a rounded settlement price",
            payload.value
        );
        return;
    };

    // Use the Chainlink observation time, not local receive time and not the
    // outer publisher timestamp.
    let observed_ms = payload.timestamp.unwrap_or(0);

    {
        let mut state = btc_price.write().await;
        state.twap_30_value = Some(exact.to_string());
        state.twap_30_observed_at_ms = observed_ms;
        state.twap_30_display_value = payload.value;
    }

    if !*twap_seen {
        *twap_seen = true;
        info!(
            "TWAP feed live: {} btc/usd = {} (window_s={:?}, observed_at_ms={})",
            TWAP_TOPIC,
            format_e18(exact).unwrap_or_else(|| exact.to_string()),
            payload.window_s,
            observed_ms
        );
    }

    debug!(
        "TWAP30 btc/usd: {} (raw_e18={}, observed_at_ms={})",
        format_e18(exact).unwrap_or_else(|| exact.to_string()),
        exact,
        observed_ms
    );
}

/// RTDS does not document an error envelope, so rather than guess a schema and
/// risk swallowing a rejection silently, inspect the raw frame: anything naming
/// the TWAP topic alongside an error marker is surfaced as a warning.
///
/// A rejected subscription is not retried on an open socket, so the next
/// reconnect is what re-attempts it.
fn report_twap_subscription_error(text: &str) {
    if !text.contains(TWAP_TOPIC) {
        return;
    }
    let lower = text.to_ascii_lowercase();
    let is_error = lower.contains("not found")
        || lower.contains("error")
        || lower.contains("invalid")
        || lower.contains("reject")
        || lower.contains("unauthorized");
    if !is_error {
        return;
    }
    let excerpt: String = text.chars().take(300).collect();
    warn!(
        "TWAP subscription appears rejected (will retry on next reconnect, \
         spot feed unaffected): {}",
        excerpt
    );
}
