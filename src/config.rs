use std::env;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::types::SharedConfig;

#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    // Polymarket auth
    pub poly_private_key: String,
    pub poly_address: String,
    pub poly_api_key: String,
    pub poly_api_secret: String,
    pub poly_api_passphrase: String,

    // Telegram
    pub telegram_bot_token: String,
    pub telegram_chat_id: i64,

    // Strategy parameters (mutable via Telegram)
    pub btc_threshold_pct: f64,
    pub max_ask_price: f64,
    pub max_spread: f64,
    pub bet_shares: f64,
    pub max_slippage: f64,
    pub min_trend_strength: f64,
    /// Minimum |delta_now| / |delta_past| ratio required to enter. Below this
    /// the move is contracting (reversing toward zero) and the signal is
    /// rejected as "delta_reversing". 0.0 disables the filter entirely.
    pub min_delta_momentum: f64,

    // Risk limits
    pub max_consecutive_losses: i64,
    pub daily_loss_limit_usdc: f64,

    // Proxy wallet
    pub poly_proxy_address: String,

    // Operational
    pub db_path: String,
    pub dry_run: bool,
    /// Explicit opt-in to the experimental taker strategy. Collection continues when false.
    pub taker_enabled: bool,
    pub max_trade_cost_usdc: f64,
    pub max_open_cost_usdc: f64,
    pub max_book_age_ms: u64,
    pub max_price_age_ms: u64,

    /// How often to log a `signals` row while the rejection reason is
    /// unchanged, in milliseconds. 0 disables the cadence, restoring the
    /// original change-only logging.
    ///
    /// Without a cadence the table records only transition moments, which
    /// leaves the fair-value fit with no data in any region the evaluation sits
    /// in quietly -- above all the near-zero deltas a market maker cares most
    /// about.
    pub signal_log_cadence_ms: i64,

    // ── Maker (Phase 2: shadow quoting, MEASUREMENT ONLY) ──
    /// Off by default. `shadow` computes and records the quotes the bot would
    /// have posted; it never sends an order. `live` is rejected outright —
    /// see [`crate::maker::MakerMode::parse`].
    pub maker_mode: crate::maker::MakerMode,
    /// Path to the fair-value curve fitted by `tools/build_fairvalue.py`.
    /// Required (and validated) whenever `maker_mode` is not Off.
    pub fairvalue_path: String,
    /// Half-width of the quoted spread around fair value. Deliberately wide:
    /// order placement is 400-1400ms, so tightness cannot be defended.
    pub maker_half_spread: f64,

    /// When true, the entry threshold and side selection use the TWAP-based
    /// delta (strike = TWAP at window open) instead of the Binance spot delta.
    ///
    /// Defaults to FALSE: shipping this must not change live trading behavior.
    /// With it false the bot runs in shadow mode — the TWAP delta is computed
    /// and logged, but the spot delta still drives every decision.
    pub use_twap_strike: bool,

    // ── Chainlink Data Streams fallback ──
    // NOTE: the fetch/decode path is NOT implemented — the v2 report schema
    // used by the TWAP streams is not published in Chainlink's official docs
    // (see report notes). These fields carry the credentials so the path can be
    // added without a config change once the schema is confirmed.
    // Unread until the fetch/decode path lands; kept so enabling the fallback
    // is a config change only, not a schema change.
    #[allow(dead_code)]
    pub chainlink_api_key: String,
    #[allow(dead_code)]
    pub chainlink_api_secret: String,
    #[allow(dead_code)]
    pub chainlink_stream_id: String,
    /// Defaults to FALSE. Enabling it currently only logs an error — see above.
    #[allow(dead_code)]
    pub use_chainlink_fallback: bool,
}

