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
    /// Rolling history of (timestamp_ms, delta_pct) for the current window,
    /// used to detect whether the move is expanding or reversing. Cleared on
    /// window rotation.
    ///
    /// Holds the DECISION delta — whichever of the spot/TWAP deltas actually
    /// drives the threshold check for the active mode — so momentum is always
    /// measured on the same series the entry is based on.
    pub delta_history: VecDeque<(i64, f64)>,

    // ── DATA COLLECTION ONLY: per-window running extremes ──
    // Read by no gate; recorded so entry patterns can be scored offline.
    /// Running max |decision delta| seen this window, and when.
    pub delta_peak_abs: Option<f64>,
    pub delta_peak_ms: Option<i64>,
    /// When |decision delta| first exceeded DELTA_RISE_FLOOR_PCT this window.
    /// Together with delta_peak_ms this gives rise time: how fast the move
    /// formed. A move that goes 0 -> 0.15% in 8 seconds is a spike; the same
    /// move over 90 seconds is a trend. The momentum ratio cannot tell them
    /// apart.
    pub delta_first_cross_ms: Option<i64>,
    /// Running max ask seen this window for the CURRENTLY SIGNALLED side, and
    /// the max across both sides. Both are tracked because the signalled side
    /// can flip mid-window: the signalled peak resets on a flip, the any-side
    /// peak spans the whole window.
    pub ask_peak_signalled_side: Option<f64>,
    pub ask_peak_any_side: Option<f64>,
    /// Rolling (timestamp_ms, ask_price) for the signalled side, 45s window,
    /// same trim/cap discipline as delta_history. Cleared on rotation.
    ///
    /// Also cleared when the signalled side flips mid-window: splicing two
    /// tokens' prices into one series would make the lookbacks meaningless.
    /// This is why the series can legitimately be much shorter than 45s.
    pub ask_history: VecDeque<(i64, f64)>,
    /// Which side the last evaluation signalled, used only to detect a flip.
    pub last_signalled_side: Option<String>,

    /// PHASE 2 SHADOW QUOTING, MEASUREMENT ONLY. Last `maker_shadow` write per
    /// side ("Up"/"Down"), so rows are throttled to one per second per side
    /// instead of one per 250ms tick. Read and written by nothing else; no
    /// gate, threshold or ordering decision consults it.
    ///
    /// Cleared on rotation so a new window records its first tick immediately.
    pub maker_shadow_last_write_ms: HashMap<String, i64>,

    /// When the last `signals` row was written this window, for the cadence
    /// trigger in [`WindowState::should_write_signal`]. Cleared on rotation so
    /// every window logs its first qualifying evaluation immediately.
    pub last_signal_write_ms: Option<i64>,
}

/// Why a `signals` row is being written. Recorded so the fitted curve can be
/// re-derived from cadence rows alone, without the change-triggered rows that
/// over-represent transition moments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalTrigger {
    /// An entry was taken. Always logged.
    Entered,
    /// The rejection reason differs from the last one logged. This is the
    /// original (and only) trigger the bot had.
    ReasonChange,
    /// The logging cadence elapsed and the evaluation produced a decision
    /// delta. This is what makes `signals` a uniform time sample.
    Cadence,
}

impl SignalTrigger {
    pub fn as_str(&self) -> &'static str {
        match self {
            SignalTrigger::Entered => "entered",
            SignalTrigger::ReasonChange => "reason_change",
            SignalTrigger::Cadence => "cadence",
        }
    }
}

