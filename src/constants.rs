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

/// Length of a trading window in seconds.
pub const WINDOW_SECS: i64 = 300;
