mod binance_ws;
mod config;
mod constants;
mod db;
mod discovery;
mod market_ws;
mod rtds;
mod strategy;
mod telegram;
mod trading;
mod types;

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use alloy::signers::local::PrivateKeySigner;
use alloy::signers::Signer;
use tokio::sync::{mpsc, RwLock};
use tracing::{error, info, warn};
use tracing_subscriber::fmt::time::FormatTime;

struct EtTimer;

impl FormatTime for EtTimer {
    fn format_time(&self, w: &mut tracing_subscriber::fmt::format::Writer<'_>) -> std::fmt::Result {
        let now = chrono::Utc::now().with_timezone(&chrono_tz::America::New_York);
        write!(w, "{}", now.format("%Y-%m-%d %H:%M:%S %Z"))
    }
}

use crate::constants::*;
use crate::db::Database;
use crate::market_ws::{ClobCommand, ResolutionEvent};
use crate::types::*;

const BOT_VERSION: &str = env!("BOT_GIT_HASH");

#[tokio::main]
async fn main() {
    // ── 1. Load config ──
    let cfg = match config::RuntimeConfig::from_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Configuration error: {}", e);
            std::process::exit(1);
        }
    };

    // ── Init tracing ──
    let log_level = std::env::var("LOG_LEVEL").unwrap_or_else(|_| "info".into());
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(&log_level)),
        )
        .with_timer(EtTimer)
        .init();

    info!("Polymarket BTC 5-min bot starting (version {})", BOT_VERSION);
    info!("DRY_RUN = {}", cfg.dry_run);

    // ── 2. Parse wallet once at startup ──
    let wallet: SharedWallet = if !cfg.poly_private_key.is_empty() {
        match PrivateKeySigner::from_str(&cfg.poly_private_key) {
            Ok(w) => {
                let w = w.with_chain_id(Some(137));
                info!("Wallet loaded: 0x{}", w.address());
                Arc::new(w)
            }
            Err(e) => {
                error!("Invalid POLY_PRIVATE_KEY: {}", e);
                std::process::exit(1);
            }
        }
    } else {
        // Dry-run mode doesn't need a real wallet; use a dummy
        let dummy_key = "0000000000000000000000000000000000000000000000000000000000000001";
        Arc::new(
            PrivateKeySigner::from_str(dummy_key)
                .unwrap()
                .with_chain_id(Some(137)),
        )
    };

    // ── 3. Init SQLite ──
    let db = match Database::new(&cfg.db_path) {
        Ok(d) => Arc::new(d),
        Err(e) => {
            error!("Database init failed: {}", e);
            std::process::exit(1);
        }
    };

    // ── 4. Shared state ──
    let shared_config = cfg.into_shared();
    let btc_price: SharedBtcPrice = Arc::new(RwLock::new(BtcPriceState::default()));
    let binance_price: SharedBinancePrice = Arc::new(RwLock::new(BinanceBtcPrice::default()));
    let market_state: SharedMarketState = Arc::new(RwLock::new(MarketState::default()));
    let window_state: SharedWindowState = Arc::new(RwLock::new(WindowState::default()));
    let token_window_map: SharedTokenWindowMap = Arc::new(RwLock::new(HashMap::new()));
    let start_time = std::time::Instant::now();

    let http_client = reqwest::Client::new();

    // ── 5. Build authenticated SDK client once (live mode only) ──
    let sdk_client: Option<SharedSdkClient> = {
        let cfg_ref = shared_config.read().await;
        if !cfg_ref.dry_run && cfg_ref.has_trading_credentials() {
            info!("Building authenticated SDK client...");
            match trading::build_shared_sdk_client(&cfg_ref).await {
                Ok(c) => {
                    info!("SDK client authenticated successfully (auth verified)");
                    Some(c)
                }
                Err(e) => {
                    error!("SDK client authentication failed: {}", e);
                    telegram::send_message(&cfg_ref, &format!("🛑 Auth failed: {}", e)).await;
                    std::process::exit(1);
                }
            }
        } else {
            if cfg_ref.dry_run {
                info!("Skipping SDK client build (dry-run mode)");
            } else {
                info!("Skipping SDK client build (no trading credentials)");
            }
            None
        }
    };

    // ── 6. Notify startup ──
    {
        let cfg = shared_config.read().await;
        let mode = if cfg.dry_run { "DRY_RUN" } else { "LIVE" };
        telegram::send_message(&cfg, &format!("🤖 Bot started ({}) v{}", mode, BOT_VERSION)).await;
    }

    // ── 7. Channels ──
    let (clob_cmd_tx, clob_cmd_rx) = mpsc::channel::<ClobCommand>(16);
    let (resolution_tx, mut resolution_rx) = mpsc::channel::<ResolutionEvent>(16);

    // ── 8. Spawn RTDS WebSocket (BTC price feed — Chainlink, ground truth) ──
    let btc_price_rtds = btc_price.clone();
    tokio::spawn(async move {
        rtds::run_rtds_feed(btc_price_rtds).await;
    });

    // ── 8b. Spawn Binance WebSocket (BTC price feed — primary, faster) ──
    let binance_price_ws = binance_price.clone();
    tokio::spawn(async move {
        binance_ws::run_binance_feed(binance_price_ws).await;
    });

    // ── 9. Spawn CLOB WebSocket (market data) ──
    let market_state_ws = market_state.clone();
    tokio::spawn(async move {
        market_ws::run_clob_ws(market_state_ws, clob_cmd_rx, resolution_tx).await;
    });

    // ── 10. Spawn Telegram bot ──
    let tg_config = shared_config.clone();
    let tg_btc = btc_price.clone();
    let tg_ws = window_state.clone();
    let tg_db = db.clone();
    let tg_wallet = wallet.clone();
    let tg_http = http_client.clone();
    let tg_sdk = sdk_client.clone();
    tokio::spawn(async move {
        telegram::run_telegram_bot(tg_config, tg_btc, tg_ws, tg_db, start_time, tg_wallet, tg_http, tg_sdk).await;
    });

    // ── 11. Spawn daily summary task ──
    let ds_config = shared_config.clone();
    let ds_db = db.clone();
    tokio::spawn(async move {
        loop {
            let now = chrono::Utc::now();
            let tomorrow = (now + chrono::Duration::days(1))
                .date_naive()
                .and_hms_opt(0, 0, 0)
                .unwrap();
            let until_midnight = tomorrow
                .signed_duration_since(now.naive_utc())
                .to_std()
                .unwrap_or(std::time::Duration::from_secs(3600));
            tokio::time::sleep(until_midnight).await;
            telegram::send_daily_summary(&ds_config, &ds_db).await;
        }
    });

    // ── 12. Setup graceful shutdown ──
    let shutdown_config = shared_config.clone();
    let (shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(1);
    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        info!("Received shutdown signal");
        telegram::notify(&shutdown_config, "🛑 Bot stopped").await;
        let _ = shutdown_tx.send(()).await;
    });

    // ── 13. Main loop: window rotation + strategy evaluation ──
    let mut last_window_ts: u64 = 0;

    info!("Entering main trading loop");

    loop {
        // Check for shutdown
        if shutdown_rx.try_recv().is_ok() {
            info!("Shutting down gracefully...");
            break;
        }

        let current_ts = discovery::current_window_ts();
        let secs_left = discovery::secs_remaining();

        // ── Window rotation ──
        if current_ts != last_window_ts {
            info!(
                "New window: ts={} slug={} ({}s remaining)",
                current_ts,
                discovery::window_slug(current_ts),
                secs_left
            );

            // Reset window state
            {
                let mut ws = window_state.write().await;
                ws.window_ts = current_ts;
                ws.entered = false;
                ws.failed_attempts = 0;
                ws.last_signal_reason = None;
                ws.market = None;
                ws.next_window_prefetched = false;
            }

            // Reset market state
            {
                let mut ms = market_state.write().await;
                *ms = MarketState::default();
            }

            // Record window open price (RTDS/Chainlink)
            {
                let mut btc = btc_price.write().await;
                btc.window_open_price = btc.current_price;
                if let Some(p) = btc.window_open_price {
                    info!("Window open BTC price (RTDS): ${:.2}", p);
                }
            }

            // Record window open price (Binance)
            {
                let mut bn = binance_price.write().await;
                bn.window_open_price = bn.current_price;
                if let Some(p) = bn.window_open_price {
                    info!("Window open BTC price (Binance): ${:.2}", p);
                }
            }

            // Discover market
            let slug = discovery::window_slug(current_ts);
            match discovery::fetch_market(&http_client, &slug).await {
                Some(market) => {
                    // Subscribe CLOB WebSocket to new tokens
                    let _ = clob_cmd_tx
                        .send(ClobCommand::Subscribe {
                            up_token_id: market.up_token_id.clone(),
                            down_token_id: market.down_token_id.clone(),
                        })
                        .await;

                    // Populate token_id → window_ts map for resolution matching
                    {
                        let mut twm = token_window_map.write().await;
                        twm.insert(market.up_token_id.clone(), current_ts);
                        twm.insert(market.down_token_id.clone(), current_ts);
                        // Evict entries older than TOKEN_WINDOW_RETENTION_SECS (30 minutes)
                        twm.retain(|_, ts| current_ts.saturating_sub(*ts) <= TOKEN_WINDOW_RETENTION_SECS);
                    }

                    let mut ws = window_state.write().await;
                    ws.market = Some(market);
                }
                None => {
                    warn!("Could not discover market for {}, skipping window", slug);
                }
            }

            last_window_ts = current_ts;
        }

        // ── Pre-fetch next window (once, 10-15s before end) ──
        if secs_left <= 15 && secs_left > 10 {
            let should_prefetch = {
                let ws = window_state.read().await;
                !ws.next_window_prefetched
            };
            if should_prefetch {
                {
                    let mut ws = window_state.write().await;
                    ws.next_window_prefetched = true;
                }
                let next_ts = discovery::next_window_ts();
                let next_slug = discovery::window_slug(next_ts);
                let client = http_client.clone();
                tokio::spawn(async move {
                    let _ = discovery::fetch_market(&client, &next_slug).await;
                });
            }
        }

        // ── Strategy evaluation ──
        let signal_detected_ms = chrono::Utc::now().timestamp_millis();

        let (eval_result, dry_run_flag) = {
            let ws = window_state.read().await;
            let cfg = shared_config.read().await;
            let btc = btc_price.read().await;
            let bn = binance_price.read().await;
            let ms = market_state.read().await;

            let dry_run = cfg.dry_run;

            let result = if ws.paused {
                EvaluationResult::rejected("paused")
            } else if ws.entered {
                EvaluationResult::rejected("already_entered")
            } else if ws.failed_attempts >= MAX_ENTRY_ATTEMPTS {
                EvaluationResult::rejected("max_attempts")
            } else if let Some(market) = ws.market.as_ref() {
                if ms.resolved {
                    EvaluationResult::rejected("market_resolved")
                } else {
                    strategy::evaluate_entry(&cfg, &btc, &bn, &ms, market, secs_left)
                }
            } else {
                EvaluationResult::rejected("no_market")
            };

            (result, dry_run)
        };

        // Signal logging: skip plumbing-noise reasons entirely, always write
        // "entered", otherwise write only when rejection reason changes.
        let reason = eval_result.rejection_reason;
        let skip_noise = matches!(
            reason,
            "no_binance_price" | "no_ask" | "no_market" | "paused" | "already_entered"
        );

        if !skip_noise {
            let should_write = reason == "entered" || {
                let ws = window_state.read().await;
                ws.last_signal_reason.as_deref() != Some(reason)
            };

            if should_write {
                db.insert_signal(&eval_result, current_ts as i64, secs_left, dry_run_flag);
                let mut ws = window_state.write().await;
                ws.last_signal_reason = Some(reason.to_string());
            }
        }

        if let Some(ref signal) = eval_result.signal {
            info!(
                "ENTRY SIGNAL: {} | BTC Δ: {:+.4}% | Ask: ${:.2} | Spread: ${:.2} | {}s left",
                signal.side, signal.btc_delta_pct, signal.ask_price, signal.spread, signal.secs_left
            );

            let cfg = shared_config.read().await;
            let shares = cfg.bet_shares;
            let dry_run = cfg.dry_run;

            // Capture market snapshot for the trade record
            let (
                bid_price_observed, ask_depth_val, bid_depth_val,
                up_tc, down_tc, opposite_side_ask, neg_risk_val,
            ) = {
                let ws = window_state.read().await;
                let ms = market_state.read().await;
                let market = ws.market.as_ref().unwrap();
                let (predicted_book, opposite_book) = if signal.side == "Up" {
                    (&ms.up_book, &ms.down_book)
                } else {
                    (&ms.down_book, &ms.up_book)
                };
                (
                    predicted_book.best_bid,
                    predicted_book.ask_depth,
                    predicted_book.bid_depth,
                    ms.up_trade_count,
                    ms.down_trade_count,
                    opposite_book.best_ask,
                    market.neg_risk,
                )
            };

            // Capture price source snapshot
            let (bn_entry, bn_open, rtds_entry, rtds_open, rtds_stale, trend_val) = {
                let bn = binance_price.read().await;
                let btc = btc_price.read().await;
                let now_ms = chrono::Utc::now().timestamp_millis() as u64;
                let stale = btc.last_update_ms == 0
                    || now_ms.saturating_sub(btc.last_update_ms) > RTDS_STALE_MS;
                (
                    bn.current_price,
                    bn.window_open_price,
                    btc.current_price,
                    btc.window_open_price,
                    stale,
                    bn.trend_strength(),
                )
            };

            let limit_price = {
                let raw = signal.ask_price + cfg.max_slippage;
                let tick = (raw * 100.0).round() / 100.0;
                tick.clamp(0.02, 0.99)
            };

            let failed_before = {
                let ws = window_state.read().await;
                ws.failed_attempts
            };

            let order_sent_ms = chrono::Utc::now().timestamp_millis();

            let fill = if dry_run {
                trading::simulate_trade(signal, shares)
            } else {
                let ws = window_state.read().await;
                let market = ws.market.as_ref().unwrap();
                let sdk = sdk_client.as_ref().expect("SDK client required for live trading");
                match trading::place_fok_buy(
                    sdk,
                    &cfg,
                    &wallet,
                    signal,
                    shares,
                    &market.tick_size,
                    market.neg_risk,
                )
                .await
                {
                    Ok(fill) => fill,
                    Err(e) => {
                        error!("Order placement failed: {}", e);
                        {
                            let mut ws = window_state.write().await;
                            ws.failed_attempts += 1;
                            if ws.failed_attempts >= MAX_ENTRY_ATTEMPTS {
                                warn!("Max entry attempts ({}) reached for window {}", MAX_ENTRY_ATTEMPTS, current_ts);
                                telegram::notify(&shared_config, &format!("⚠️ Max retries reached for window {}", current_ts)).await;
                            }
                        }
                        let failed_signal = EvaluationResult {
                            rejection_reason: "failed_fok",
                            signal: None,
                            ..eval_result.clone()
                        };
                        db.insert_signal(&failed_signal, current_ts as i64, secs_left, dry_run);
                        let msg = format!("⚠️ Order failed: {}", e);
                        telegram::notify(&shared_config, &msg).await;
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                        continue;
                    }
                }
            };

            let order_ack_ms = chrono::Utc::now().timestamp_millis();

            // If FOK didn't fill, don't record a trade
            if fill.filled_size == 0.0 {
                warn!("FOK order {} did not fill, not recording trade", fill.order_id);
                {
                    let mut ws = window_state.write().await;
                    ws.failed_attempts += 1;
                    if ws.failed_attempts >= MAX_ENTRY_ATTEMPTS {
                        warn!("Max entry attempts ({}) reached for window {}", MAX_ENTRY_ATTEMPTS, current_ts);
                        telegram::notify(&shared_config, &format!("⚠️ Max retries reached for window {}", current_ts)).await;
                    }
                }
                let unmatched_signal = EvaluationResult {
                    rejection_reason: "unmatched_fok",
                    signal: None,
                    ..eval_result
                };
                db.insert_signal(&unmatched_signal, current_ts as i64, secs_left, dry_run);
                let msg = format!("⚠️ FOK not filled (order {})", fill.order_id);
                telegram::notify(&shared_config, &msg).await;
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                continue;
            }

            // Record trade with actual fill data
            let now = chrono::Utc::now().timestamp();
            let actual_cost = fill.fill_price * fill.filled_size;
            let trade = TradeRecord {
                timestamp: now,
                window_ts: current_ts as i64,
                slug: discovery::window_slug(current_ts),
                side: signal.side.clone(),
                btc_delta_pct: signal.btc_delta_pct,
                entry_price: fill.fill_price,
                shares: fill.filled_size,
                cost_usdc: actual_cost,
                secs_left: signal.secs_left,
                resolution: None,
                won: None,
                profit: None,
                order_id: Some(fill.order_id),
                dry_run,
                ask_price_observed: Some(signal.ask_price),
                bid_price_observed,
                spread_observed: Some(signal.spread),
                ask_depth: ask_depth_val,
                bid_depth: bid_depth_val,
                up_trade_count: Some(up_tc as i32),
                down_trade_count: Some(down_tc as i32),
                opposite_side_ask,
                binance_price_entry: bn_entry,
                binance_open_price: bn_open,
                rtds_price_entry: rtds_entry,
                rtds_open_price: rtds_open,
                rtds_stale_at_entry: Some(rtds_stale),
                trend_strength: trend_val,
                limit_price: Some(limit_price),
                fill_price: Some(fill.fill_price),
                fill_attempts: Some(failed_before as i32 + 1),
                signal_detected_ms: Some(signal_detected_ms),
                order_sent_ms: if dry_run { None } else { Some(order_sent_ms) },
                order_ack_ms: if dry_run { None } else { Some(order_ack_ms) },
                bot_version: BOT_VERSION.to_string(),
                neg_risk: neg_risk_val,
            };

            if let Err(e) = db.insert_trade(&trade) {
                error!("Failed to insert trade: {}", e);
            }

            db.insert_signal(&eval_result, current_ts as i64, secs_left, dry_run);

            // Mark entered
            {
                let mut ws = window_state.write().await;
                ws.entered = true;
            }

            // Notify Telegram
            let dry_tag = if dry_run { " [DRY]" } else { "" };
            let msg = format!(
                "🟢 BOUGHT {} {} @ ${:.2} | BTC Δ: {:+.3}% | {}s left{}",
                fill.filled_size, signal.side, fill.fill_price, signal.btc_delta_pct, signal.secs_left, dry_tag
            );
            telegram::notify(&shared_config, &msg).await;
        }

        // ── Handle resolution events ──
        while let Ok(event) = resolution_rx.try_recv() {
            info!("Resolution: {} (asset={})", event.winning_outcome, event.winning_asset_id);

            // Look up window_ts from the token→window map
            let maybe_window_ts = {
                let twm = token_window_map.read().await;
                twm.get(&event.winning_asset_id).copied()
            };

            let window_ts = match maybe_window_ts {
                Some(ts) => ts as i64,
                None => {
                    warn!(
                        "Resolution for unknown asset {}, cannot match to trade",
                        event.winning_asset_id
                    );
                    continue;
                }
            };

            // Find the trade for this specific window
            let trade = db.get_trade_by_window_ts(window_ts);

            if let Some(trade) = trade {
                if trade.resolution.is_some() {
                    info!("Trade for window {} already resolved, skipping", window_ts);
                    continue;
                }

                let won = trade.side == event.winning_outcome;
                let payout = if won { trade.shares * 1.0 } else { 0.0 };
                let profit = payout - trade.cost_usdc;

                if let Err(e) = db.resolve_trade_by_window_ts(
                    window_ts,
                    &event.winning_outcome,
                    won,
                    payout,
                    profit,
                ) {
                    error!("Failed to resolve trade: {}", e);
                }

                let cfg = shared_config.read().await;
                let dry_tag = if cfg.dry_run { " [DRY]" } else { "" };
                let msg = if won {
                    format!(
                        "✅ WIN +${:.2} ({} resolved){}",
                        profit, event.winning_outcome, dry_tag
                    )
                } else {
                    format!(
                        "❌ LOSS -${:.2} ({} resolved){}",
                        trade.cost_usdc, event.winning_outcome, dry_tag
                    )
                };
                telegram::notify(&shared_config, &msg).await;

                // ── Kill switch: check consecutive losses and daily P&L (live trades only) ──
                let consec_losses = db.get_recent_consecutive_losses();
                if consec_losses >= cfg.max_consecutive_losses {
                    warn!("Kill switch: {} consecutive live losses (limit {})", consec_losses, cfg.max_consecutive_losses);
                    let mut ws = window_state.write().await;
                    ws.paused = true;
                    drop(ws);

                    db.insert_kill_switch_event("consecutive_losses", Some(consec_losses), None);

                    let last3 = db.get_last_trades(3);
                    let mut details = String::new();
                    for t in &last3 {
                        let r = match t.won {
                            Some(true) => format!("W +${:.2}", t.profit.unwrap_or(0.0)),
                            Some(false) => format!("L -${:.2}", t.cost_usdc),
                            None => "pending".into(),
                        };
                        details.push_str(&format!("\n  {} {} {}", t.side, t.slug, r));
                    }
                    let alert = format!(
                        "🛑 AUTO-PAUSED: {} consecutive losses{}\nUse /resume to continue",
                        consec_losses, details
                    );
                    telegram::notify(&shared_config, &alert).await;
                }

                let today_stats = db.get_stats_today_live();
                if today_stats.net_pnl <= -cfg.daily_loss_limit_usdc {
                    let mut ws = window_state.write().await;
                    if !ws.paused {
                        warn!("Kill switch: daily live P&L ${:.2} exceeds limit -${:.2}", today_stats.net_pnl, cfg.daily_loss_limit_usdc);
                        ws.paused = true;
                        drop(ws);

                        db.insert_kill_switch_event("daily_limit", None, Some(today_stats.net_pnl));

                        let alert = format!(
                            "🛑 AUTO-PAUSED: Daily loss ${:.2} exceeds limit ${:.2}\nUse /resume to continue",
                            today_stats.net_pnl.abs(),
                            cfg.daily_loss_limit_usdc
                        );
                        telegram::notify(&shared_config, &alert).await;
                    }
                }
            }
        }

        // ── Tick interval ──
        tokio::time::sleep(std::time::Duration::from_millis(BOT_TICK_MS)).await;
    }

    info!("Bot shutdown complete");
}
