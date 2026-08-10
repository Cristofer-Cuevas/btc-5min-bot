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

/// Shared across windows and mutated by the CLOB WebSocket task. EVERY field
/// here carries meaning for exactly one window, so all of them must be cleared
/// on rotation — see [`MarketState::reset_for_new_window`].
#[derive(Debug, Clone, Default)]
pub struct MarketState {
    pub up_book: TokenBook,
    pub down_book: TokenBook,
    pub resolved: bool,
    pub winning_outcome: Option<String>,
    pub up_trade_count: u32,
    pub down_trade_count: u32,
}

impl MarketState {
    /// Clear all per-window state at rotation.
    ///
    /// Every field is window-scoped: `resolved`/`winning_outcome` describe one
    /// market, the trade counts are per-window activity, and the books hold
    /// prices for token ids that cease to exist when the window turns. Leaving
    /// any of them set lets the previous window's data drive the next window's
    /// decisions.
    ///
    /// Equivalent to assigning `Self::default()`, but named so the intent is
    /// explicit and the behavior is directly testable.
    pub fn reset_for_new_window(&mut self) {
        *self = Self::default();
    }
}

// ── TWAP Source ──

/// Which feed produced a TWAP reading. Recorded alongside every stored value so
/// it is always possible to tell which source fed a given decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TwapSource {
    /// Polymarket RTDS relay of the Chainlink TWAP (primary).
    Rtds,
    /// Chainlink Data Streams direct (fallback).
    Chainlink,
}

impl TwapSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            TwapSource::Rtds => "rtds",
            TwapSource::Chainlink => "chainlink",
        }
    }
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
    /// Which feed produced `twap_30_value`.
    pub twap_30_source: Option<TwapSource>,
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

    /// Time-weighted average price over the trailing `window_ms`, ending at
    /// the newest sample. Weights each price by how long it stood, matching
    /// TWAP semantics (NOT a simple mean of ticks, which would over-weight
    /// bursty periods).
    /// Returns None if the buffer does not span at least `window_ms`.
    ///
    /// SHADOW/DATA COLLECTION ONLY — no gate, threshold, or side selection
    /// reads this.
    ///
    /// Each sample's price is held until the NEXT sample arrives, so sample
    /// `i` is weighted by `t[i+1] - t[i]`, clipped to the window. That makes
    /// the newest sample weight zero: no time has yet elapsed at its price.
    /// The sample straddling the window start still contributes, weighted only
    /// for the portion of its life inside the window.
    pub fn twap(&self, window_ms: u64) -> Option<f64> {
        if window_ms == 0 || self.price_buffer.len() < 2 {
            return None;
        }

        let newest_ts = self.price_buffer.back()?.0;
        let oldest_ts = self.price_buffer.front()?.0;

        // Fail closed, mirroring the span guard in trend_strength(): never
        // extrapolate or pad a buffer that does not actually cover the window.
        // This also rejects a buffer poisoned by a zero timestamp (a trade
        // message with no trade_time), which would otherwise yield a span of 0.
        if newest_ts.saturating_sub(oldest_ts) < window_ms {
            return None;
        }
        let window_start = newest_ts.saturating_sub(window_ms);

        let mut weighted_sum = 0.0f64;
        let mut total_weight: u64 = 0;
        // Forward-only cursor: guarantees the weighted intervals tile the
        // window without overlapping, so total_weight can never exceed
        // window_ms even if timestamps arrive out of order.
        let mut cursor = window_start;

        for ((t0, p0), (t1, _)) in self
            .price_buffer
            .iter()
            .zip(self.price_buffer.iter().skip(1))
        {
            // Binance timestamps can repeat or arrive out of order; a
            // zero-width or backwards interval contributes nothing rather than
            // dividing by zero or subtracting weight.
            if t1 <= t0 {
                continue;
            }
            let start = (*t0).max(cursor);
            let end = (*t1).min(newest_ts);
            if end <= start {
                continue;
            }
            let dt = end - start;
            weighted_sum += p0 * dt as f64;
            total_weight += dt;
            cursor = end;
        }

        if total_weight == 0 {
            return None;
        }
        Some(weighted_sum / total_weight as f64)
    }
}

