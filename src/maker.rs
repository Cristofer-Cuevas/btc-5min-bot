//! PHASE 2 — SHADOW QUOTING. MEASUREMENT ONLY.
//!
//! Nothing in this module places, signs, cancels or otherwise touches an order.
//! It computes candidate resting quotes. A single contemporaneous book cannot
//! establish a passive fill: crossing that book is a taker order (or a rejected
//! post-only order). Historical `would_*_fill` rows used that invalid rule and
//! must not be used to infer maker profitability. New rows never claim fills.
//! A fill study needs order arrival/cancel latency, queue/depth, subsequent
//! aggressor trades, quantities and inventory, none of which this API receives.
//!
//! The taker path does not read anything here. `evaluate_entry` never sees
//! this module, and `strategy::shadow_isolation_tests` pins that.

use crate::fairvalue::{self, Side};

/// How the bot treats the maker machinery.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MakerMode {
    /// No fair-value model is loaded, no shadow rows are written. The bot
    /// behaves exactly as it did before this module existed.
    #[default]
    Off,
    /// Fair-value model required at startup; shadow rows written every tick.
    /// Still sends nothing to Polymarket.
    Shadow,
}

impl MakerMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            MakerMode::Off => "off",
            MakerMode::Shadow => "shadow",
        }
    }

    /// Parse the `MAKER_MODE` env value. `live` is deliberately rejected:
    /// Phase 3 does not exist yet, and silently downgrading it to shadow would
    /// let an operator believe they were quoting when they were not.
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "" | "off" | "false" | "0" => Ok(MakerMode::Off),
            "shadow" => Ok(MakerMode::Shadow),
            "live" => Err(
                "MAKER_MODE=live is not implemented — live quoting is Phase 3, gated on the \
                 adverse-selection numbers Phase 2 measures. Use 'shadow' or 'off'."
                    .into(),
            ),
            other => Err(format!(
                "MAKER_MODE must be one of off|shadow, got '{}'",
                other
            )),
        }
    }
}

/// Minimum gap between shadow rows for the same side, so the table stays
/// queryable at a 250ms tick.
pub const SHADOW_MIN_WRITE_INTERVAL_MS: i64 = 1000;

/// Quote bounds. Polymarket will not accept a resting order outside these, and
/// a fair value that implies one is not actionable anyway.
pub const MIN_QUOTE: f64 = 0.01;
pub const MAX_QUOTE: f64 = 0.99;

/// One tick's worth of simulated quoting for one side.
#[derive(Debug, Clone, PartialEq)]
pub struct ShadowQuote {
    pub side: Side,
    pub decision_delta: f64,
    pub secs_left: i64,
    pub fair_value: f64,
    pub our_bid: f64,
    pub our_ask: f64,
    pub market_bid: Option<f64>,
    pub market_ask: Option<f64>,
    /// Legacy schema field. Always false: same-tick books cannot prove fills.
    pub would_bid_fill: bool,
    /// Legacy schema field. Always false: same-tick books cannot prove fills.
    pub would_ask_fill: bool,
}

/// Parse a tick size string such as "0.01" from the market metadata. Returns
/// None for anything unsupported or unparseable — the caller must then skip
/// the tick rather than assume a default, since a wrong tick silently shifts
/// every quote.
pub fn parse_tick(tick_size: &str) -> Option<f64> {
    let t: f64 = tick_size.trim().parse().ok()?;
    if valid_tick(t) {
        Some(t)
    } else {
        None
    }
}

fn valid_tick(tick: f64) -> bool {
    [0.1, 0.01, 0.001, 0.0001].contains(&tick)
}

/// Round `price` DOWN to a multiple of `tick`.
fn floor_to_tick(price: f64, tick: f64) -> f64 {
    let n = (price / tick + 1e-9).floor();
    n * tick
}

/// Round `price` UP to a multiple of `tick`.
fn ceil_to_tick(price: f64, tick: f64) -> f64 {
    let n = (price / tick - 1e-9).ceil();
    n * tick
}

/// Snap a rounded price back onto the tick grid cleanly. Repeated float
/// multiplication leaves values like 0.30000000000000004, which then compare
/// badly against book prices parsed from decimal strings.
fn snap(price: f64, tick: f64) -> f64 {
    let decimals = decimals_for(tick);
    let f = 10f64.powi(decimals);
    (price * f).round() / f
}

