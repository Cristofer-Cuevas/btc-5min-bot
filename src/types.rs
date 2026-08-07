use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use tokio::sync::RwLock;

// ── Market Window ──

#[derive(Debug, Clone)]
pub struct MarketWindow {
    pub up_token_id: String,
    pub down_token_id: String,
    pub neg_risk: bool,
    pub tick_size: String,
}

// ── Order Book State ──

#[derive(Debug, Clone, Default)]
pub struct TokenBook {
    pub best_bid: Option<f64>,
    pub best_ask: Option<f64>,
    pub last_trade_price: Option<f64>,
    pub ask_depth: Option<f64>,
    pub bid_depth: Option<f64>,
    /// Full ask ladder from the most recent `book` snapshot, sorted ascending
    /// by price. Stale between snapshots (not rebuilt on price_change events).
    pub ask_levels: Vec<(f64, f64)>,
}

impl TokenBook {
    pub fn spread(&self) -> Option<f64> {
        match (self.best_bid, self.best_ask) {
            (Some(bid), Some(ask)) => Some(ask - bid),
            _ => None,
        }
    }

    /// Total shares available at prices <= max_price (fillable depth up to a
    /// limit). Returns 0.0 if no levels or none qualify — that's intentional
    /// fail-closed behavior: an empty/unpopulated ladder fails the depth gate.
    pub fn ask_depth_up_to(&self, max_price: f64) -> f64 {
        self.ask_levels
            .iter()
            .filter(|(price, _)| *price <= max_price + 1e-9)
            .map(|(_, size)| size)
            .sum()
    }
}

#[derive(Debug, Clone, Default)]
pub struct MarketState {
    pub up_book: TokenBook,
    pub down_book: TokenBook,
    pub resolved: bool,
    pub winning_outcome: Option<String>,
    pub up_trade_count: u32,
    pub down_trade_count: u32,
}

// ── BTC Price State ──

#[derive(Debug, Clone, Default)]
pub struct BtcPriceState {
    pub current_price: Option<f64>,
    pub window_open_price: Option<f64>,
    pub last_update_ms: u64,
    /// Latest Chainlink 30s TWAP, held as the exact signed E18 fixed-point
    /// integer string straight from `payload.full_accuracy_value`. This is a
    /// settlement price, so it is never parsed through f64 — see
    /// [`format_e18`] to render it for display.
    ///
    /// DATA COLLECTION ONLY: nothing in strategy/entry/resolution reads this.
    pub twap_30_value: Option<String>,
    /// Chainlink observation time for `twap_30_value` (`payload.timestamp`,
    /// ms), not local receive time.
    ///
    /// NOTE: the "30 seconds" is a LOOKBACK WINDOW, not a publication cadence.
    /// Judge freshness only from this timestamp — never from how often updates
    /// arrive, since the feed's update rate says nothing about staleness.
    pub twap_30_observed_at_ms: u64,
    /// `payload.value`, the feed's display-only float. Diagnostics and logging
    /// only — never persisted to a settlement column.
    pub twap_30_display_value: Option<f64>,
}

/// Render an exact signed E18 fixed-point integer string (Chainlink
/// `full_accuracy_value`) as a decimal string.
///
/// Uses string arithmetic only, so arbitrarily large values round-trip without
/// the precision loss an f64 or a fixed-width integer would introduce. Returns
/// `None` if `raw` is not a plain, optionally-signed integer.
pub fn format_e18(raw: &str) -> Option<String> {
    let s = raw.trim();
    let (neg, digits) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }

    let trimmed = digits.trim_start_matches('0');
    let trimmed = if trimmed.is_empty() { "0" } else { trimmed };

    let (int_part, frac_raw) = if trimmed.len() > 18 {
        trimmed.split_at(trimmed.len() - 18)
    } else {
        ("0", trimmed)
    };
    let frac_padded = format!("{:0>18}", frac_raw);
    let frac = frac_padded.trim_end_matches('0');

    let is_zero = int_part == "0" && frac.is_empty();
    let mut out = String::new();
    if neg && !is_zero {
        out.push('-');
    }
    out.push_str(int_part);
    if !frac.is_empty() {
        out.push('.');
        out.push_str(frac);
    }
    Some(out)
}

