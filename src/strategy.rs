use tracing::{debug, info};

use crate::config::RuntimeConfig;
use crate::constants::*;
use crate::types::{
    BinanceBtcPrice, BtcPriceState, EntrySignal, EvaluationResult, MarketState, MarketWindow,
    WindowState,
};

/// Evaluate Strategy A entry conditions.
/// Returns EvaluationResult with signal on success, or rejection diagnostics.
// Each parameter is a distinct piece of shared state read by the evaluation;
// bundling them would obscure which state the decision actually depends on.
#[allow(clippy::too_many_arguments)]
pub fn evaluate_entry(
    config: &RuntimeConfig,
    btc: &BtcPriceState,
    binance: &BinanceBtcPrice,
    market: &MarketState,
    window: &MarketWindow,
    secs_left: i64,
    twap_strike: Option<&str>,
    binance_twap_strike: Option<f64>,
    window_state: &WindowState,
) -> EvaluationResult {
    let mut r = EvaluationResult::rejected("entered");

    if market.resolved {
        r.rejection_reason = "market_resolved";
        return r;
    }

    let (bn_current, bn_open) = match (binance.current_price, binance.window_open_price) {
        (Some(c), Some(o)) => (c, o),
        _ => {
            r.rejection_reason = "no_binance_price";
            return r;
        }
    };

    let delta_pct = ((bn_current - bn_open) / bn_open) * 100.0;
    r.btc_delta_pct = Some(delta_pct);

    // SHADOW ONLY — computed here so it is recorded alongside the deltas that
    // do drive decisions, but deliberately never read below this line. Nothing
    // in the gates, threshold, side selection, or ordering consults it; the
    // shadow test asserts the decision is identical with and without it.
    r.binance_twap_delta_pct = binance_twap_strike.and_then(|strike| {
        let current = binance.twap(BINANCE_TWAP_WINDOW_MS)?;
        crate::types::binance_twap_delta_pct(strike, current)
    });

    // TWAP delta vs the window's strike (TWAP at window open). Computed in both
    // modes: under the default it is recorded for comparison and drives nothing.
    // Both sides must be fresh — fresh_twap_30 returns None on a stale reading,
    // so a silent feed can never produce a delta against a backfilled price.
    let now_ms = chrono::Utc::now().timestamp_millis();
    r.twap_delta_pct = twap_strike.and_then(|strike| {
        let (current, _) = btc.fresh_twap_30(now_ms, TWAP_MAX_AGE_MS)?;
        crate::types::twap_delta_pct(strike, &current)
    });

    // Which delta drives the threshold and side selection. With
    // use_twap_strike == false this is the spot delta, making every downstream
    // comparison identical to the previous behavior.
    let decision_delta = if config.use_twap_strike {
        match r.twap_delta_pct {
            Some(d) => d,
            None => {
                // Fail closed. Falling back to the spot delta here would make
                // the two modes indistinguishable in the data and hide feed
                // outages behind apparently-normal trading.
                r.rejection_reason = "twap_unavailable";
                return r;
            }
        }
    } else {
        delta_pct
    };

    r.side = Some(if decision_delta > 0.0 { "Up" } else { "Down" }.into());
    // Single source of truth for what the delta history records, so momentum
    // is always measured on the series that actually drives entries.
    r.decision_delta = Some(decision_delta);

    if decision_delta.abs() < config.btc_threshold_pct {
        r.rejection_reason = "below_threshold";
        return r;
    }

    // Record the momentum measurement regardless of outcome, so rejected and
    // accepted signals are equally analysable.
    if let Some(d) = window_state.delta_momentum_detail(decision_delta, now_ms) {
        r.delta_momentum = Some(d.ratio);
        r.delta_past_value = Some(d.past_value);
        r.delta_past_age_ms = Some(d.past_age_ms);
    }

    // A delta shrinking toward zero means the move is reversing, even if its
    // magnitude still clears the threshold. Entering there bets against a
    // trend that is visibly in progress.
    match window_state.delta_momentum(decision_delta, now_ms) {
        Some(ratio) if ratio < config.min_delta_momentum => {
            debug!(
                "Delta momentum {:.2} below {:.2} — move is reversing",
                ratio, config.min_delta_momentum
            );
            r.rejection_reason = "delta_reversing";
            return r;
        }
        Some(ratio) => {
            debug!("Delta momentum {:.2} OK", ratio);
        }
        None => {
            // Not enough history yet (early in the window). Do NOT reject —
            // this differs deliberately from the trend/depth gates, because a
            // missing history is a normal early-window state, not a data
            // failure. The threshold check already guards magnitude.
        }
    }

    r.trend_strength = binance.trend_strength();
    match r.trend_strength {
        Some(strength) if strength < config.min_trend_strength => {
            debug!(
                "Trend strength {:.2} below {:.2}, market is choppy",
                strength, config.min_trend_strength
            );
            r.rejection_reason = "choppy";
            return r;
        }
        Some(strength) => {
            debug!("Trend strength {:.2} OK", strength);
        }
        None => {
            info!("Trend strength not yet available (< {} samples), rejecting", MIN_TREND_SAMPLES);
            r.rejection_reason = "trend_unavailable";
            return r;
        }
    }

    // The RTDS mismatch gate cross-checks Binance spot against RTDS spot.
    // In TWAP mode the decision comes from the Chainlink TWAP delta, so this
    // gate validates feeds that are not driving the trade. RTDS spot and the
    // TWAP feed are both Chainlink-sourced, making the check near-redundant.
    if !config.use_twap_strike {
        let now_ms = chrono::Utc::now().timestamp_millis() as u64;
        let rtds_stale =
            btc.last_update_ms == 0 || now_ms.saturating_sub(btc.last_update_ms) > RTDS_STALE_MS;

        if !rtds_stale {
            match (btc.current_price, btc.window_open_price) {
                (Some(rtds_current), Some(rtds_open)) => {
                    let rtds_delta = rtds_current - rtds_open;
                    let same_direction = (delta_pct > 0.0 && rtds_delta > 0.0)
                        || (delta_pct < 0.0 && rtds_delta < 0.0);
                    if !same_direction {
                        info!(
                            "RTDS direction mismatch: binance Δ={:+.4}% rtds Δ=${:+.2}",
                            delta_pct, rtds_delta
                        );
                        r.rejection_reason = "rtds_mismatch";
                        return r;
                    }
                }
                _ => {
                    info!("RTDS prices not yet available, using Binance alone");
                }
            }
        } else {
            info!("RTDS stale (>{}ms), using Binance alone", RTDS_STALE_MS);
        }
    }

    let (side, token_id, book) = if decision_delta > 0.0 {
        ("Up", &window.up_token_id, &market.up_book)
    } else {
        ("Down", &window.down_token_id, &market.down_book)
    };

    let total_trades = market.up_trade_count + market.down_trade_count;
    r.trade_count = Some(total_trades);

    let ask_price = match book.best_ask {
        Some(p) => p,
        None => {
            r.rejection_reason = "no_ask";
            return r;
        }
    };
    r.ask_price = Some(ask_price);
    r.bid_price = book.best_bid;
    r.ask_depth = book.ask_depth;

    if ask_price >= config.max_ask_price {
        debug!(
            "{} ask ${:.2} >= max ${:.2}",
            side, ask_price, config.max_ask_price
        );
        r.rejection_reason = "ask_too_high";
        return r;
    }

    if ask_price <= 0.00 {
        debug!("{} ask ${:.2} too low — likely stale data", side, ask_price);
        r.rejection_reason = "ask_too_low";
        return r;
    }

    let spread = match book.spread() {
        Some(s) => s,
        None => {
            r.rejection_reason = "no_ask";
            return r;
        }
    };
    r.spread = Some(spread);

    if spread >= config.max_spread {
        debug!(
            "{} spread ${:.2} >= max ${:.2}",
            side, spread, config.max_spread
        );
        r.rejection_reason = "spread_wide";
        return r;
    }

    // Fillable depth at our actual limit, not total book depth. Total depth
    // is misleading — size resting above our limit can't fill our order.
    // An empty/unpopulated ladder returns 0.0 and correctly fails this gate.
    let limit_price = ask_price + config.max_slippage;
    let fillable = book.ask_depth_up_to(limit_price);
    if fillable < MIN_ASK_DEPTH {
        debug!(
            "{} fillable depth {:.0} at limit ${:.2} < min {:.0}",
            side, fillable, limit_price, MIN_ASK_DEPTH
        );
        r.rejection_reason = "depth_low";
        return r;
    }
    debug!("{} fillable depth {:.0} at limit ${:.2} OK", side, fillable, limit_price);

    if secs_left < 30 {
        debug!("Only {}s left, too late to enter", secs_left);
        r.rejection_reason = "too_late";
        return r;
    }

    if secs_left > 120 {
        debug!("{}s left, too early to enter", secs_left);
        r.rejection_reason = "too_early";
        return r;
    }

    r.signal = Some(EntrySignal {
        side: side.to_string(),
        token_id: token_id.clone(),
        btc_delta_pct: delta_pct,
        ask_price,
        spread,
        secs_left,
    });
    r.rejection_reason = "entered";
    r
}