impl RuntimeConfig {
    pub fn from_env() -> Result<Self, String> {
        dotenvy::dotenv().ok();

        let poly_private_key = env::var("POLY_PRIVATE_KEY").unwrap_or_default();
        let poly_address = env::var("POLY_ADDRESS").unwrap_or_default();
        let poly_api_key = env::var("POLY_API_KEY").unwrap_or_default();
        let poly_api_secret = env::var("POLY_API_SECRET").unwrap_or_default();
        let poly_api_passphrase = env::var("POLY_API_PASSPHRASE").unwrap_or_default();

        let telegram_bot_token = env::var("TELEGRAM_BOT_TOKEN")
            .map_err(|_| "TELEGRAM_BOT_TOKEN not set".to_string())?;
        let telegram_chat_id: i64 = env::var("TELEGRAM_CHAT_ID")
            .map_err(|_| "TELEGRAM_CHAT_ID not set".to_string())?
            .parse()
            .map_err(|_| "TELEGRAM_CHAT_ID must be a number".to_string())?;

        let btc_threshold_pct: f64 = env::var("BTC_THRESHOLD_PCT")
            .unwrap_or_else(|_| "0.07".into())
            .parse()
            .unwrap_or(0.07);

        let max_ask_price: f64 = env::var("MAX_ASK_PRICE")
            .unwrap_or_else(|_| "0.80".into())
            .parse()
            .unwrap_or(0.80);

        let max_spread: f64 = env::var("MAX_SPREAD")
            .unwrap_or_else(|_| "0.10".into())
            .parse()
            .unwrap_or(0.10);

        let bet_shares: f64 = env::var("BET_SHARES")
            .unwrap_or_else(|_| "5".into())
            .parse()
            .unwrap_or(5.0);

        let max_slippage = {
            let raw: f64 = env::var("MAX_SLIPPAGE")
                .unwrap_or_else(|_| "0.03".into())
                .parse()
                .unwrap_or(0.03);
            if raw > 0.0 && raw <= 0.20 {
                raw
            } else {
                tracing::warn!(
                    "MAX_SLIPPAGE {:.4} outside (0.0, 0.20]; clamping to default 0.03",
                    raw
                );
                0.03
            }
        };

        let min_trend_strength = {
            let raw: f64 = env::var("MIN_TREND_STRENGTH")
                .unwrap_or_else(|_| "0.41".into())
                .parse()
                .unwrap_or(0.2);
            if (0.0..=1.0).contains(&raw) {
                raw
            } else {
                tracing::warn!(
                    "MIN_TREND_STRENGTH {:.4} outside [0.0, 1.0]; clamping to default 0.2",
                    raw
                );
                0.2
            }
        };

        let min_delta_momentum = {
            let raw: f64 = env::var("MIN_DELTA_MOMENTUM")
                .unwrap_or_else(|_| "0.70".into())
                .parse()
                .unwrap_or(0.70);
            // 0.0 is a valid value meaning "filter disabled", so the lower
            // bound is inclusive.
            if (0.0..=5.0).contains(&raw) {
                raw
            } else {
                tracing::warn!(
                    "MIN_DELTA_MOMENTUM {:.4} outside [0.0, 5.0]; clamping to default 0.70",
                    raw
                );
                0.70
            }
        };

        let db_path = env::var("DB_PATH").unwrap_or_else(|_| "trades.db".into());

        let max_consecutive_losses: i64 = env::var("MAX_CONSECUTIVE_LOSSES")
            .unwrap_or_else(|_| "3".into())
            .parse()
            .unwrap_or(3);

        let daily_loss_limit_usdc: f64 = env::var("DAILY_LOSS_LIMIT_USDC")
            .unwrap_or_else(|_| "20.0".into())
            .parse()
            .unwrap_or(20.0);

        let poly_proxy_address = env::var("POLY_PROXY_ADDRESS").unwrap_or_default();

        let dry_run = env_bool("DRY_RUN", true)?;

        let taker_enabled = env_bool("TAKER_ENABLED", false)?;
        let max_trade_cost_usdc = positive_env("MAX_TRADE_COST_USDC", 5.0)?;
        let max_open_cost_usdc = positive_env("MAX_OPEN_COST_USDC", 10.0)?;
        let max_book_age_ms = positive_env("MAX_BOOK_AGE_MS", 2000.0)? as u64;
        let max_price_age_ms = positive_env("MAX_PRICE_AGE_MS", 2000.0)? as u64;
        if max_book_age_ms == 0 || max_price_age_ms == 0 {
            return Err("Feed freshness limits must be at least 1ms".into());
        }
        for (name, value) in [
            ("BTC_THRESHOLD_PCT", btc_threshold_pct),
            ("MAX_ASK_PRICE", max_ask_price),
            ("MAX_SPREAD", max_spread),
            ("BET_SHARES", bet_shares),
            ("DAILY_LOSS_LIMIT_USDC", daily_loss_limit_usdc),
        ] {
            if !value.is_finite() || value <= 0.0 {
                return Err(format!("{name} must be finite and positive"));
            }
        }
        if max_ask_price >= 1.0 || max_spread >= 1.0 || bet_shares < 5.0
            || max_consecutive_losses < 1 || max_trade_cost_usdc > max_open_cost_usdc
        {
            return Err("Invalid price, share, or risk limits".into());
        }

        // Defaults to false: only an explicit "true" opts in.
        let use_twap_strike = env::var("USE_TWAP_STRIKE")
            .unwrap_or_else(|_| "false".into())
            .to_lowercase()
            == "true";
        if use_twap_strike {
            tracing::warn!(
                "USE_TWAP_STRIKE=true: entry threshold and side selection are driven by the \
                 TWAP delta. Entries reject with 'twap_unavailable' when the feed is stale."
            );
        } else {
            tracing::info!("USE_TWAP_STRIKE=false (shadow mode): TWAP delta logged, spot delta decides");
        }

        // ── Maker configuration ──
        let signal_log_cadence_ms: i64 = {
            let raw: i64 = env::var("SIGNAL_LOG_CADENCE_MS")
                .unwrap_or_else(|_| "5000".into())
                .parse()
                .unwrap_or(5000);
            // Below one loop tick (250ms) the cadence would fire every
            // iteration. 0 is a valid value meaning "disabled", so it is
            // allowed through explicitly rather than caught by the range.
            if raw == 0 || (250..=60_000).contains(&raw) {
                raw
            } else {
                tracing::warn!(
                    "SIGNAL_LOG_CADENCE_MS {} outside [250, 60000] (0 = off);                      clamping to default 5000",
                    raw
                );
                5000
            }
        };
        if signal_log_cadence_ms > 0 {
            tracing::info!(
                "SIGNAL_LOG_CADENCE_MS={} - signals written on reason change AND every                  {}ms while a decision delta exists",
                signal_log_cadence_ms,
                signal_log_cadence_ms
            );
        } else {
            tracing::info!("SIGNAL_LOG_CADENCE_MS=0 - signals logged on reason change only");
        }

        let maker_mode = crate::maker::MakerMode::parse(
            &env::var("MAKER_MODE").unwrap_or_default(),
        )?;

        let fairvalue_path = env::var("FAIRVALUE_PATH")
            .unwrap_or_else(|_| "/etc/btc-5min-bot/fairvalue.json".into());

        let maker_half_spread = {
            let raw: f64 = env::var("MAKER_HALF_SPREAD")
                .unwrap_or_else(|_| "0.04".into())
                .parse()
                .unwrap_or(0.04);
            // A half-spread at or below one tick is not a maker strategy, and
            // above 0.25 the quotes leave the tradeable band entirely.
            if (0.01..=0.25).contains(&raw) {
                raw
            } else {
                tracing::warn!(
                    "MAKER_HALF_SPREAD {:.4} outside [0.01, 0.25]; clamping to default 0.04",
                    raw
                );
                0.04
            }
        };

        if maker_mode != crate::maker::MakerMode::Off {
            tracing::info!(
                "MAKER_MODE={} — shadow quoting active. NO orders are placed or \
                 cancelled; the taker path is unchanged.",
                maker_mode.as_str()
            );
        }

        if !dry_run && poly_proxy_address.is_empty() {
            return Err("POLY_PROXY_ADDRESS is required when DRY_RUN=false".to_string());
        }

        if !poly_proxy_address.is_empty()
            && (!poly_proxy_address.starts_with("0x") || poly_proxy_address.len() != 42)
        {
            return Err(format!(
                "POLY_PROXY_ADDRESS must be a valid 0x-prefixed address (42 chars), got '{}'",
                poly_proxy_address
            ));
        }

        let chainlink_api_key = env::var("CHAINLINK_API_KEY").unwrap_or_default();
        let chainlink_api_secret = env::var("CHAINLINK_API_SECRET").unwrap_or_default();
        let chainlink_stream_id = env::var("CHAINLINK_STREAM_ID").unwrap_or_default();
        let use_chainlink_fallback = env::var("USE_CHAINLINK_FALLBACK")
            .unwrap_or_else(|_| "false".into())
            .to_lowercase()
            == "true";
        if use_chainlink_fallback {
            // Fail loudly rather than silently no-op: the fetch/decode path is
            // not implemented because the v2 report schema for the TWAP streams
            // is not published in Chainlink's official documentation.
            tracing::error!(
                "USE_CHAINLINK_FALLBACK=true but the Chainlink Data Streams fallback is NOT \
                 IMPLEMENTED (v2 report schema unconfirmed). RTDS remains the only TWAP source \
                 and no fallback fetch will occur."
            );
        }

        Ok(Self {
            poly_private_key,
            poly_address,
            poly_api_key,
            poly_api_secret,
            poly_api_passphrase,
            telegram_bot_token,
            telegram_chat_id,
            btc_threshold_pct,
            max_ask_price,
            max_spread,
            bet_shares,
            max_slippage,
            min_trend_strength,
            min_delta_momentum,
            max_consecutive_losses,
            daily_loss_limit_usdc,
            poly_proxy_address,
            db_path,
            dry_run,
            taker_enabled,
            max_trade_cost_usdc,
            max_open_cost_usdc,
            max_book_age_ms,
            max_price_age_ms,
            use_twap_strike,
            signal_log_cadence_ms,
            maker_mode,
            fairvalue_path,
            maker_half_spread,
            chainlink_api_key,
            chainlink_api_secret,
            chainlink_stream_id,
            use_chainlink_fallback,
        })
    }

