/// RTDS confirmation freshness window (milliseconds).
/// If RTDS hasn't updated within this period, treat it as stale.
pub const RTDS_STALE_MS: u64 = 60_000;

/// Maximum number of entry attempts per window before giving up.
pub const MAX_ENTRY_ATTEMPTS: u32 = 3;

/// Minimum total size (shares) on the ask side to ensure fill.
pub const MIN_ASK_DEPTH: f64 = 50.0;

/// Minimum number of Binance price samples before trend_strength is valid.
pub const MIN_TREND_SAMPLES: usize = 100;

/// How long (seconds) to retain token→window mappings for resolution matching.
pub const TOKEN_WINDOW_RETENTION_SECS: u64 = 1800;

/// Main loop tick interval (milliseconds).
pub const BOT_TICK_MS: u64 = 250;

/// Max age for a TWAP reading to count as fresh (ms).
pub const TWAP_MAX_AGE_MS: i64 = 60_000;

/// Silence on the TWAP topic after which the subscription is re-sent on the
/// existing socket. Also the minimum gap between re-subscribe attempts, so a
/// genuinely dead feed cannot spam.
pub const TWAP_RESUBSCRIBE_AFTER_MS: u64 = 45_000;

/// Silence on the TWAP topic after which the whole RTDS socket is torn down and
/// reconnected, re-establishing both the spot and TWAP subscriptions.
pub const TWAP_RECONNECT_AFTER_MS: u64 = 120_000;

/// How many recent windows the TWAP coverage counter tracks.
pub const TWAP_COVERAGE_WINDOW: usize = 20;

/// How far back to look when measuring whether the delta is expanding or
/// contracting.
pub const DELTA_HISTORY_WINDOW_MS: i64 = 45_000;

/// Minimum lookback age required before momentum can be judged. Without
/// this, early-window evaluations compare against a near-identical
/// reading and always look flat.
pub const DELTA_MOMENTUM_MIN_AGE_MS: i64 = 20_000;

/// Floor for "the move has started", used to measure rise time.
pub const DELTA_RISE_FLOOR_PCT: f64 = 0.02;

/// Defensive cap on delta history length. At a 250ms tick the 45s window holds
/// ~180 entries; this only bounds growth if a window somehow stalls.
pub const DELTA_HISTORY_MAX_ENTRIES: usize = 1200;

/// Lookback for the Binance-derived TWAP estimate, matched to Polymarket's
/// 30s settlement TWAP for 5-minute markets.
pub const BINANCE_TWAP_WINDOW_MS: u64 = 30_000;

/// A TWAP reading observed slightly before the boundary is expected
/// (the feed publishes on its own cadence). But a reading from well
/// before the boundary belongs to the previous window.
pub const TWAP_STRIKE_LOOKBACK_TOLERANCE_MS: i64 = 30_000;

/// Length of a trading window in seconds.
pub const WINDOW_SECS: i64 = 300;