#[cfg(test)]
mod strike_mode_tests {
    use super::*;
    use crate::types::TokenBook;

    fn cfg(use_twap_strike: bool) -> RuntimeConfig {
        RuntimeConfig {
            poly_private_key: String::new(),
            poly_address: String::new(),
            poly_api_key: String::new(),
            poly_api_secret: String::new(),
            poly_api_passphrase: String::new(),
            telegram_bot_token: String::new(),
            telegram_chat_id: 0,
            btc_threshold_pct: 0.07,
            max_ask_price: 0.80,
            max_spread: 0.10,
            bet_shares: 5.0,
            max_slippage: 0.03,
            min_trend_strength: 0.41,
            // Disabled by default in fixtures so pre-existing tests exercise
            // the gates they were written for; momentum tests opt in.
            min_delta_momentum: 0.0,
            max_consecutive_losses: 3,
            daily_loss_limit_usdc: 20.0,
            poly_proxy_address: String::new(),
            db_path: ":memory:".into(),
            dry_run: true,
            use_twap_strike,
            chainlink_api_key: String::new(),
            chainlink_api_secret: String::new(),
            chainlink_stream_id: String::new(),
            use_chainlink_fallback: false,
        }
    }

    /// Spot delta of +0.20% (well past the 0.07% threshold).
    fn binance_up() -> BinanceBtcPrice {
        BinanceBtcPrice {
            current_price: Some(65_130.0),
            window_open_price: Some(65_000.0),
            ..Default::default()
        }
    }