    pub fn into_shared(self) -> SharedConfig {
        Arc::new(RwLock::new(self))
    }

    pub fn has_trading_credentials(&self) -> bool {
        !self.poly_private_key.is_empty()
            && !self.poly_address.is_empty()
            && !self.poly_api_key.is_empty()
            && !self.poly_api_secret.is_empty()
    }
}

fn env_bool(name: &str, default: bool) -> Result<bool, String> {
    match env::var(name) {
        Ok(s) => match s.trim().to_ascii_lowercase().as_str() {
            "true" => Ok(true),
            "false" => Ok(false),
            _ => Err(format!("{name} must be true or false")),
        },
        Err(env::VarError::NotPresent) => Ok(default),
        Err(e) => Err(format!("{name}: {e}")),
    }
}

fn positive_env(name: &str, default: f64) -> Result<f64, String> {
    let value = match env::var(name) {
        Ok(s) => s.parse::<f64>().map_err(|_| format!("{name} must be a number"))?,
        Err(env::VarError::NotPresent) => default,
        Err(e) => return Err(format!("{name}: {e}")),
    };
    if !value.is_finite() || value <= 0.0 {
        return Err(format!("{name} must be finite and positive"));
    }
    Ok(value)
}

#[cfg(test)]
pub(crate) fn test_config() -> RuntimeConfig {
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
            taker_enabled: false,
            max_trade_cost_usdc: 5.0,
            max_open_cost_usdc: 10.0,
            max_book_age_ms: 2000,
            max_price_age_ms: 2000,
            // Fixtures keep the original change-only logging so they exercise
            // the taker path exactly as it shipped.
            signal_log_cadence_ms: 0,
            use_twap_strike: false,
            // The taker fixtures must exercise the taker path exactly as it
            // shipped: maker mode OFF, so no fair-value model is consulted.
            maker_mode: crate::maker::MakerMode::Off,
            fairvalue_path: String::new(),
            maker_half_spread: 0.04,
            chainlink_api_key: String::new(),
            chainlink_api_secret: String::new(),
            chainlink_stream_id: String::new(),
            use_chainlink_fallback: false,
        }
}