/// Percentage change between two Binance-derived TWAP values.
///
/// SHADOW ONLY. Binance publishes float prices, so this path is f64 by nature
/// — kept deliberately separate from the Chainlink E18 string path, which must
/// never round through f64.
pub fn binance_twap_delta_pct(strike: f64, current: f64) -> Option<f64> {
    if !strike.is_finite() || !current.is_finite() || strike == 0.0 {
        return None;
    }
    Some((current - strike) / strike * 100.0)
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
    /// Rolling record of the last N windows' resolve captures, newest at the
    /// back: `Some(source)` when a FRESH reading was found and which feed it
    /// came from, `None` when the window was dark. Health metric for the feed
    /// dependency, surfaced via Telegram /status.
    ///
    /// Deliberately NOT cleared on rotation — it spans windows by design.
    pub twap_coverage_recent: VecDeque<Option<TwapSource>>,
    /// The window's strike: the TWAP at window open, as the raw E18 string.
    /// `None` when the open capture found no fresh reading — never backfilled.
    /// Cleared on rotation.
    pub twap_strike: Option<String>,
    /// Chainlink observation time of `twap_strike`.
    pub twap_strike_observed_ms: Option<i64>,
    /// SHADOW ONLY. Binance-derived 30s TWAP at window open, for offline
    /// comparison against the Chainlink strike. Never read by a decision.
    pub binance_twap_strike: Option<f64>,
    /// Bot wall-clock at the Binance strike capture.
    pub binance_twap_strike_ms: Option<i64>,
}

/// SHADOW / DATA COLLECTION ONLY. Context gathered at signal-write time so the
/// Binance-vs-Chainlink TWAP comparison can be reconstructed offline, including
/// the lead-time cross-correlation. Nothing here is read by any gate,
/// threshold, side selection, or order path.
#[derive(Debug, Clone, Default)]
pub struct SignalShadowContext {
    pub binance_twap_delta_pct: Option<f64>,
    pub binance_twap_value: Option<f64>,
    pub binance_twap_strike: Option<f64>,
    pub binance_spot_price: Option<f64>,
    /// Raw E18 string — kept distinct from the f64 Binance columns.
    pub chainlink_twap_value: Option<String>,
    pub chainlink_twap_strike: Option<String>,
    /// Buffer health: distinguishes a thin buffer (e.g. post-reconnect) from
    /// genuine divergence when a Binance estimate is missing or poor.
    pub binance_buffer_span_ms: Option<i64>,
    pub binance_buffer_samples: Option<i64>,
    pub binance_newest_sample_ms: Option<i64>,
    /// Feed observation time of the Chainlink value used, for lead-time
    /// measurement against the Binance series.
    pub chainlink_twap_observed_ms: Option<i64>,
    pub signal_evaluated_at_ms: i64,
}

impl WindowState {
    /// Record this window's resolve capture — `Some(source)` if a fresh reading
    /// was found — evicting the oldest sample beyond `TWAP_COVERAGE_WINDOW`.
    pub fn push_twap_coverage(&mut self, source: Option<TwapSource>) {
        self.twap_coverage_recent.push_back(source);
        while self.twap_coverage_recent.len() > crate::constants::TWAP_COVERAGE_WINDOW {
            self.twap_coverage_recent.pop_front();
        }
    }