// ── Binance BTC Price State ──

#[derive(Debug, Clone, Default)]
pub struct BinanceBtcPrice {
    pub current_price: Option<f64>,
    pub window_open_price: Option<f64>,
    pub last_update_ms: u64,
    pub price_buffer: VecDeque<(u64, f64)>,
}

impl BinanceBtcPrice {
    /// Returns Some(strength) where strength is 0.0–1.0, or None if insufficient samples
    /// OR if the buffer spans less than 90 seconds of real time.
    pub fn trend_strength(&self) -> Option<f64> {
        // Need a real time span, not just a sample count. A cold buffer
        // (e.g. right after a Binance reconnect) can hold many ticks over
        // only a few seconds, which trivially scores ~1.0 and defeats the
        // choppiness filter. Require >= 90s of actual elapsed data.
        let front_ts = self.price_buffer.front()?.0;
        let back_ts = self.price_buffer.back()?.0;
        if back_ts.saturating_sub(front_ts) < 90_000 {
            return None;
        }

        if self.price_buffer.len() < crate::constants::MIN_TREND_SAMPLES {
            return None;
        }

        let prices: Vec<f64> = self.price_buffer.iter().map(|(_, p)| *p).collect();
        let first = prices.first()?;
        let last = prices.last()?;

        let net = (last - first).abs();
        let gross: f64 = prices.windows(2).map(|w| (w[1] - w[0]).abs()).sum();

        if gross < 1e-9 {
            return Some(1.0);
        }

        Some(net / gross)
    }
}

// ── Binance WebSocket Messages ──

#[derive(Debug, Deserialize)]
pub struct BinanceAggTrade {
    #[serde(rename = "p")]
    pub price: Option<String>,
    #[serde(rename = "T")]
    pub trade_time: Option<u64>,
}

// ── Window Trading State ──

#[derive(Debug, Clone, Default)]
pub struct WindowState {
    pub window_ts: u64,
    pub entered: bool,
    pub paused: bool,
    pub market: Option<MarketWindow>,
    pub failed_attempts: u32,
    pub last_signal_reason: Option<String>,
    pub next_window_prefetched: bool,
    pub pending_retry_signal: Option<EntrySignal>,
    pub last_attempt_failed_at_ms: Option<i64>,
}

// ── Trade Record (for DB) ──

#[derive(Debug, Clone)]
pub struct TradeRecord {
    pub timestamp: i64,
    pub window_ts: i64,
    pub slug: String,
    pub side: String,
    pub btc_delta_pct: f64,
    pub entry_price: f64,
    pub shares: f64,
    pub cost_usdc: f64,
    pub secs_left: i64,
    pub resolution: Option<String>,
    pub won: Option<bool>,
    pub profit: Option<f64>,
    pub order_id: Option<String>,
    pub dry_run: bool,
    // Market snapshot at entry
    pub ask_price_observed: Option<f64>,
    pub bid_price_observed: Option<f64>,
    pub spread_observed: Option<f64>,
    pub ask_depth: Option<f64>,
    pub bid_depth: Option<f64>,
    pub up_trade_count: Option<i32>,
    pub down_trade_count: Option<i32>,
    pub opposite_side_ask: Option<f64>,
    // Price sources at entry
    pub binance_price_entry: Option<f64>,
    pub binance_open_price: Option<f64>,
    pub rtds_price_entry: Option<f64>,
    pub rtds_open_price: Option<f64>,
    pub rtds_stale_at_entry: Option<bool>,
    pub trend_strength: Option<f64>,
    // Fill quality
    pub limit_price: Option<f64>,
    pub fill_price: Option<f64>,
    pub fill_attempts: Option<i32>,
    // Timing (Unix milliseconds)
    pub signal_detected_ms: Option<i64>,
    pub order_sent_ms: Option<i64>,
    pub order_ack_ms: Option<i64>,
    // Bot metadata
    pub bot_version: String,
    pub neg_risk: bool,
}

// ── Strategy Evaluation Result ──

#[derive(Debug, Clone)]
pub struct EvaluationResult {
    pub signal: Option<EntrySignal>,
    pub rejection_reason: &'static str,
    pub btc_delta_pct: Option<f64>,
    pub ask_price: Option<f64>,
    pub bid_price: Option<f64>,
    pub spread: Option<f64>,
    pub ask_depth: Option<f64>,
    pub trade_count: Option<u32>,
    pub trend_strength: Option<f64>,
    pub side: Option<String>,
}

