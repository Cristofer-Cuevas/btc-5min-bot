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
    /// ms), not local receive time. `None` until the feed delivers a reading
    /// carrying a timestamp — freshness cannot be judged without one.
    ///
    /// NOTE: the "30 seconds" is a LOOKBACK WINDOW, not a publication cadence.
    /// Judge freshness only from this timestamp — never from how often updates
    /// arrive, since the feed's update rate says nothing about staleness.
    pub twap_30_observed_at_ms: Option<i64>,
    /// `payload.value`, the feed's display-only float. Diagnostics and logging
    /// only — never persisted to a settlement column.
    pub twap_30_display_value: Option<f64>,
}

impl BtcPriceState {
    /// Returns (value, observed_at_ms) only if the reading is fresh.
    ///
    /// The RTDS TWAP feed can go silent for long stretches with no backfill
    /// (docs: "no snapshot, history, or replay after a disconnect"), so a
    /// last-known value can be hours old. 30s is a LOOKBACK WINDOW, not a
    /// publication rate — freshness must come from observed_at_ms.
    ///
    /// DATA COLLECTION ONLY: no trading path calls this.
    pub fn fresh_twap_30(&self, now_ms: i64, max_age_ms: i64) -> Option<(String, i64)> {
        let v = self.twap_30_value.as_ref()?;
        let obs = self.twap_30_observed_at_ms?;
        if now_ms.saturating_sub(obs) > max_age_ms {
            return None;
        }
        Some((v.clone(), obs))
    }
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
    /// DATA COLLECTION ONLY. Set once the window's open TWAP/snapshot capture
    /// has run (at or after `window_ts`); cleared on rotation so each window
    /// captures exactly once. Read by no trading path.
    pub open_captured: bool,
    /// DATA COLLECTION ONLY. Set once the outgoing window's boundary capture
    /// has run (at or after `window_ts + WINDOW_SECS`); cleared on rotation.
    pub resolve_captured: bool,
    /// Rolling record of whether the last N windows had a FRESH TWAP at the
    /// resolve capture point, newest at the back. Health metric for the feed
    /// dependency, surfaced via Telegram /status.
    ///
    /// Deliberately NOT cleared on rotation — it spans windows by design.
    pub twap_coverage_recent: VecDeque<bool>,
    /// The window's strike: the TWAP at window open, as the raw E18 string.
    /// `None` when the open capture found no fresh reading — never backfilled.
    /// Cleared on rotation.
    pub twap_strike: Option<String>,
    /// Chainlink observation time of `twap_strike`.
    pub twap_strike_observed_ms: Option<i64>,
}

impl WindowState {
    /// Record whether this window's resolve capture found a fresh TWAP,
    /// evicting the oldest sample beyond `TWAP_COVERAGE_WINDOW`.
    pub fn push_twap_coverage(&mut self, fresh: bool) {
        self.twap_coverage_recent.push_back(fresh);
        while self.twap_coverage_recent.len() > crate::constants::TWAP_COVERAGE_WINDOW {
            self.twap_coverage_recent.pop_front();
        }
    }

    /// (fresh_count, sample_count) over the tracked windows.
    pub fn twap_coverage(&self) -> (usize, usize) {
        (
            self.twap_coverage_recent.iter().filter(|f| **f).count(),
            self.twap_coverage_recent.len(),
        )
    }
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
    // TWAP strike comparison
    /// TWAP-based delta at entry. Recorded in both modes.
    pub twap_delta_pct_at_entry: Option<f64>,
    /// The window's TWAP strike, raw E18 string.
    pub twap_strike_at_entry: Option<String>,
    /// Which model produced this trade: true = TWAP delta drove the threshold
    /// and side, false = Binance spot delta did.
    pub used_twap_strike: bool,
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
    /// TWAP-based delta vs the window's strike. Always computed when available,
    /// regardless of `use_twap_strike` — under the default (false) it is
    /// recorded for comparison only and drives nothing.
    pub twap_delta_pct: Option<f64>,
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
            twap_delta_pct: None,
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

    fn state_with(value: Option<&str>, obs: Option<i64>) -> BtcPriceState {
        BtcPriceState {
            twap_30_value: value.map(|v| v.to_string()),
            twap_30_observed_at_ms: obs,
            ..Default::default()
        }
    }