    /// (fresh_count, sample_count, rtds_count, chainlink_count) over the
    /// tracked windows, so /status can show how much coverage depends on the
    /// fallback rather than the primary feed.
    pub fn twap_coverage(&self) -> (usize, usize, usize, usize) {
        let fresh = self.twap_coverage_recent.iter().flatten().count();
        let rtds = self
            .twap_coverage_recent
            .iter()
            .flatten()
            .filter(|s| **s == TwapSource::Rtds)
            .count();
        let chainlink = self
            .twap_coverage_recent
            .iter()
            .flatten()
            .filter(|s| **s == TwapSource::Chainlink)
            .count();
        (fresh, self.twap_coverage_recent.len(), rtds, chainlink)
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
    /// Which feed supplied the TWAP reading at entry ("rtds"/"chainlink").
    pub twap_source_at_entry: Option<String>,
    // SHADOW ONLY — recorded for offline analysis, never read by a decision.
    pub binance_twap_delta_at_entry: Option<f64>,
    pub binance_twap_strike_at_entry: Option<f64>,
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
    /// SHADOW ONLY. Binance-derived TWAP delta vs the Binance strike. Computed
    /// and persisted for offline analysis; never read by any gate, threshold,
    /// side selection, or ordering decision in either mode.
    pub binance_twap_delta_pct: Option<f64>,
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
            binance_twap_delta_pct: None,
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

    fn buf(samples: &[(u64, f64)]) -> BinanceBtcPrice {
        BinanceBtcPrice {
            price_buffer: samples.iter().copied().collect(),
            ..Default::default()
        }
    }

    /// The whole experiment rests on this being TIME-weighted. A simple mean of
    /// ticks would give 200 here; the correct answer is ~103.33 because $100
    /// stood for 29 of the 30 seconds.
    #[test]
    fn binance_twap_is_time_weighted_not_sample_weighted() {
        let b = buf(&[(0, 100.0), (29_000, 200.0), (30_000, 300.0)]);
        let t = b.twap(30_000).expect("spans the window");
        let expected = (100.0 * 29_000.0 + 200.0 * 1_000.0) / 30_000.0;
        assert!((t - expected).abs() < 1e-9, "got {t}, expected {expected}");
        assert!((t - 103.333_333_333).abs() < 1e-6, "got {t}");

        let simple_mean = (100.0 + 200.0 + 300.0) / 3.0;
        assert!(
            (t - simple_mean).abs() > 90.0,
            "time-weighting must differ sharply from a tick mean"
        );
    }

    /// The newest sample carries zero weight: no time has elapsed at its price.
    #[test]
    fn binance_twap_newest_sample_has_no_weight() {
        // A wild final print must not move a TWAP whose window it just entered.
        let a = buf(&[(0, 100.0), (30_000, 100.0)]).twap(30_000).unwrap();
        let b = buf(&[(0, 100.0), (30_000, 999_999.0)]).twap(30_000).unwrap();
        assert_eq!(a, b);
        assert_eq!(a, 100.0);
    }

    /// Fail closed on a buffer that does not cover the window — never pad or
    /// extrapolate. Mirrors the span guard in trend_strength().
    #[test]
    fn binance_twap_requires_full_span() {
        assert_eq!(buf(&[(0, 100.0), (29_999, 200.0)]).twap(30_000), None);
        assert!(buf(&[(0, 100.0), (30_000, 200.0)]).twap(30_000).is_some());
        // Empty / single-sample buffers.
        assert_eq!(buf(&[]).twap(30_000), None);
        assert_eq!(buf(&[(0, 100.0)]).twap(30_000), None);
        // A zero timestamp (trade_time missing) as the newest sample collapses
        // the span, so it fails closed rather than producing nonsense.
        assert_eq!(buf(&[(50_000, 100.0), (0, 200.0)]).twap(30_000), None);
    }

    /// Repeated or backwards timestamps must never divide by zero, produce
    /// NaN/inf, or double-count time. Malformed ordering is bounded, not
    /// exact — the guarantee is that the result stays a genuine weighted
    /// average of observed prices.
    #[test]
    fn binance_twap_survives_zero_width_and_backwards_intervals() {
        let b = buf(&[
            (0, 100.0),
            (10_000, 100.0),
            (10_000, 500.0), // duplicate stamp: zero width, no weight
            (5_000, 900.0),  // backwards
            (30_000, 200.0),
        ]);
        let t = b.twap(30_000).expect("still spans the window");
        assert!(t.is_finite(), "must not be NaN/inf");
        // A weighted average is a convex combination: it can never fall
        // outside the observed price range.
        assert!(
            (100.0..=900.0).contains(&t),
            "{t} outside the observed price range"
        );
    }

    /// The forward-only cursor keeps weights non-overlapping, so a duplicate or
    /// backwards stamp cannot inflate total weight past the window. Verified by
    /// the identity: a constant-price buffer must return exactly that price.
    #[test]
    fn binance_twap_weights_never_overlap() {
        let b = buf(&[
            (0, 250.0),
            (5_000, 250.0),
            (5_000, 250.0),  // duplicate
            (2_000, 250.0),  // backwards
            (20_000, 250.0),
            (30_000, 250.0),
        ]);
        // If any interval were double-counted the normalisation would still
        // return 250 here, so also check a two-price case where overlap shows.
        assert!((b.twap(30_000).unwrap() - 250.0).abs() < 1e-9);

        let c = buf(&[(0, 100.0), (15_000, 100.0), (1_000, 300.0), (30_000, 100.0)]);
        let t = c.twap(30_000).unwrap();
        assert!((100.0..=300.0).contains(&t), "got {t}");
    }

    /// A sample straddling the window start counts only for its in-window life.
    #[test]
    fn binance_twap_clips_to_the_window() {
        // Window [30_000, 60_000]: 100 holds for 10s inside, 200 for 20s.
        let b = buf(&[(0, 100.0), (40_000, 200.0), (60_000, 300.0)]);
        let t = b.twap(30_000).unwrap();
        let expected = (100.0 * 10_000.0 + 200.0 * 20_000.0) / 30_000.0;
        assert!((t - expected).abs() < 1e-9, "got {t}, expected {expected}");
    }

    #[test]
    fn binance_twap_delta_pct_basic() {
        assert_eq!(binance_twap_delta_pct(100.0, 101.0), Some(1.0));
        assert_eq!(binance_twap_delta_pct(100.0, 100.0), Some(0.0));
        assert_eq!(binance_twap_delta_pct(0.0, 100.0), None);
        assert_eq!(binance_twap_delta_pct(f64::NAN, 100.0), None);
        let d = binance_twap_delta_pct(100.0, 99.0).unwrap();
        assert!(d < 0.0);
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