impl EvaluationResult {
    pub fn rejected(reason: &'static str) -> Self {
        Self {
            signal: None,
            rejection_reason: reason,
            btc_delta_pct: None,
            ask_price: None,
            bid_price: None,
            spread: None,
            ask_depth: None,
            trade_count: None,
            trend_strength: None,
            side: None,
        }
    }
}

// ── Gamma API Response ──

#[derive(Debug, Deserialize)]
pub struct GammaMarket {
    pub outcomes: Option<String>,
    #[serde(rename = "clobTokenIds")]
    pub clob_token_ids: Option<String>,
    #[serde(rename = "negRisk")]
    pub neg_risk: Option<bool>,
    #[serde(rename = "tickSize")]
    pub tick_size: Option<String>,
}

// ── RTDS WebSocket Messages ──

#[derive(Debug, Serialize)]
pub struct RtdsSubscribe {
    pub action: String,
    pub subscriptions: Vec<RtdsSubscription>,
}

#[derive(Debug, Serialize)]
pub struct RtdsSubscription {
    pub topic: String,
    #[serde(rename = "type")]
    pub sub_type: String,
    /// JSON-*encoded string* (not a nested object), in the exact compact form
    /// the RTDS docs require: `{"symbol":"btc/usd"}` — lowercase, no spaces.
    /// Skipped when absent so the existing spot subscription frame serializes
    /// exactly as it did before.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filters: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct RtdsMessage {
    pub topic: Option<String>,
    pub payload: Option<RtdsPayload>,
}

#[derive(Debug, Deserialize)]
pub struct RtdsPayload {
    pub symbol: Option<String>,
    pub value: Option<f64>,
    pub timestamp: Option<u64>,
    /// TWAP topics only: the exact signed E18 fixed-point value, as a string.
    /// `value` above is documented as display-only for these topics.
    pub full_accuracy_value: Option<String>,
    /// TWAP topics only: the lookback window in seconds (30 or 60).
    pub window_s: Option<u32>,
}

#[cfg(test)]
mod rtds_tests {
    use super::*;

    /// The spot frame must serialize exactly as it did before `filters` was
    /// added, i.e. with no `filters` key at all.
    #[test]
    fn spot_subscription_omits_filters() {
        let sub = RtdsSubscribe {
            action: "subscribe".into(),
            subscriptions: vec![RtdsSubscription {
                topic: "crypto_prices_chainlink".into(),
                sub_type: "update".into(),
                filters: None,
            }],
        };
        let json = serde_json::to_string(&sub).unwrap();
        assert!(!json.contains("filters"), "spot frame changed: {}", json);
        assert_eq!(
            json,
            r#"{"action":"subscribe","subscriptions":[{"topic":"crypto_prices_chainlink","type":"update"}]}"#
        );
    }

    /// RTDS requires `filters` to be a JSON-encoded *string*, not a nested
    /// object, in compact lowercase form.
    #[test]
    fn twap_subscription_encodes_filters_as_string() {
        let sub = RtdsSubscribe {
            action: "subscribe".into(),
            subscriptions: vec![RtdsSubscription {
                topic: "crypto_prices_twap_thirty".into(),
                sub_type: "update".into(),
                filters: Some(r#"{"symbol":"btc/usd"}"#.into()),
            }],
        };
        let json = serde_json::to_string(&sub).unwrap();
        assert_eq!(
            json,
            r#"{"action":"subscribe","subscriptions":[{"topic":"crypto_prices_twap_thirty","type":"update","filters":"{\"symbol\":\"btc/usd\"}"}]}"#
        );
    }

    /// The documented TWAP payload must parse, preserving the exact value as a
    /// string and the Chainlink observation timestamp.
    #[test]
    fn parses_documented_twap_payload() {
        let raw = r#"{"topic":"crypto_prices_twap_thirty","type":"update","timestamp":1785178800123,
            "payload":{"symbol":"btc/usd","value":65000.5,
            "full_accuracy_value":"65000500000000000000000",
            "timestamp":1785178800000,"window_s":30}}"#;
        let msg: RtdsMessage = serde_json::from_str(raw).unwrap();
        assert_eq!(msg.topic.as_deref(), Some("crypto_prices_twap_thirty"));
        let p = msg.payload.unwrap();
        assert_eq!(p.full_accuracy_value.as_deref(), Some("65000500000000000000000"));
        assert_eq!(p.timestamp, Some(1785178800000));
        assert_eq!(p.window_s, Some(30));
        assert_eq!(format_e18(&p.full_accuracy_value.unwrap()).unwrap(), "65000.5");
    }