    /// TWAP currently BELOW the strike, i.e. the opposite direction to spot.
    fn btc_twap_down() -> BtcPriceState {
        BtcPriceState {
            twap_30_value: Some("64800000000000000000000".into()), // 64800
            twap_30_observed_at_ms: Some(chrono::Utc::now().timestamp_millis()),
            ..Default::default()
        }
    }

    const STRIKE: &str = "65000000000000000000000"; // 65000

    fn market() -> (MarketState, MarketWindow) {
        (
            MarketState::default(),
            MarketWindow {
                up_token_id: "up".into(),
                down_token_id: "down".into(),
                neg_risk: false,
                tick_size: "0.01".into(),
            },
        )
    }

    fn eval(config_flag: bool, strike: Option<&str>) -> EvaluationResult {
        let (ms, win) = market();
        evaluate_entry(
            &cfg(config_flag),
            &btc_twap_down(),
            &binance_up(),
            &ms,
            &win,
            60,
            strike,
            None,
            &WindowState::default(),
        )
    }

    /// The core safety property: with the flag off, a TWAP strike pointing the
    /// other way must not influence the decision at all.
    #[test]
    fn shadow_mode_decision_is_unaffected_by_twap() {
        let with_strike = eval(false, Some(STRIKE));
        let without_strike = eval(false, None);

        // Spot said Up; TWAP said Down. Shadow mode must still say Up.
        assert_eq!(with_strike.side.as_deref(), Some("Up"));
        assert_eq!(with_strike.side, without_strike.side);
        assert_eq!(
            with_strike.rejection_reason,
            without_strike.rejection_reason,
            "presence of a TWAP strike changed the outcome in shadow mode"
        );
        // ...and the TWAP delta is still recorded for comparison.
        let d = with_strike.twap_delta_pct.expect("twap delta recorded");
        assert!(d < 0.0, "twap delta should be negative, got {}", d);
        assert_eq!(without_strike.twap_delta_pct, None);
    }

