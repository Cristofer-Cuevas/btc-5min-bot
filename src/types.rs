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
}

impl TokenBook {
    pub fn spread(&self) -> Option<f64> {
        match (self.best_bid, self.best_ask) {
            (Some(bid), Some(ask)) => Some(ask - bid),
            _ => None,
        }
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
    /// Returns Some(strength) where strength is 0.0–1.0, or None if insufficient samples.
    pub fn trend_strength(&self) -> Option<f64> {
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