    /// The pre-existing spot payload must still parse unchanged.
    #[test]
    fn parses_spot_payload_unchanged() {
        let raw = r#"{"topic":"crypto_prices_chainlink",
            "payload":{"symbol":"btc/usd","value":65000.5,"timestamp":1785178800000}}"#;
        let msg: RtdsMessage = serde_json::from_str(raw).unwrap();
        let p = msg.payload.unwrap();
        assert_eq!(p.value, Some(65000.5));
        assert_eq!(p.full_accuracy_value, None);
    }

    #[test]
    fn format_e18_preserves_full_precision() {
        assert_eq!(format_e18("65000500000000000000000").unwrap(), "65000.5");
        assert_eq!(
            format_e18("65000123456789012345678").unwrap(),
            "65000.123456789012345678"
        );
        assert_eq!(format_e18("500000000000000000").unwrap(), "0.5");
        assert_eq!(format_e18("1").unwrap(), "0.000000000000000001");
        assert_eq!(format_e18("0").unwrap(), "0");
        assert_eq!(format_e18("-65000500000000000000000").unwrap(), "-65000.5");
        assert_eq!(format_e18("12.5"), None);
        assert_eq!(format_e18("abc"), None);
        assert_eq!(format_e18(""), None);
    }
}

// ── CLOB WebSocket Messages ──

#[derive(Debug, Serialize)]
pub struct ClobSubscribe {
    pub assets_ids: Vec<String>,
    #[serde(rename = "type")]
    pub sub_type: String,
    pub custom_feature_enabled: bool,
}

#[derive(Debug, Deserialize)]
pub struct ClobWsMessage {
    pub event_type: Option<String>,
    pub asset_id: Option<String>,
    // book event
    pub bids: Option<Vec<ClobBookLevel>>,
    pub asks: Option<Vec<ClobBookLevel>>,
    // price_change event
    pub best_bid: Option<String>,
    pub best_ask: Option<String>,
    // last_trade_price event
    pub price: Option<String>,
    // market_resolved
    pub winning_outcome: Option<String>,
    pub winning_asset_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ClobBookLevel {
    pub price: String,
    pub size: String,
}

// ── Shared App State ──

pub type SharedConfig = Arc<RwLock<crate::config::RuntimeConfig>>;
pub type SharedBtcPrice = Arc<RwLock<BtcPriceState>>;
pub type SharedBinancePrice = Arc<RwLock<BinanceBtcPrice>>;
pub type SharedMarketState = Arc<RwLock<MarketState>>;
pub type SharedWindowState = Arc<RwLock<WindowState>>;
pub type SharedWallet = Arc<alloy::signers::local::PrivateKeySigner>;
pub type SharedSdkClient = Arc<crate::trading::AuthedSdkClient>;
pub type SharedTokenWindowMap = Arc<RwLock<HashMap<String, u64>>>;

// ── Strategy Signal ──

#[derive(Debug, Clone)]
pub struct EntrySignal {
    pub side: String,           // "Up" or "Down"
    pub token_id: String,
    pub btc_delta_pct: f64,
    pub ask_price: f64,
    pub spread: f64,
    pub secs_left: i64,
}

// ── Fill Result (from CLOB order response) ──

#[derive(Debug, Clone)]
pub struct FillResult {
    pub order_id: String,
    pub fill_price: f64,
    pub filled_size: f64,
}

// ── Stats ──

#[derive(Debug, Clone, Default)]
pub struct TradingStats {
    pub trades: i64,
    pub wins: i64,
    pub losses: i64,
    pub total_cost: f64,
    pub total_payout: f64,
    pub net_pnl: f64,
    pub win_rate: f64,
}
