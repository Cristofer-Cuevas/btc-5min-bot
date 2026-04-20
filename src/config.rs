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

    // Risk limits
    pub max_consecutive_losses: i64,
    pub daily_loss_limit_usdc: f64,

    // Proxy wallet
    pub poly_proxy_address: String,

    // Operational
    pub db_path: String,
    pub dry_run: bool,
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

        let btc_threshold_pct = env::var("BTC_THRESHOLD_PCT")
            .unwrap_or_else(|_| "0.07".into())
            .parse()
            .unwrap_or(0.07);

        let max_ask_price = env::var("MAX_ASK_PRICE")
            .unwrap_or_else(|_| "0.80".into())
            .parse()
            .unwrap_or(0.80);

        let max_spread = env::var("MAX_SPREAD")
            .unwrap_or_else(|_| "0.10".into())
            .parse()
            .unwrap_or(0.10);

        let bet_shares = env::var("BET_SHARES")
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
                .unwrap_or_else(|_| "0.2".into())
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

        let dry_run = env::var("DRY_RUN")
            .unwrap_or_else(|_| "true".into())
            .to_lowercase()
            == "true";

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
            max_consecutive_losses,
            daily_loss_limit_usdc,
            poly_proxy_address,
            db_path,
            dry_run,
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