    const NOW: i64 = 1_785_178_800_000;
    const MAX_AGE: i64 = 60_000;

    #[test]
    fn fresh_twap_returns_value_within_max_age() {
        let s = state_with(Some("65000500000000000000000"), Some(NOW - 59_000));
        let (v, obs) = s.fresh_twap_30(NOW, MAX_AGE).unwrap();
        assert_eq!(v, "65000500000000000000000");
        assert_eq!(obs, NOW - 59_000);
    }

    #[test]
    fn fresh_twap_rejects_stale_reading() {
        // The real bug: one reading replayed across ~110 minutes of windows.
        let s = state_with(Some("65000500000000000000000"), Some(NOW - 110 * 60_000));
        assert_eq!(s.fresh_twap_30(NOW, MAX_AGE), None);
    }

    #[test]
    fn fresh_twap_boundary_is_inclusive_at_max_age() {
        let s = state_with(Some("1"), Some(NOW - MAX_AGE));
        assert!(s.fresh_twap_30(NOW, MAX_AGE).is_some(), "exactly max_age is fresh");
        let s = state_with(Some("1"), Some(NOW - MAX_AGE - 1));
        assert!(s.fresh_twap_30(NOW, MAX_AGE).is_none(), "one ms past is stale");
    }

    #[test]
    fn fresh_twap_none_without_value_or_timestamp() {
        assert_eq!(state_with(None, Some(NOW)).fresh_twap_30(NOW, MAX_AGE), None);
        // No observation time means freshness cannot be judged: fail closed.
        assert_eq!(state_with(Some("1"), None).fresh_twap_30(NOW, MAX_AGE), None);
        assert_eq!(BtcPriceState::default().fresh_twap_30(NOW, MAX_AGE), None);
    }

    #[test]
    fn twap_delta_pct_is_exact() {
        // 65000 -> 65065 is exactly +0.1%
        let strike = "65000000000000000000000";
        let cur = "65065000000000000000000";
        assert_eq!(twap_delta_pct(strike, cur).unwrap(), 0.1);

        // Reverse direction: -65/65065*100 = -0.0999000999...
        let back = twap_delta_pct(cur, strike).unwrap();
        assert!((back - -0.0999000999000999).abs() < 1e-15, "got {}", back);

        // No move at all.
        assert_eq!(twap_delta_pct(strike, strike).unwrap(), 0.0);
    }

    #[test]
    fn twap_delta_pct_resolves_sub_cent_moves() {
        // The single miss in the collected data turned on $0.11. A delta this
        // small must still carry the correct sign and magnitude.
        let strike = "65000000000000000000000"; // 65000.00
        let cur = "65000110000000000000000"; //    65000.11
        let d = twap_delta_pct(strike, cur).unwrap();
        assert!(d > 0.0, "sign lost on a $0.11 move: {}", d);
        assert!((d - 0.000169230769).abs() < 1e-12, "got {}", d);
    }

    #[test]
    fn twap_delta_pct_rejects_bad_input() {
        assert_eq!(twap_delta_pct("0", "65000000000000000000000"), None);
        assert_eq!(twap_delta_pct("abc", "65000000000000000000000"), None);
        assert_eq!(twap_delta_pct("65000000000000000000000", "12.5"), None);
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

/// Percentage change from `strike_e18` to `current_e18`, both raw signed E18
/// fixed-point strings from the TWAP feed.
///
/// The arithmetic runs entirely in `Decimal` — the E18 strings are converted to
/// exact decimal values via [`format_e18`] (lossless string manipulation) and
/// never pass through f64. Only the final percentage, which is compared against
/// an f64 threshold, is narrowed on return.
///
/// Returns `None` if either value is unparseable or the strike is zero.
pub fn twap_delta_pct(strike_e18: &str, current_e18: &str) -> Option<f64> {
    use rust_decimal::prelude::ToPrimitive;
    use rust_decimal::Decimal;
    use std::str::FromStr;

    let strike = Decimal::from_str(&format_e18(strike_e18)?).ok()?;
    let current = Decimal::from_str(&format_e18(current_e18)?).ok()?;
    if strike.is_zero() {
        return None;
    }

    let delta = (current - strike)
        .checked_div(strike)?
        .checked_mul(Decimal::from(100))?;
    delta.to_f64()
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
