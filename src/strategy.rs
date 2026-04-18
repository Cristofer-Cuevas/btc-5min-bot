use tracing::{debug, info};

use crate::config::RuntimeConfig;
use crate::constants::*;
use crate::types::{
    BinanceBtcPrice, BtcPriceState, EntrySignal, EvaluationResult, MarketState, MarketWindow,
};

/// Evaluate Strategy A entry conditions.
/// Returns EvaluationResult with signal on success, or rejection diagnostics.
pub fn evaluate_entry(
    config: &RuntimeConfig,
    btc: &BtcPriceState,
    binance: &BinanceBtcPrice,
    market: &MarketState,
    window: &MarketWindow,
    secs_left: i64,
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
    r.side = Some(if delta_pct > 0.0 { "Up" } else { "Down" }.into());

    if delta_pct.abs() < config.btc_threshold_pct {
        r.rejection_reason = "below_threshold";
        return r;
    }

    r.trend_strength = binance.trend_strength();
    match r.trend_strength {
        Some(strength) if strength < MIN_TREND_STRENGTH => {
            debug!(
                "Trend strength {:.2} below {:.2}, market is choppy",
                strength, MIN_TREND_STRENGTH
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

    let (side, token_id, book) = if delta_pct > 0.0 {
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

    if let Some(depth) = book.ask_depth {
        if depth < MIN_ASK_DEPTH {
            debug!("{} ask depth {:.0} < min {:.0}", side, depth, MIN_ASK_DEPTH);
            r.rejection_reason = "depth_low";
            return r;
        }
    }

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