    /// With the flag on, the TWAP delta drives the side instead.
    #[test]
    fn twap_mode_side_follows_twap_delta() {
        let r = eval(true, Some(STRIKE));
        assert_eq!(r.side.as_deref(), Some("Down"));
    }

    /// Fail closed: no silent fallback to the spot delta.
    #[test]
    fn twap_mode_rejects_when_strike_missing() {
        assert_eq!(eval(true, None).rejection_reason, "twap_unavailable");
    }

    /// A stale TWAP reading must not produce a delta, even with a strike set.
    #[test]
    fn twap_mode_rejects_on_stale_reading() {
        let stale = BtcPriceState {
            twap_30_value: Some("64800000000000000000000".into()),
            twap_30_observed_at_ms: Some(
                chrono::Utc::now().timestamp_millis() - TWAP_MAX_AGE_MS - 1,
            ),
            ..Default::default()
        };
        let (ms, win) = market();
        let r = evaluate_entry(&cfg(true), &stale, &binance_up(), &ms, &win, 60, Some(STRIKE), None, &WindowState::default());
        assert_eq!(r.rejection_reason, "twap_unavailable");
        assert_eq!(r.twap_delta_pct, None);
    }

    /// RTDS spot state AGREEING with `binance_up_trending()` (both up), so
    /// downstream gates do not mask what a test is exercising.
    fn btc_rtds_agrees() -> BtcPriceState {
        BtcPriceState {
            current_price: Some(65_100.0),
            window_open_price: Some(65_000.0),
            last_update_ms: chrono::Utc::now().timestamp_millis() as u64,
            twap_30_value: Some("64800000000000000000000".into()),
            twap_30_observed_at_ms: Some(chrono::Utc::now().timestamp_millis()),
            ..Default::default()
        }
    }

    /// RTDS spot state that disagrees in direction with `binance_up()`:
    /// binance says +0.20%, RTDS spot says down. Fresh, so the gate applies.
    fn btc_rtds_disagrees() -> BtcPriceState {
        BtcPriceState {
            current_price: Some(64_900.0),
            window_open_price: Some(65_000.0),
            last_update_ms: chrono::Utc::now().timestamp_millis() as u64,
            twap_30_value: Some("64800000000000000000000".into()),
            twap_30_observed_at_ms: Some(chrono::Utc::now().timestamp_millis()),
            ..Default::default()
        }
    }

    /// Same +0.20% move as `binance_up()`, but with a buffer that clears the
    /// trend-strength gate so evaluation actually reaches the RTDS gate.
    /// Monotonic prices over 120s give net/gross = 1.0.
    fn binance_up_trending() -> BinanceBtcPrice {
        let now = chrono::Utc::now().timestamp_millis() as u64;
        let n = crate::constants::MIN_TREND_SAMPLES + 20;
        let buffer = (0..n)
            .map(|i| {
                let frac = i as f64 / (n - 1) as f64;
                (
                    now - 120_000 + (frac * 120_000.0) as u64,
                    65_000.0 + frac * 130.0,
                )
            })
            .collect();
        BinanceBtcPrice {
            current_price: Some(65_130.0),
            window_open_price: Some(65_000.0),
            last_update_ms: now,
            price_buffer: buffer,
        }
    }