/// Decimal places implied by a tick size, capped at the finest tick
/// Polymarket offers (0.0001).
fn decimals_for(tick: f64) -> i32 {
    let mut d = 0;
    let mut t = tick;
    while t < 1.0 && d < 4 {
        t *= 10.0;
        d += 1;
    }
    d
}

/// Build the quotes we would post for `side`, or None when the fair value is
/// unavailable (low-confidence cell, out-of-range secs_left, no model).
///
/// `half_spread` comes from config. We quote WIDE on purpose: order placement
/// was measured at 400-1400ms, so we cannot defend a tight quote and must be
/// paid for the staleness instead.
///
/// `inventory_skew` is 0.0 in Phase 2 — the parameter exists so Phase 3 can
/// supply a real skew without changing this signature or the recorded schema.
// Each argument is a distinct input to the quote; bundling them into a struct
// would add a type without removing a single decision the caller has to make.
#[allow(clippy::too_many_arguments)]
pub fn shadow_quote(
    side: Side,
    decision_delta: f64,
    secs_left: i64,
    half_spread: f64,
    inventory_skew: f64,
    tick: f64,
    market_bid: Option<f64>,
    market_ask: Option<f64>,
) -> Option<ShadowQuote> {
    let p = fairvalue::fair_value(side, decision_delta, secs_left)?;
    quote_from_fair_value(
        side,
        decision_delta,
        secs_left,
        p,
        half_spread,
        inventory_skew,
        tick,
        market_bid,
        market_ask,
    )
}