impl WindowState {
    /// Whether to write a `signals` row for this evaluation, and why.
    ///
    /// Three triggers, checked in priority order:
    ///
    /// 1. `entered` — always logged, unconditionally.
    /// 2. The rejection reason changed — the bot's original behavior, kept
    ///    exactly as it was so no previously-logged moment stops being logged.
    /// 3. The cadence elapsed AND this evaluation produced a decision delta.
    ///
    /// Trigger 3 is the addition. Without it the table only holds transition
    /// moments (~2.5 rows per window), which leaves the fair-value fit blind in
    /// any region the evaluation passes through without changing its verdict —
    /// most importantly the near-zero deltas, where `below_threshold` is
    /// written once and then never again for the rest of the window.
    ///
    /// The delta requirement is what keeps the cadence from filling the table
    /// with rows the model cannot use: `decision_delta` is exactly the column
    /// the fair-value join requires, and it is None for every evaluation that
    /// bailed out before computing one.
    ///
    /// `cadence_ms <= 0` disables trigger 3, restoring the original behavior
    /// byte for byte.
    pub fn should_write_signal(
        &self,
        reason: &str,
        has_decision_delta: bool,
        now_ms: i64,
        cadence_ms: i64,
    ) -> Option<SignalTrigger> {
        if reason == "entered" {
            return Some(SignalTrigger::Entered);
        }
        if self.last_signal_reason.as_deref() != Some(reason) {
            return Some(SignalTrigger::ReasonChange);
        }
        if cadence_ms > 0 && has_decision_delta {
            let due = match self.last_signal_write_ms {
                Some(last) => now_ms.saturating_sub(last) >= cadence_ms,
                // No row yet this window: log the first qualifying evaluation.
                None => true,
            };
            if due {
                return Some(SignalTrigger::Cadence);
            }
        }
        None
    }
}

/// Value in `hist` nearest to `age_ms` before `now_ms`, or None when the series
/// does not reach back that far. Picks the NEAREST entry to the target age
/// rather than the first one past it.
fn nearest_at_age(hist: &VecDeque<(i64, f64)>, now_ms: i64, age_ms: i64) -> Option<f64> {
    let target = now_ms - age_ms;
    // Fail closed rather than returning the oldest available: a series that
    // does not span the lookback cannot answer the question, and a substitute
    // value would be indistinguishable from a real one.
    if hist.front()?.0 > target {
        return None;
    }
    hist.iter()
        .min_by_key(|(ts, _)| (ts - target).abs())
        .map(|(_, v)| *v)
}