    /// SHADOW INVARIANCE: supplying a Binance TWAP strike — including one whose
    /// delta points the OPPOSITE way to the deciding delta — must not change
    /// the entry decision in either mode. Mirrors
    /// `shadow_mode_decision_is_unaffected_by_twap`.
    #[test]
    fn binance_twap_never_affects_the_decision() {
        for flag in [false, true] {
            for secs in [60, 100] {
                let (ms, win) = market();
                let without = evaluate_entry(
                    &cfg(flag),
                    &btc_twap_down(),
                    &binance_up_trending(),
                    &ms,
                    &win,
                    secs,
                    Some(STRIKE),
                    None,
                    &WindowState::default(),
                );
                // A strike far ABOVE the current Binance TWAP, so the shadow
                // delta is strongly negative regardless of the real signal.
                let with = evaluate_entry(
                    &cfg(flag),
                    &btc_twap_down(),
                    &binance_up_trending(),
                    &ms,
                    &win,
                    secs,
                    Some(STRIKE),
                    Some(99_000.0),
                    &WindowState::default(),
                );

                assert_eq!(
                    without.rejection_reason, with.rejection_reason,
                    "binance twap changed the rejection reason (flag={flag}, secs={secs})"
                );
                assert_eq!(
                    without.side, with.side,
                    "binance twap changed side selection (flag={flag}, secs={secs})"
                );
                assert_eq!(without.btc_delta_pct, with.btc_delta_pct);
                assert_eq!(without.twap_delta_pct, with.twap_delta_pct);
                assert_eq!(
                    without.signal.is_some(),
                    with.signal.is_some(),
                    "binance twap changed whether a signal was produced"
                );

                // ...but it IS recorded when a strike is supplied.
                assert_eq!(without.binance_twap_delta_pct, None);
                let d = with.binance_twap_delta_pct.expect("shadow delta recorded");
                assert!(d < 0.0, "expected negative shadow delta, got {d}");
            }
        }
    }

    // ── Delta momentum gate ──

    fn cfg_momentum(use_twap_strike: bool, min_delta_momentum: f64) -> RuntimeConfig {
        RuntimeConfig {
            min_delta_momentum,
            ..cfg(use_twap_strike)
        }
    }

    /// History old enough to judge, holding `past` as the comparison point.
    fn ws_hist(past: f64) -> WindowState {
        let now = chrono::Utc::now().timestamp_millis();
        WindowState {
            delta_history: [(now - 30_000, past)].into_iter().collect(),
            ..Default::default()
        }
    }

    fn eval_momentum(cfg: &RuntimeConfig, ws: &WindowState) -> EvaluationResult {
        let (ms, win) = market();
        evaluate_entry(
            cfg,
            &btc_rtds_agrees(),
            &binance_up_trending(),
            &ms,
            &win,
            60,
            Some(STRIKE),
            None,
            ws,
        )
    }

    /// Reproduces the losing trade: spot delta +0.20% (passes threshold) but
    /// the history shows it contracted from a much larger move.
    #[test]
    fn contracting_delta_is_rejected_by_the_gate() {
        // binance_up_trending() gives +0.20%; a past of +0.80% -> ratio 0.25.
        let r = eval_momentum(&cfg_momentum(false, 0.70), &ws_hist(0.80));
        assert_eq!(r.rejection_reason, "delta_reversing");
        let ratio = r.delta_momentum.expect("ratio recorded");
        assert!((ratio - 0.25).abs() < 1e-6, "got {ratio}");
        assert_eq!(r.delta_past_value, Some(0.80));
        assert!(r.delta_past_age_ms.unwrap() >= DELTA_MOMENTUM_MIN_AGE_MS);
    }

    /// An expanding move passes the gate and continues to the later checks.
    #[test]
    fn expanding_delta_passes_the_gate() {
        let r = eval_momentum(&cfg_momentum(false, 0.70), &ws_hist(0.05));
        assert_ne!(r.rejection_reason, "delta_reversing");
        assert!(r.delta_momentum.unwrap() > 1.0);
    }

