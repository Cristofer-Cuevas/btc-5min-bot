use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, error, info, warn};

use crate::constants::{TWAP_RECONNECT_AFTER_MS, TWAP_RESUBSCRIBE_AFTER_MS};
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

/// How often the read loop wakes to run the TWAP watchdog when no frame has
/// arrived. Short enough that watchdog thresholds are honoured closely, long
/// enough to be free.
const WATCHDOG_TICK_MS: u64 = 250;

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

/// The TWAP subscription frame, used for the initial subscribe and for every
/// watchdog re-subscribe so the two can never drift apart.
fn twap_subscribe_frame() -> Result<String, String> {
    let sub = RtdsSubscribe {
        action: "subscribe".into(),
        subscriptions: vec![RtdsSubscription {
            topic: TWAP_TOPIC.into(),
            sub_type: "update".into(),
            filters: Some(TWAP_FILTER.into()),
        }],
    };
    serde_json::to_string(&sub).map_err(|e| format!("Serialize error: {}", e))
}

/// What a single inbound frame told us, so the read loop can drive the
/// watchdog without inspecting message internals itself.
#[derive(Default)]
struct MsgOutcome {
    /// A usable TWAP update landed — resets the silence timer.
    twap_update: bool,
    /// The TWAP subscription was rejected — a full reconnect is required, since
    /// a rejected subscription is not retried on an open socket.
    twap_rejected: bool,
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
    match twap_subscribe_frame() {
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

    // TWAP watchdog state. The silence timer starts at connection time so a
    // subscription that never delivers anything is caught just like one that
    // goes quiet later.
    let mut last_twap_at = std::time::Instant::now();
    let mut last_resubscribe_at: Option<std::time::Instant> = None;

    // Read messages. The read is wrapped in a short timeout so the watchdog
    // still runs while the socket is quiet — the spot feed can keep flowing
    // (or not) independently, so we cannot rely on inbound traffic to tick it.
    loop {
        let next = tokio::time::timeout(
            std::time::Duration::from_millis(WATCHDOG_TICK_MS),
            read.next(),
        )
        .await;

        let mut reconnect_needed = false;

        match next {
            Ok(Some(Ok(Message::Text(text)))) => {
                let outcome = handle_rtds_message(&text, btc_price, &mut twap_seen).await;
                if outcome.twap_update {
                    last_twap_at = std::time::Instant::now();
                    last_resubscribe_at = None;
                }
                if outcome.twap_rejected {
                    // A rejected subscription does not retry on an open socket,
                    // so only a full reconnect can recover it.
                    warn!("TWAP subscription rejected, reconnecting RTDS socket");
                    reconnect_needed = true;
                }
            }
            Ok(Some(Ok(Message::Ping(data)))) => {
                let mut w = ping_write.lock().await;
                let _ = w.send(Message::Pong(data)).await;
            }
            Ok(Some(Ok(Message::Close(_)))) => {
                info!("RTDS WebSocket received close frame");
                break;
            }
            Ok(Some(Err(e))) => {
                error!("RTDS read error: {}", e);
                break;
            }
            Ok(Some(Ok(_))) => {}
            // Stream ended.
            Ok(None) => break,
            // No frame within the tick — fall through to the watchdog.
            Err(_) => {}
        }

        // ── TWAP watchdog ──
        // Escalates: re-subscribe on the live socket first (cheap, keeps the
        // spot feed and its trading dependency completely undisturbed), then a
        // full reconnect only if silence persists.
        let silent_ms = last_twap_at.elapsed().as_millis() as u64;

        if !reconnect_needed && silent_ms >= TWAP_RECONNECT_AFTER_MS {
            warn!(
                "TWAP feed silent {}s (>= {}s), reconnecting RTDS socket to \
                 re-establish both subscriptions",
                silent_ms / 1000,
                TWAP_RECONNECT_AFTER_MS / 1000
            );
            reconnect_needed = true;
        }

        if reconnect_needed {
            ping_handle.abort();
            return Ok(());
        }

        let resubscribe_due = silent_ms >= TWAP_RESUBSCRIBE_AFTER_MS
            && last_resubscribe_at
                .map(|t| t.elapsed().as_millis() as u64 >= TWAP_RESUBSCRIBE_AFTER_MS)
                .unwrap_or(true);

        if resubscribe_due {
            info!("TWAP feed silent {}s, re-subscribing", silent_ms / 1000);
            match twap_subscribe_frame() {
                Ok(msg) => {
                    let mut w = ping_write.lock().await;
                    if let Err(e) = w.send(Message::Text(msg)).await {
                        warn!("TWAP re-subscribe send failed: {}", e);
                    }
                }
                Err(e) => warn!("TWAP re-subscribe serialize failed: {}", e),
            }
            last_resubscribe_at = Some(std::time::Instant::now());
        }
    }

    ping_handle.abort();
    Ok(())
}

async fn handle_rtds_message(
    text: &str,
    btc_price: &SharedBtcPrice,
    twap_seen: &mut bool,
) -> MsgOutcome {
    let msg: RtdsMessage = match serde_json::from_str(text) {
        Ok(m) => m,
        Err(_) => {
            debug!("Ignoring unparseable RTDS message");
            return MsgOutcome {
                twap_rejected: report_twap_subscription_error(text),
                ..Default::default()
            };
        }
    };

    match msg.topic.as_deref() {
        Some(SPOT_TOPIC) => {
            handle_spot_update(&msg, btc_price).await;
            MsgOutcome::default()
        }
        Some(TWAP_TOPIC) => MsgOutcome {
            twap_update: handle_twap_update(&msg, btc_price, twap_seen).await,
            twap_rejected: false,
        },
        _ => {
            // Not a topic we subscribed to. Still worth checking for a
            // rejection notice naming the TWAP topic.
            MsgOutcome {
                twap_rejected: report_twap_subscription_error(text),
                ..Default::default()
            }
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

/// Record the latest 30s TWAP reading. Returns true if a usable reading was
/// stored, which is what resets the watchdog's silence timer — a malformed or
/// off-symbol frame must NOT count as the feed being alive.
async fn handle_twap_update(
    msg: &RtdsMessage,
    btc_price: &SharedBtcPrice,
    twap_seen: &mut bool,
) -> bool {
    let Some(payload) = &msg.payload else {
        return false;
    };

    if payload.symbol.as_deref() != Some(BTC_SYMBOL) {
        return false;
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
        return false;
    };

    // Use the Chainlink observation time, not local receive time and not the
    // outer publisher timestamp. Left as None if the feed omits it: freshness
    // is judged solely from this value, so an absent timestamp must fail
    // closed (treated as stale) rather than default to a misleading zero.
    let observed_ms = payload.timestamp.map(|t| t as i64);
    if observed_ms.is_none() {
        warn!("TWAP update missing payload.timestamp; reading cannot be freshness-checked");
    }

    {
        let mut state = btc_price.write().await;
        state.twap_30_value = Some(exact.to_string());
        state.twap_30_observed_at_ms = observed_ms;
        state.twap_30_display_value = payload.value;
    }

    if !*twap_seen {
        *twap_seen = true;
        info!(
            "TWAP feed live: {} btc/usd = {} (window_s={:?}, observed_at_ms={:?})",
            TWAP_TOPIC,
            format_e18(exact).unwrap_or_else(|| exact.to_string()),
            payload.window_s,
            observed_ms
        );
    }

    debug!(
        "TWAP30 btc/usd: {} (raw_e18={}, observed_at_ms={:?})",
        format_e18(exact).unwrap_or_else(|| exact.to_string()),
        exact,
        observed_ms
    );

    true
}

/// RTDS does not document an error envelope, so rather than guess a schema and
/// risk swallowing a rejection silently, inspect the raw frame: anything naming
/// the TWAP topic alongside an error marker is surfaced as a warning.
///
/// A rejected subscription is not retried on an open socket, so the caller
/// escalates to a full reconnect. Returns true if a rejection was detected.
fn report_twap_subscription_error(text: &str) -> bool {
    if !text.contains(TWAP_TOPIC) {
        return false;
    }
    let lower = text.to_ascii_lowercase();
    let is_error = lower.contains("not found")
        || lower.contains("error")
        || lower.contains("invalid")
        || lower.contains("reject")
        || lower.contains("unauthorized");
    if !is_error {
        return false;
    }
    let excerpt: String = text.chars().take(300).collect();
    warn!(
        "TWAP subscription appears rejected (spot feed unaffected): {}",
        excerpt
    );
    true
}