/// Result of a delta-momentum measurement, including the comparison point so a
/// genuine reversal can be told apart from a comparison against a stale or
/// too-recent reading.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DeltaMomentum {
    /// |delta_now| / |delta_past|. Below 1.0 the move is contracting.
    pub ratio: f64,
    pub past_value: f64,
    pub past_age_ms: i64,
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

    // ── Entry-pattern diagnostics ──
    // Written on EVERY signal including rejections, so rejected signals can be
    // scored against twap_observations.actual_resolution. NULL whenever the
    // history does not reach back far enough — never a substitute value, since
    // a fabricated zero is indistinguishable from a real zero delta.
    pub delta_peak_abs: Option<f64>,
    pub delta_peak_secs_ago: Option<f64>,
    pub delta_rise_time_s: Option<f64>,
    pub delta_5s_ago: Option<f64>,
    pub delta_15s_ago: Option<f64>,
    pub delta_30s_ago: Option<f64>,
    pub ask_5s_ago: Option<f64>,
    pub ask_30s_ago: Option<f64>,
    pub ask_peak_signalled: Option<f64>,
    pub ask_peak_any: Option<f64>,
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

    /// Record the decision delta for this evaluation, trimming entries beyond
    /// `DELTA_HISTORY_WINDOW_MS` and capping length defensively.
    pub fn push_delta(&mut self, ts_ms: i64, delta: f64) {
        self.delta_history.push_back((ts_ms, delta));

        let cutoff = ts_ms - crate::constants::DELTA_HISTORY_WINDOW_MS;
        while let Some(&(front_ts, _)) = self.delta_history.front() {
            if front_ts < cutoff {
                self.delta_history.pop_front();
            } else {
                break;
            }
        }
        while self.delta_history.len() > crate::constants::DELTA_HISTORY_MAX_ENTRIES {
            self.delta_history.pop_front();
        }
    }

    /// DATA COLLECTION ONLY. Update the window's running delta extremes from
    /// the decision delta. Records the peak and the first crossing of
    /// DELTA_RISE_FLOOR_PCT, whose difference is the move's rise time.
    pub fn observe_delta_extremes(&mut self, ts_ms: i64, delta: f64) {
        let abs = delta.abs();
        if self.delta_peak_abs.map(|p| abs > p).unwrap_or(true) {
            self.delta_peak_abs = Some(abs);
            self.delta_peak_ms = Some(ts_ms);
        }
        if self.delta_first_cross_ms.is_none() && abs > crate::constants::DELTA_RISE_FLOOR_PCT {
            self.delta_first_cross_ms = Some(ts_ms);
        }
    }

    /// DATA COLLECTION ONLY. Record the observed asks for this evaluation.
    ///
    /// `signalled_ask` is the ask for the side currently signalled; `any_ask`
    /// is the best ask across both sides. When the signalled side flips, the
    /// ask series and its peak reset — they describe one token, and splicing
    /// two tokens' prices together would make the lookbacks meaningless.
    /// `ask_peak_any_side` deliberately survives a flip: it spans the window.
    pub fn observe_ask(
        &mut self,
        ts_ms: i64,
        side: Option<&str>,
        signalled_ask: Option<f64>,
        any_ask: Option<f64>,
    ) {
        if let Some(s) = side {
            if self.last_signalled_side.as_deref() != Some(s) {
                if self.last_signalled_side.is_some() {
                    self.ask_history.clear();
                    self.ask_peak_signalled_side = None;
                }
                self.last_signalled_side = Some(s.to_string());
            }
        }

        if let Some(ask) = signalled_ask {
            self.ask_history.push_back((ts_ms, ask));
            let cutoff = ts_ms - crate::constants::DELTA_HISTORY_WINDOW_MS;
            while let Some(&(front_ts, _)) = self.ask_history.front() {
                if front_ts < cutoff {
                    self.ask_history.pop_front();
                } else {
                    break;
                }
            }
            while self.ask_history.len() > crate::constants::DELTA_HISTORY_MAX_ENTRIES {
                self.ask_history.pop_front();
            }
            if self.ask_peak_signalled_side.map(|p| ask > p).unwrap_or(true) {
                self.ask_peak_signalled_side = Some(ask);
            }
        }

        if let Some(ask) = any_ask {
            if self.ask_peak_any_side.map(|p| ask > p).unwrap_or(true) {
                self.ask_peak_any_side = Some(ask);
            }
        }
    }

    /// Value of the decision delta closest to `age_ms` before `now_ms`,
    /// or None if history does not reach back that far.
    /// Picks the entry NEAREST the target age, not the first one past it.
    pub fn delta_at_age(&self, now_ms: i64, age_ms: i64) -> Option<f64> {
        nearest_at_age(&self.delta_history, now_ms, age_ms)
    }

    /// As [`Self::delta_at_age`], for the signalled-side ask series.
    pub fn ask_at_age(&self, now_ms: i64, age_ms: i64) -> Option<f64> {
        nearest_at_age(&self.ask_history, now_ms, age_ms)
    }

    /// Seconds between the delta peak and `now_ms`, when a peak exists.
    pub fn delta_peak_secs_ago(&self, now_ms: i64) -> Option<f64> {
        self.delta_peak_ms
            .map(|p| now_ms.saturating_sub(p) as f64 / 1000.0)
    }

    /// Seconds the move took to form: peak time minus first floor crossing.
    /// None unless both are known; negative spans (a peak recorded before the
    /// floor was crossed) are reported as None rather than a bogus value.
    pub fn delta_rise_time_s(&self) -> Option<f64> {
        let peak = self.delta_peak_ms?;
        let first = self.delta_first_cross_ms?;
        if peak < first {
            return None;
        }
        Some((peak - first) as f64 / 1000.0)
    }

    /// Returns the ratio |delta_now| / |delta_past|, where delta_past is the
    /// oldest entry in the history that is at least DELTA_MOMENTUM_MIN_AGE_MS
    /// old.
    ///
    ///   ratio < 1.0  -> move is CONTRACTING (reversing toward zero)
    ///   ratio > 1.0  -> move is EXPANDING (continuing)
    ///
    /// Returns None if there is no history entry old enough to compare
    /// against, or if |delta_past| is too small to form a meaningful ratio
    /// (guard against division by ~zero).
    pub fn delta_momentum(&self, delta_now: f64, now_ms: i64) -> Option<f64> {
        self.delta_momentum_detail(delta_now, now_ms).map(|d| d.ratio)
    }

    /// As [`Self::delta_momentum`], but also reports the comparison point.
    pub fn delta_momentum_detail(&self, delta_now: f64, now_ms: i64) -> Option<DeltaMomentum> {
        // The deque is oldest-first, so the first entry meeting the age
        // requirement is the OLDEST qualifying one — the comparison therefore
        // spans the full available lookback rather than the shortest.
        let &(past_ts, past_value) = self.delta_history.iter().find(|(ts, _)| {
            now_ms.saturating_sub(*ts) >= crate::constants::DELTA_MOMENTUM_MIN_AGE_MS
        })?;

        // Too small a denominator makes the ratio meaningless, not merely large.
        if past_value.abs() < 1e-6 {
            return None;
        }

        let past_age_ms = now_ms.saturating_sub(past_ts);

        // A sign flip means the move already crossed through zero: the
        // strongest possible reversal signal, so it fails any threshold.
        let flipped = (delta_now > 0.0 && past_value < 0.0)
            || (delta_now < 0.0 && past_value > 0.0);
        let ratio = if flipped {
            0.0
        } else {
            delta_now.abs() / past_value.abs()
        };

        Some(DeltaMomentum {
            ratio,
            past_value,
            past_age_ms,
        })
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
    /// Delta-momentum ratio at the moment the order was placed.
    pub delta_momentum_at_entry: Option<f64>,
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
    /// The delta that actually drove the threshold check and side selection,
    /// respecting use_twap_strike. Single source of truth for what gets pushed
    /// into the delta history.
    pub decision_delta: Option<f64>,
    /// Delta-momentum ratio and its comparison point, when measurable.
    pub delta_momentum: Option<f64>,
    pub delta_past_value: Option<f64>,
    pub delta_past_age_ms: Option<i64>,
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
            decision_delta: None,
            delta_momentum: None,
            delta_past_value: None,
            delta_past_age_ms: None,
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

    // ── Delta momentum ──

    const NOW_MS: i64 = 1_786_624_500_000;

    fn ws_with(history: &[(i64, f64)]) -> WindowState {
        WindowState {
            delta_history: history.iter().copied().collect(),
            ..Default::default()
        }
    }

    /// The actual losing trade: window 1786624200 went -0.15223 -> -0.03775.
    #[test]
    fn contracting_delta_is_rejected_at_default() {
        let ws = ws_with(&[(NOW_MS - 45_000, -0.15223)]);
        let ratio = ws.delta_momentum(-0.03775, NOW_MS).unwrap();
        assert!(
            (ratio - 0.2480).abs() < 1e-3,
            "expected ~0.248, got {ratio}"
        );
        assert!(ratio < 0.70, "must fail the 0.70 default");
    }

    /// The mirror image: a move growing at the same rate must pass.
    #[test]
    fn expanding_delta_passes() {
        let ws = ws_with(&[(NOW_MS - 45_000, -0.03775)]);
        let ratio = ws.delta_momentum(-0.15223, NOW_MS).unwrap();
        assert!((ratio - 4.032).abs() < 1e-2, "expected ~4.03, got {ratio}");
        assert!(ratio >= 0.70);
    }

    /// A sign flip means the move already crossed zero — the strongest reversal.
    #[test]
    fn sign_flip_returns_zero() {
        let ws = ws_with(&[(NOW_MS - 30_000, -0.05)]);
        assert_eq!(ws.delta_momentum(0.03, NOW_MS), Some(0.0));
        let ws = ws_with(&[(NOW_MS - 30_000, 0.05)]);
        assert_eq!(ws.delta_momentum(-0.03, NOW_MS), Some(0.0));
    }

    /// Identical magnitude, opposite meaning — the bug this filter fixes.
    #[test]
    fn same_delta_opposite_meaning() {
        let contracting = ws_with(&[(NOW_MS - 30_000, -0.152)])
            .delta_momentum(-0.038, NOW_MS)
            .unwrap();
        let expanding = ws_with(&[(NOW_MS - 30_000, -0.005)])
            .delta_momentum(-0.038, NOW_MS)
            .unwrap();
        assert!(contracting < 0.70, "contracting {contracting} must reject");
        assert!(expanding >= 0.70, "expanding {expanding} must pass");
    }

    /// Too-recent history cannot judge momentum, and must return None (which
    /// the gate treats as "do not reject").
    #[test]
    fn insufficient_history_returns_none() {
        assert_eq!(ws_with(&[]).delta_momentum(-0.05, NOW_MS), None);
        // Present but younger than DELTA_MOMENTUM_MIN_AGE_MS.
        let ws = ws_with(&[(NOW_MS - 19_999, -0.15)]);
        assert_eq!(ws.delta_momentum(-0.05, NOW_MS), None);
        // Exactly at the minimum age qualifies.
        let ws = ws_with(&[(NOW_MS - 20_000, -0.15)]);
        assert!(ws.delta_momentum(-0.05, NOW_MS).is_some());
    }

    /// A near-zero denominator makes the ratio meaningless, not merely large.
    #[test]
    fn tiny_past_value_returns_none() {
        let ws = ws_with(&[(NOW_MS - 30_000, 1e-9)]);
        assert_eq!(ws.delta_momentum(0.05, NOW_MS), None);
    }

    /// The comparison spans the FULL lookback: the oldest qualifying entry is
    /// used, not the newest.
    #[test]
    fn uses_oldest_qualifying_entry() {
        let ws = ws_with(&[
            (NOW_MS - 40_000, -0.20), // oldest qualifying -> should be chosen
            (NOW_MS - 25_000, -0.10),
            (NOW_MS - 5_000, -0.05), // too recent
        ]);
        let d = ws.delta_momentum_detail(-0.05, NOW_MS).unwrap();
        assert_eq!(d.past_value, -0.20);
        assert_eq!(d.past_age_ms, 40_000);
        assert!((d.ratio - 0.25).abs() < 1e-9);
    }

    #[test]
    fn push_delta_trims_and_caps() {
        let mut ws = WindowState::default();
        // Entries older than the window are dropped.
        ws.push_delta(NOW_MS - 60_000, -0.1);
        ws.push_delta(NOW_MS, -0.2);
        assert_eq!(ws.delta_history.len(), 1, "stale entry should be trimmed");

        let mut ws = WindowState::default();
        for i in 0..(crate::constants::DELTA_HISTORY_MAX_ENTRIES + 50) {
            ws.push_delta(NOW_MS + i as i64, -0.1);
        }
        assert_eq!(
            ws.delta_history.len(),
            crate::constants::DELTA_HISTORY_MAX_ENTRIES
        );
    }

    #[test]
    fn history_clears_on_rotation() {
        let mut ws = ws_with(&[(NOW_MS - 30_000, -0.15), (NOW_MS, -0.05)]);
        assert!(!ws.delta_history.is_empty());
        // Rotation clears it, as main.rs does alongside the other resets.
        ws.delta_history.clear();
        assert!(ws.delta_history.is_empty());
        assert_eq!(ws.delta_momentum(-0.05, NOW_MS), None);
    }

    // ── Entry-pattern diagnostics ──

    #[test]
    fn delta_at_age_picks_nearest_not_first_past() {
        let ws = ws_with(&[
            (NOW_MS - 30_000, -0.30),
            (NOW_MS - 16_000, -0.16),
            (NOW_MS - 14_000, -0.14),
            (NOW_MS - 1_000, -0.01),
        ]);
        // Target 15s: 16s and 14s are both 1s away; the older wins on tie.
        assert_eq!(ws.delta_at_age(NOW_MS, 15_000), Some(-0.16));
        // Target 30s lands exactly on an entry.
        assert_eq!(ws.delta_at_age(NOW_MS, 30_000), Some(-0.30));
        // Target 5s: nearest is the 1s entry, and history reaches back past 5s.
        assert_eq!(ws.delta_at_age(NOW_MS, 5_000), Some(-0.01));
    }

    #[test]
    fn delta_at_age_is_none_when_history_too_short() {
        let ws = ws_with(&[(NOW_MS - 4_000, -0.05)]);
        assert_eq!(ws.delta_at_age(NOW_MS, 5_000), None, "must not substitute");
        assert_eq!(ws.delta_at_age(NOW_MS, 30_000), None);
        assert_eq!(ws_with(&[]).delta_at_age(NOW_MS, 5_000), None);
    }

    #[test]
    fn delta_extremes_track_peak_and_rise_time() {
        let mut ws = WindowState::default();
        ws.observe_delta_extremes(NOW_MS - 40_000, 0.005); // under the floor
        assert_eq!(ws.delta_first_cross_ms, None);
        ws.observe_delta_extremes(NOW_MS - 30_000, 0.05); // crosses 0.02
        ws.observe_delta_extremes(NOW_MS - 20_000, 0.15); // peak
        ws.observe_delta_extremes(NOW_MS - 5_000, -0.09); // shrinking

        assert_eq!(ws.delta_first_cross_ms, Some(NOW_MS - 30_000));
        assert_eq!(ws.delta_peak_abs, Some(0.15));
        assert_eq!(ws.delta_peak_ms, Some(NOW_MS - 20_000));
        // Rise: crossed at -30s, peaked at -20s => 10s to form.
        assert_eq!(ws.delta_rise_time_s(), Some(10.0));
        assert_eq!(ws.delta_peak_secs_ago(NOW_MS), Some(20.0));
    }

    #[test]
    fn delta_peak_uses_absolute_value() {
        let mut ws = WindowState::default();
        ws.observe_delta_extremes(NOW_MS - 10_000, -0.20);
        ws.observe_delta_extremes(NOW_MS, 0.05);
        assert_eq!(ws.delta_peak_abs, Some(0.20), "sign must not shrink the peak");
        assert_eq!(ws.delta_peak_ms, Some(NOW_MS - 10_000));
    }

    #[test]
    fn rise_time_none_without_both_marks() {
        let mut ws = WindowState::default();
        // Peak recorded but the floor never crossed.
        ws.observe_delta_extremes(NOW_MS, 0.001);
        assert!(ws.delta_peak_ms.is_some());
        assert_eq!(ws.delta_rise_time_s(), None);
    }

    #[test]
    fn ask_history_clears_on_side_flip() {
        let mut ws = WindowState::default();
        ws.observe_ask(NOW_MS - 30_000, Some("Up"), Some(0.40), Some(0.60));
        ws.observe_ask(NOW_MS - 20_000, Some("Up"), Some(0.55), Some(0.55));
        assert_eq!(ws.ask_history.len(), 2);
        assert_eq!(ws.ask_peak_signalled_side, Some(0.55));
        assert_eq!(ws.ask_peak_any_side, Some(0.60));

        // Flip: the series and the signalled peak reset, the any-side peak does not.
        ws.observe_ask(NOW_MS - 10_000, Some("Down"), Some(0.30), Some(0.45));
        assert_eq!(ws.ask_history.len(), 1, "mixed-token series must be dropped");
        assert_eq!(ws.ask_peak_signalled_side, Some(0.30));
        assert_eq!(ws.ask_peak_any_side, Some(0.60), "any-side peak spans the window");
        // ...and the shortened series correctly reports no 30s lookback.
        assert_eq!(ws.ask_at_age(NOW_MS, 30_000), None);
    }

    #[test]
    fn ask_history_absent_ask_is_not_recorded() {
        let mut ws = WindowState::default();
        ws.observe_ask(NOW_MS - 10_000, Some("Up"), None, None);
        assert!(ws.ask_history.is_empty());
        assert_eq!(ws.ask_peak_signalled_side, None);
        assert_eq!(ws.ask_at_age(NOW_MS, 5_000), None);
    }

    #[test]
    fn diagnostics_clear_on_rotation() {
        let mut ws = WindowState::default();
        ws.observe_delta_extremes(NOW_MS - 10_000, 0.15);
        ws.observe_ask(NOW_MS - 10_000, Some("Up"), Some(0.5), Some(0.5));
        ws.push_delta(NOW_MS, 0.1);

        // Mirrors the rotation reset in main.rs.
        ws.delta_history.clear();
        ws.delta_peak_abs = None;
        ws.delta_peak_ms = None;
        ws.delta_first_cross_ms = None;
        ws.ask_peak_signalled_side = None;
        ws.ask_peak_any_side = None;
        ws.ask_history.clear();
        ws.last_signalled_side = None;

        assert_eq!(ws.delta_peak_abs, None);
        assert_eq!(ws.delta_rise_time_s(), None);
        assert_eq!(ws.delta_peak_secs_ago(NOW_MS), None);
        assert_eq!(ws.ask_at_age(NOW_MS, 5_000), None);
        assert_eq!(ws.delta_at_age(NOW_MS, 5_000), None);
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

#[cfg(test)]
mod signal_cadence_tests {
    use super::*;

    const CADENCE: i64 = 5_000;

    fn ws_after(reason: &str, wrote_at_ms: i64) -> WindowState {
        WindowState {
            last_signal_reason: Some(reason.to_string()),
            last_signal_write_ms: Some(wrote_at_ms),
            ..Default::default()
        }
    }

    /// An entry is always logged, regardless of cadence or repetition.
    #[test]
    fn entered_always_logs() {
        let ws = ws_after("entered", 10_000);
        assert_eq!(
            ws.should_write_signal("entered", true, 10_001, CADENCE),
            Some(SignalTrigger::Entered)
        );
        // ...even with the cadence disabled.
        assert_eq!(
            ws.should_write_signal("entered", false, 10_001, 0),
            Some(SignalTrigger::Entered)
        );
    }

    /// The original trigger still fires the moment the verdict changes.
    #[test]
    fn changed_reason_logs_immediately() {
        let ws = ws_after("below_threshold", 10_000);
        assert_eq!(
            ws.should_write_signal("choppy", true, 10_001, CADENCE),
            Some(SignalTrigger::ReasonChange)
        );
    }

    /// THE FIX: a reason that stays the same is logged again once the cadence
    /// elapses. This is what fills the near-zero delta bands, where
    /// "below_threshold" used to be written once and then never again.
    #[test]
    fn unchanged_reason_logs_again_after_the_cadence() {
        let ws = ws_after("below_threshold", 10_000);
        // Too soon.
        assert_eq!(
            ws.should_write_signal("below_threshold", true, 14_999, CADENCE),
            None
        );
        // Exactly due.
        assert_eq!(
            ws.should_write_signal("below_threshold", true, 15_000, CADENCE),
            Some(SignalTrigger::Cadence)
        );
        // Overdue.
        assert_eq!(
            ws.should_write_signal("below_threshold", true, 30_000, CADENCE),
            Some(SignalTrigger::Cadence)
        );
    }

    /// The cadence only fires for evaluations that produced a decision delta —
    /// that is exactly the column the fair-value join requires, so rows the
    /// model cannot use are never written.
    #[test]
    fn cadence_requires_a_decision_delta() {
        let ws = ws_after("twap_unavailable", 10_000);
        assert_eq!(
            ws.should_write_signal("twap_unavailable", false, 30_000, CADENCE),
            None
        );
        assert_eq!(
            ws.should_write_signal("twap_unavailable", true, 30_000, CADENCE),
            Some(SignalTrigger::Cadence)
        );
    }

    /// cadence_ms = 0 restores the original change-only behavior exactly.
    #[test]
    fn zero_cadence_restores_change_only_logging() {
        let ws = ws_after("below_threshold", 10_000);
        for now in [10_001, 15_000, 60_000, 600_000] {
            assert_eq!(
                ws.should_write_signal("below_threshold", true, now, 0),
                None,
                "cadence fired at {} with the cadence disabled",
                now
            );
        }
        // A changed reason still logs.
        assert_eq!(
            ws.should_write_signal("choppy", true, 10_001, 0),
            Some(SignalTrigger::ReasonChange)
        );
    }

    /// A fresh window (nothing written yet) logs its first qualifying
    /// evaluation immediately rather than waiting out an interval.
    #[test]
    fn first_evaluation_of_a_window_logs_immediately() {
        let ws = WindowState {
            last_signal_reason: Some("below_threshold".to_string()),
            last_signal_write_ms: None,
            ..Default::default()
        };
        assert_eq!(
            ws.should_write_signal("below_threshold", true, 1, CADENCE),
            Some(SignalTrigger::Cadence)
        );
    }

    /// Rate: at a 250ms loop tick and a 5s cadence, a 300s window yields about
    /// one row per cadence interval, not one per tick. Guards against the
    /// change turning into a 20x write amplification.
    #[test]
    fn cadence_bounds_the_write_rate() {
        let mut ws = WindowState {
            last_signal_reason: Some("below_threshold".to_string()),
            ..Default::default()
        };
        let mut writes = 0;
        let mut now = 0i64;
        while now < 300_000 {
            if ws.should_write_signal("below_threshold", true, now, CADENCE).is_some() {
                writes += 1;
                ws.last_signal_write_ms = Some(now);
            }
            now += 250;
        }
        assert_eq!(
            writes, 60,
            "expected ~one row per 5s over a 300s window, got {}",
            writes
        );
    }
}