    /// MIN_DELTA_MOMENTUM=0.0 disables the filter: a ratio of 0.0 (sign flip)
    /// still passes, so behaviour is identical to before this change.
    #[test]
    fn zero_config_disables_the_filter() {
        // Sign flip: the most extreme reversal signal possible.
        let r = eval_momentum(&cfg_momentum(false, 0.0), &ws_hist(-0.80));
        assert_eq!(r.delta_momentum, Some(0.0), "ratio is 0.0 (sign flip)");
        assert_ne!(
            r.rejection_reason, "delta_reversing",
            "0.0 must disable the gate entirely"
        );
    }

    /// Missing history must NOT reject — a deliberate exception to the
    /// fail-closed pattern, since an empty history is normal early in a window.
    #[test]
    fn missing_history_does_not_reject() {
        let r = eval_momentum(&cfg_momentum(false, 0.70), &WindowState::default());
        assert_ne!(r.rejection_reason, "delta_reversing");
        assert_eq!(r.delta_momentum, None);
        assert_eq!(r.delta_past_value, None);
    }

    /// The gate must measure the delta that actually drives the decision.
    /// Same history, same inputs — only the flag differs, and the recorded
    /// decision delta follows it.
    #[test]
    fn momentum_uses_the_deciding_delta_for_the_mode() {
        // Spot delta is +0.20%; the TWAP delta here is negative (64800 vs the
        // 65000 strike), so the two modes must record different values.
        let spot = eval_momentum(&cfg_momentum(false, 0.0), &ws_hist(0.05));
        let twap = eval_momentum(&cfg_momentum(true, 0.0), &ws_hist(0.05));

        let spot_d = spot.decision_delta.expect("spot decision delta");
        let twap_d = twap.decision_delta.expect("twap decision delta");
        assert!(spot_d > 0.0, "spot mode should use the +0.20% spot delta");
        assert!(twap_d < 0.0, "twap mode should use the negative twap delta");
        assert_eq!(spot.btc_delta_pct, twap.btc_delta_pct, "spot delta recorded in both");
        assert_ne!(spot_d, twap_d, "the deciding delta must differ by mode");

        // And the ratio is computed from that mode's delta: twap mode flips
        // sign against the +0.05 history, so its ratio is 0.0.
        assert_eq!(twap.delta_momentum, Some(0.0));
        assert!(spot.delta_momentum.unwrap() > 1.0);
    }

    /// Guard: the fixture really does clear the trend gate, otherwise the two
    /// tests below would pass for the wrong reason.
    #[test]
    fn trending_fixture_clears_trend_gate() {
        let s = binance_up_trending().trend_strength();
        assert!(s.is_some() && s.unwrap() >= 0.41, "trend_strength = {:?}", s);
    }

    /// Item 3: the gate must still fire in non-TWAP mode.
    #[test]
    fn rtds_mismatch_gate_still_fires_in_spot_mode() {
        let (ms, win) = market();
        let r = evaluate_entry(
            &cfg(false),
            &btc_rtds_disagrees(),
            &binance_up_trending(),
            &ms,
            &win,
            60,
            Some(STRIKE),
            None,
            &WindowState::default(),
        );
        assert_eq!(r.rejection_reason, "rtds_mismatch");
    }

    /// Item 3: the same inputs must NOT hit the gate in TWAP mode, because the
    /// gate cross-checks feeds that are not driving the decision. Evaluation
    /// continues to the next unchanged gate instead.
    #[test]
    fn rtds_mismatch_gate_skipped_in_twap_mode() {
        let (ms, win) = market();
        let r = evaluate_entry(
            &cfg(true),
            &btc_rtds_disagrees(),
            &binance_up_trending(),
            &ms,
            &win,
            60,
            Some(STRIKE),
            None,
            &WindowState::default(),
        );
        assert_ne!(r.rejection_reason, "rtds_mismatch");
        // Empty book, so the next gate it reaches is the ask check.
        assert_eq!(r.rejection_reason, "no_ask");
    }