/// The pure half of [`shadow_quote`], with the fair value supplied directly so
/// it is testable without installing a process-wide model.
///
/// Returns None when no postable two-sided quote exists at this tick — see the
/// tradeable-band note below. A quote we could not actually place is not a
/// measurement, so recording one would only pollute the dataset.
#[allow(clippy::too_many_arguments)]
pub fn quote_from_fair_value(
    side: Side,
    decision_delta: f64,
    secs_left: i64,
    fair_value: f64,
    half_spread: f64,
    inventory_skew: f64,
    tick: f64,
    market_bid: Option<f64>,
    market_ask: Option<f64>,
) -> Option<ShadowQuote> {
    if !valid_tick(tick)
        || !decision_delta.is_finite()
        || !(0..=300).contains(&secs_left)
        || !fair_value.is_finite()
        || !(0.0..=1.0).contains(&fair_value)
        || !half_spread.is_finite()
        || half_spread < 0.0
        || !inventory_skew.is_finite()
        || market_bid
            .into_iter()
            .chain(market_ask)
            .any(|p| !p.is_finite() || !(0.0..=1.0).contains(&p))
        || matches!((market_bid, market_ask), (Some(b), Some(a)) if b >= a)
    {
        return None;
    }
    // The tradeable band, expressed ON THE TICK GRID. Clamping to the raw
    // 0.01/0.99 bounds would emit prices off the grid on a coarse tick (a 0.01
    // bid in a 0.1-tick market), which the exchange rejects — so the bounds are
    // snapped inward first.
    let band_lo = snap(ceil_to_tick(MIN_QUOTE, tick), tick);
    let band_hi = snap(floor_to_tick(MAX_QUOTE, tick), tick);
    if band_lo > band_hi {
        return None;
    }

    // Rounding is conservative in both directions — the bid rounds down and the
    // ask rounds up — so the realised spread is never TIGHTER than configured.
    // Phase 3 will place real orders this way, and measuring quotes we would
    // not actually post would be measuring the wrong strategy.
    let raw_bid = fair_value - half_spread - inventory_skew;
    // Positive inventory shifts BOTH prices down: buy less, sell more.
    let raw_ask = fair_value + half_spread - inventory_skew;

    let our_bid = snap(floor_to_tick(raw_bid, tick), tick).clamp(band_lo, band_hi);
    let our_ask = snap(ceil_to_tick(raw_ask, tick), tick).clamp(band_lo, band_hi);

    // Near the edges of the band a coarse tick can collapse or invert the
    // quote (fair value 0.05 with a 0.1 tick leaves nowhere to rest a bid
    // below an ask). There is no two-sided quote to post and nothing to
    // measure, so the tick is skipped rather than recorded as a locked market.
    if our_bid >= our_ask {
        return None;
    }

    // Polymarket rejects post-only orders which would match immediately.
    // https://docs.polymarket.com/concepts/order-lifecycle
    if market_ask.is_some_and(|a| a <= our_bid + 1e-9)
        || market_bid.is_some_and(|b| b >= our_ask - 1e-9)
    {
        return None;
    }

    Some(ShadowQuote {
        side,
        decision_delta,
        secs_left,
        fair_value,
        our_bid,
        our_ask,
        market_bid,
        market_ask,
        would_bid_fill: false,
        would_ask_fill: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const TICK: f64 = 0.01;

    fn q(fv: f64, hs: f64, bid: Option<f64>, ask: Option<f64>) -> ShadowQuote {
        quote_from_fair_value(Side::Up, 0.08, 90, fv, hs, 0.0, TICK, bid, ask)
            .expect("a 0.01 tick always admits a two-sided quote")
    }

    #[test]
    fn quotes_straddle_fair_value_by_the_half_spread() {
        let s = q(0.60, 0.04, None, None);
        assert!((s.our_bid - 0.56).abs() < 1e-9, "bid {}", s.our_bid);
        assert!((s.our_ask - 0.64).abs() < 1e-9, "ask {}", s.our_ask);
    }

    #[test]
    fn rounding_never_tightens_the_spread() {
        // 0.615 -/+ 0.04 = 0.575 / 0.655; conservative rounding widens both.
        let s = q(0.615, 0.04, None, None);
        assert!((s.our_bid - 0.57).abs() < 1e-9, "bid {}", s.our_bid);
        assert!((s.our_ask - 0.66).abs() < 1e-9, "ask {}", s.our_ask);
        assert!(s.our_ask - s.our_bid >= 0.08 - 1e-9);
    }

    #[test]
    fn quotes_are_clamped_into_the_tradeable_band() {
        let s = q(0.99, 0.04, None, None);
        assert!(s.our_ask <= MAX_QUOTE + 1e-9, "ask {}", s.our_ask);
        assert!(s.our_bid >= MIN_QUOTE - 1e-9, "bid {}", s.our_bid);
        let s = q(0.02, 0.04, None, None);
        assert!(s.our_bid >= MIN_QUOTE - 1e-9, "bid {}", s.our_bid);
        assert!(s.our_ask <= MAX_QUOTE + 1e-9, "ask {}", s.our_ask);
    }

    #[test]
    fn respects_a_coarser_tick() {
        let s = quote_from_fair_value(Side::Up, 0.08, 90, 0.62, 0.04, 0.0, 0.1, None, None)
            .expect("0.62 leaves room for a two-sided quote on a 0.1 grid");
        // 0.58 floors to 0.5, 0.66 ceils to 0.7 on a 0.1 grid.
        assert!((s.our_bid - 0.5).abs() < 1e-9, "bid {}", s.our_bid);
        assert!((s.our_ask - 0.7).abs() < 1e-9, "ask {}", s.our_ask);
    }

    #[test]
    fn marketable_candidates_are_post_only_rejections_not_maker_fills() {
        for (bid, ask) in [
            (None, Some(0.56)),
            (None, Some(0.50)),
            (Some(0.64), None),
            (Some(0.70), None),
        ] {
            assert!(
                quote_from_fair_value(Side::Up, 0.08, 90, 0.60, 0.04, 0.0, TICK, bid, ask,)
                    .is_none()
            );
        }
        let s = q(0.60, 0.04, Some(0.57), Some(0.63));
        assert!(!s.would_bid_fill && !s.would_ask_fill);
    }

    #[test]
    fn an_empty_book_side_is_never_a_fill() {
        let s = q(0.60, 0.04, None, None);
        assert!(!s.would_bid_fill);
        assert!(!s.would_ask_fill);
    }

    #[test]
    fn inventory_skew_moves_both_quotes_down() {
        let s =
            quote_from_fair_value(Side::Up, 0.08, 90, 0.60, 0.04, 0.02, TICK, None, None).unwrap();
        assert!((s.our_bid - 0.54).abs() < 1e-9);
        assert!((s.our_ask - 0.62).abs() < 1e-9);
    }

    #[test]
    fn malformed_inputs_never_produce_quotes_or_panic() {
        for tick in [0.0, -0.01, 0.03, 0.00001, f64::NAN, f64::INFINITY] {
            assert!(
                quote_from_fair_value(Side::Up, 0.08, 90, 0.60, 0.04, 0.0, tick, None, None,)
                    .is_none()
            );
        }
        for fv in [-0.1, 1.1, f64::NAN, f64::INFINITY] {
            assert!(
                quote_from_fair_value(Side::Up, 0.08, 90, fv, 0.04, 0.0, TICK, None, None,)
                    .is_none()
            );
        }
        assert!(quote_from_fair_value(
            Side::Up,
            0.08,
            90,
            0.6,
            0.04,
            0.0,
            TICK,
            Some(0.6),
            Some(0.5),
        )
        .is_none());
    }

    #[test]
    fn tick_parsing_rejects_nonsense() {
        assert_eq!(parse_tick("0.01"), Some(0.01));
        assert_eq!(parse_tick(" 0.001 "), Some(0.001));
        assert_eq!(parse_tick("0"), None);
        assert_eq!(parse_tick("-0.01"), None);
        assert_eq!(parse_tick("abc"), None);
        assert_eq!(parse_tick(""), None);
        assert_eq!(parse_tick("0.03"), None);
        assert_eq!(parse_tick("0.00001"), None);
    }

    #[test]
    fn quotes_land_exactly_on_the_tick_grid() {
        for tick in [0.01, 0.001, 0.1] {
            let mut fv = 0.02;
            while fv < 0.99 {
                // A skipped tick is a legitimate outcome near the band edges;
                // what must never happen is an off-grid or crossed price.
                if let Some(s) =
                    quote_from_fair_value(Side::Up, 0.08, 90, fv, 0.04, 0.0, tick, None, None)
                {
                    for price in [s.our_bid, s.our_ask] {
                        let steps = price / tick;
                        assert!(
                            (steps - steps.round()).abs() < 1e-6,
                            "price {} is off the {} grid (fv={})",
                            price,
                            tick,
                            fv
                        );
                        assert!(
                            (MIN_QUOTE..=MAX_QUOTE).contains(&price),
                            "price {} outside the tradeable band (tick={}, fv={})",
                            price,
                            tick,
                            fv
                        );
                    }
                    assert!(
                        s.our_bid < s.our_ask,
                        "crossed quote {} / {} (tick={}, fv={})",
                        s.our_bid,
                        s.our_ask,
                        tick,
                        fv
                    );
                }
                fv += 0.0037;
            }
        }
    }

    /// Regression: clamping to the raw 0.01/0.99 band used to emit a 0.01 bid
    /// in a 0.1-tick market — a price the exchange would reject.
    #[test]
    fn coarse_tick_near_the_band_edge_yields_no_quote_rather_than_an_invalid_one() {
        // fair value 0.05, half-spread 0.04, tick 0.1: the bid would floor to
        // 0.0 and the ask ceil to 0.1, leaving no two-sided quote.
        assert_eq!(
            quote_from_fair_value(Side::Up, 0.08, 90, 0.05, 0.04, 0.0, 0.1, None, None),
            None
        );
        // Same at the top of the band.
        assert_eq!(
            quote_from_fair_value(Side::Up, 0.08, 90, 0.96, 0.04, 0.0, 0.1, None, None),
            None
        );
    }

    /// On the tick the market actually uses, the whole priceable range is
    /// quotable — the skip above must not silently swallow normal cases.
    #[test]
    fn hundredth_tick_quotes_the_entire_usable_range() {
        let mut fv = 0.06;
        while fv < 0.94 {
            assert!(
                quote_from_fair_value(Side::Up, 0.08, 90, fv, 0.04, 0.0, 0.01, None, None)
                    .is_some(),
                "no quote at fv={} on a 0.01 tick",
                fv
            );
            fv += 0.0037;
        }
    }

    #[test]
    fn maker_mode_parsing() {
        assert_eq!(MakerMode::parse("off"), Ok(MakerMode::Off));
        assert_eq!(MakerMode::parse(""), Ok(MakerMode::Off));
        assert_eq!(MakerMode::parse("SHADOW"), Ok(MakerMode::Shadow));
        assert_eq!(MakerMode::default(), MakerMode::Off);
        // The important one: live must NOT silently degrade to shadow.
        let err = MakerMode::parse("live").expect_err("live must be rejected");
        assert!(err.contains("Phase 3"), "unexpected error: {}", err);
        assert!(MakerMode::parse("banana").is_err());
    }

    #[test]
    fn down_side_quotes_are_built_from_the_down_probability() {
        // The caller passes the already-complemented fair value, so a Down
        // quote at p=0.40 straddles 0.40, not 0.60.
        let s = quote_from_fair_value(Side::Down, -0.08, 90, 0.40, 0.04, 0.0, TICK, None, None)
            .expect("two-sided quote");
        assert_eq!(s.side, Side::Down);
        assert!((s.our_bid - 0.36).abs() < 1e-9);
        assert!((s.our_ask - 0.44).abs() < 1e-9);
    }
}