    /// A book with a fillable ask ladder, so evaluation can run past the
    /// book-dependent gates.
    fn book_with_asks() -> TokenBook {
        TokenBook {
            best_bid: Some(0.48),
            best_ask: Some(0.50),
            last_trade_price: Some(0.50),
            ask_depth: Some(500.0),
            bid_depth: Some(500.0),
            ask_levels: vec![(0.50, 300.0), (0.51, 300.0)],
        }
    }

    /// Regression: `resolved` is window-scoped shared state. If it survives a
    /// rotation, every later evaluation short-circuits on "market_resolved" and
    /// the bot can never enter again.
    #[test]
    fn rotation_clears_resolved_flag() {
        let mut ms = MarketState {
            resolved: true,
            winning_outcome: Some("Up".into()),
            up_trade_count: 7,
            down_trade_count: 9,
            up_book: book_with_asks(),
            down_book: book_with_asks(),
        };

        ms.reset_for_new_window();

        assert!(!ms.resolved, "resolved must be cleared on rotation");
        assert_eq!(ms.winning_outcome, None);
        assert_eq!(ms.up_trade_count, 0);
        assert_eq!(ms.down_trade_count, 0);
        // Books are window-scoped too: their prices belong to token ids that no
        // longer exist after rotation.
        assert_eq!(ms.up_book.best_ask, None);
        assert_eq!(ms.down_book.best_ask, None);
        assert!(ms.up_book.ask_levels.is_empty());
        assert_eq!(ms.up_book.ask_depth_up_to(0.99), 0.0);
    }

    /// ...and after that rotation, evaluation proceeds instead of bailing out.
    #[test]
    fn evaluate_entry_proceeds_after_rotation_clears_resolved() {
        let mut ms = MarketState {
            resolved: true,
            ..Default::default()
        };
        let (_, win) = market();

        // Before the reset, evaluation short-circuits.
        let before = evaluate_entry(
            &cfg(false),
            &btc_rtds_disagrees(),
            &binance_up_trending(),
            &ms,
            &win,
            60,
            Some(STRIKE),
            None,
            &WindowState::default(),
        );
        assert_eq!(before.rejection_reason, "market_resolved");

        // After it, the same otherwise-valid inputs get a real evaluation.
        ms.reset_for_new_window();
        ms.up_book = book_with_asks();
        ms.down_book = book_with_asks();

        let after = evaluate_entry(
            &cfg(false),
            &btc_rtds_disagrees(),
            &binance_up_trending(),
            &ms,
            &win,
            60,
            Some(STRIKE),
            None,
            &WindowState::default(),
        );
        assert_ne!(
            after.rejection_reason, "market_resolved",
            "evaluation still short-circuits after rotation"
        );
        // Reaches the real gates: these inputs disagree on direction, so the
        // (unchanged) RTDS gate is what stops it now.
        assert_eq!(after.rejection_reason, "rtds_mismatch");
    }

    /// Below-threshold behavior is still governed by the spot delta in shadow
    /// mode, even when the TWAP delta would clear the threshold.
    #[test]
    fn shadow_mode_threshold_uses_spot_delta() {
        let flat_spot = BinanceBtcPrice {
            current_price: Some(65_000.65), // +0.001%, under the 0.07% threshold
            window_open_price: Some(65_000.0),
            ..Default::default()
        };
        let (ms, win) = market();
        let r = evaluate_entry(
            &cfg(false),
            &btc_twap_down(), // TWAP delta ≈ -0.31%, would clear the threshold
            &flat_spot,
            &ms,
            &win,
            60,
            Some(STRIKE),
            None,
            &WindowState::default(),
        );
        assert_eq!(r.rejection_reason, "below_threshold");
        assert_eq!(r.side.as_deref(), Some("Up"));
        // Unused for the decision, but still recorded.
        assert!(r.twap_delta_pct.unwrap() < -0.3);
        let _ = TokenBook::default();
    }
}
