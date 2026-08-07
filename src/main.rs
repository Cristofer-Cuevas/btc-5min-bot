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

/// DATA COLLECTION ONLY. Returns the current TWAP reading only if it is fresh,
/// otherwise `None` so the caller writes NULL.
///
/// The RTDS TWAP feed goes silent for long stretches with no backfill, so the
/// last-known value can be hours old; writing it would record a stale price as
/// if it were the window's. Each capture point calls this at most once per
/// window, so the warning is inherently rate-limited to once per window per
/// capture point.
async fn fresh_twap_for_capture(
    btc_price: &SharedBtcPrice,
    now_ms: i64,
    capture_point: &str,
) -> Option<(String, i64)> {
    let btc = btc_price.read().await;
    if let Some(fresh) = btc.fresh_twap_30(now_ms, TWAP_MAX_AGE_MS) {
        return Some(fresh);
    }
    match btc.twap_30_observed_at_ms {
        Some(obs) => {
            let age_secs = now_ms.saturating_sub(obs) / 1000;
            warn!(
                "TWAP stale at {}: last observed {}s ago, storing NULL",
                capture_point, age_secs
            );
        }
        None => warn!(
            "TWAP stale at {}: no reading received yet, storing NULL",
            capture_point
        ),
    }
    None
}

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
        let now_secs = chrono::Utc::now().timestamp();
        let now_ms = chrono::Utc::now().timestamp_millis();

        // ── DATA COLLECTION ONLY: close capture at the WINDOW BOUNDARY ──
        // Fires on the first tick at/after window_ts + WINDOW_SECS, i.e. the
        // moment the outgoing window ends. Deliberately NOT driven by the
        // market_resolved event, which arrives minutes late and with variable
        // delay. Runs before the rotation block below so `last_window_ts` still
        // names the window that just closed — the row written is the outgoing
        // window's, not the new one's.
        //
        // Changes no trading state and no control flow.
        if last_window_ts != 0 && now_secs >= last_window_ts as i64 + WINDOW_SECS {
            let needs_capture = !window_state.read().await.resolve_captured;
            if needs_capture {
                let closing_ts = last_window_ts as i64;
                let fresh = fresh_twap_for_capture(&btc_price, now_ms, "resolve").await;
                let snapshot_close = btc_price.read().await.current_price;

                db.record_twap_at_resolve(
                    closing_ts,
                    fresh.as_ref().map(|(v, _)| v.as_str()),
                    fresh.as_ref().map(|(_, obs)| *obs),
                    snapshot_close,
                    now_ms,
                );

                window_state.write().await.resolve_captured = true;

                info!(
                    "Boundary close capture: window={} captured_at_ms={} \
                     (boundary={}) snapshot_close={} twap_close={}",
                    closing_ts,
                    now_ms,
                    (closing_ts + WINDOW_SECS) * 1000,
                    snapshot_close
                        .map(|p| format!("{:.2}", p))
                        .unwrap_or_else(|| "N/A".into()),
                    fresh
                        .as_ref()
                        .and_then(|(v, _)| format_e18(v))
                        .unwrap_or_else(|| "N/A".into()),
                );
            }
        }

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
                ws.pending_retry_signal = None;
                ws.last_attempt_failed_at_ms = None;
                // DATA COLLECTION ONLY: arm both captures for the new window.
                ws.open_captured = false;
                ws.resolve_captured = false;
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

                    // DATA COLLECTION ONLY: remember which tokens this bot
                    // believes belong to this window, so a resolution event
                    // naming a different asset can be identified as a mapping
                    // bug rather than a capture-timing problem.
                    db.record_expected_tokens(
                        current_ts as i64,
                        &market.up_token_id,
                        &market.down_token_id,
                    );

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

        // ── DATA COLLECTION ONLY: open capture, at or after window_ts ──
        // Never before the strike: a reading taken even a moment early belongs
        // to the previous window. Rotation is driven by a floored clock so it
        // cannot fire early in practice, but if a tick ever arrives before the
        // boundary the capture defers to the next one that qualifies. The
        // `open_captured` flag makes it exactly once per window.
        //
        // Reads `current_price` (the live tick at the boundary) rather than
        // `window_open_price`, which the trading path sets on rotation and is
        // left completely untouched here.
        if !window_state.read().await.open_captured && now_secs >= current_ts as i64 {
            let fresh = fresh_twap_for_capture(&btc_price, now_ms, "open").await;
            let snapshot_open = btc_price.read().await.current_price;

            db.record_twap_at_open(
                current_ts as i64,
                fresh.as_ref().map(|(v, _)| v.as_str()),
                fresh.as_ref().map(|(_, obs)| *obs),
                snapshot_open,
                now_ms,
            );

            window_state.write().await.open_captured = true;

            info!(
                "Boundary open capture: window={} captured_at_ms={} \
                 (boundary={}) snapshot_open={} twap_open={}",
                current_ts,
                now_ms,
                current_ts as i64 * 1000,
                snapshot_open
                    .map(|p| format!("{:.2}", p))
                    .unwrap_or_else(|| "N/A".into()),
                fresh
                    .as_ref()
                    .and_then(|(v, _)| format_e18(v))
                    .unwrap_or_else(|| "N/A".into()),
            );
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

        // ── Retry check (before strategy evaluation) ──
        // If a prior attempt left a pending retry signal and conditions allow,
        // build a refreshed signal from CURRENT book state and use it instead
        // of running strategy. Retry takes precedence over strategy evaluation.
        let retry_signal_opt: Option<EntrySignal> = {
            let pending = {
                let ws = window_state.read().await;
                ws.pending_retry_signal.clone()
            };
            if let Some(pending) = pending {
                let now_ms = chrono::Utc::now().timestamp_millis();
                let (cooldown_elapsed, attempts, entered, paused) = {
                    let ws = window_state.read().await;
                    let cooldown_elapsed = match ws.last_attempt_failed_at_ms {
                        Some(t) => now_ms - t >= 3000,
                        None => true,
                    };
                    (cooldown_elapsed, ws.failed_attempts, ws.entered, ws.paused)
                };
                let resolved = market_state.read().await.resolved;

                let can_retry = cooldown_elapsed
                    && secs_left >= 30
                    && attempts < MAX_ENTRY_ATTEMPTS
                    && !entered
                    && !paused
                    && !resolved;

                if !can_retry {
                    if attempts >= MAX_ENTRY_ATTEMPTS {
                        warn!(
                            "Retry budget exhausted ({}), abandoning entry for window {}",
                            MAX_ENTRY_ATTEMPTS, current_ts
                        );
                        let mut ws = window_state.write().await;
                        ws.pending_retry_signal = None;
                    }
                    None
                } else {
                    // Re-derive limit price from CURRENT book — ask may have moved.
                    let (current_ask_opt, current_spread) = {
                        let ms = market_state.read().await;
                        let book = if pending.side == "Up" {
                            &ms.up_book
                        } else {
                            &ms.down_book
                        };
                        (book.best_ask, book.spread().unwrap_or(0.0))
                    };
                    let max_ask_price = shared_config.read().await.max_ask_price;

                    match current_ask_opt {
                        None => None, // book has no ask this tick; try next tick
                        Some(ask) if ask >= max_ask_price => {
                            info!(
                                "Retry abandoned: market moved past MAX_ASK_PRICE ({:.2} >= {:.2})",
                                ask, max_ask_price
                            );
                            let mut ws = window_state.write().await;
                            ws.pending_retry_signal = None;
                            None
                        }
                        Some(ask) => {
                            info!(
                                "Retrying entry (attempt {}/{}): {} @ current ask ${:.2}",
                                attempts + 1,
                                MAX_ENTRY_ATTEMPTS,
                                pending.side,
                                ask
                            );
                            Some(EntrySignal {
                                side: pending.side.clone(),
                                token_id: pending.token_id.clone(),
                                btc_delta_pct: pending.btc_delta_pct,
                                ask_price: ask,
                                spread: current_spread,
                                secs_left,
                            })
                        }
                    }
                }
            } else {
                None
            }
        };

        // ── Strategy evaluation (skipped if retry took precedence) ──
        let signal_detected_ms = chrono::Utc::now().timestamp_millis();
        let is_retry = retry_signal_opt.is_some();

        let (eval_result, dry_run_flag) = if let Some(retry_sig) = retry_signal_opt {
            let dry_run = shared_config.read().await.dry_run;
            let r = EvaluationResult {
                signal: Some(retry_sig.clone()),
                rejection_reason: "entered",
                btc_delta_pct: Some(retry_sig.btc_delta_pct),
                ask_price: Some(retry_sig.ask_price),
                bid_price: None,
                spread: Some(retry_sig.spread),
                ask_depth: None,
                trade_count: None,
                trend_strength: None,
                side: Some(retry_sig.side),
            };
            (r, dry_run)
        } else {
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
            if !is_retry {
                info!(
                    "ENTRY SIGNAL: {} | BTC Δ: {:+.4}% | Ask: ${:.2} | Spread: ${:.2} | {}s left",
                    signal.side, signal.btc_delta_pct, signal.ask_price, signal.spread, signal.secs_left
                );
            }

            let cfg = shared_config.read().await;
            let shares = cfg.bet_shares;
            let dry_run = cfg.dry_run;

            // Capture market snapshot for the trade record
            let (
                bid_price_observed, ask_depth_val, bid_depth_val,
                up_tc, down_tc, opposite_side_ask, neg_risk_val, tick_size_val,
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
                    market.tick_size.clone(),
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

            // DATA COLLECTION ONLY: recorded after the order is placed, never
            // consulted when deciding to place it. Stale readings become NULL.
            let twap_entry = fresh_twap_for_capture(
                &btc_price,
                chrono::Utc::now().timestamp_millis(),
                "entry",
            )
            .await;

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
                let sdk = sdk_client.as_ref().expect("SDK client required for live trading");
                match trading::place_fak_buy(
                    sdk,
                    &cfg,
                    &wallet,
                    signal,
                    shares,
                    &tick_size_val,
                    neg_risk_val,
                )
                .await
                {
                    Ok(fill) => fill,
                    Err(e) => {
                        error!("Order placement failed: {}", e);
                        let retriable = trading::is_retriable_error(&e);
                        let now_ms = chrono::Utc::now().timestamp_millis();
                        let max_reached = {
                            let mut ws = window_state.write().await;
                            ws.failed_attempts += 1;
                            ws.last_attempt_failed_at_ms = Some(now_ms);
                            if retriable && ws.failed_attempts < MAX_ENTRY_ATTEMPTS {
                                ws.pending_retry_signal = Some(signal.clone());
                            } else {
                                ws.pending_retry_signal = None;
                            }
                            ws.failed_attempts >= MAX_ENTRY_ATTEMPTS
                        };
                        if max_reached {
                            warn!("Max entry attempts ({}) reached for window {}", MAX_ENTRY_ATTEMPTS, current_ts);
                            telegram::notify(&shared_config, &format!("⚠️ Max retries reached for window {}", current_ts)).await;
                        }
                        let failed_signal = EvaluationResult {
                            rejection_reason: "failed_fak",
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

            // If FAK didn't fill at all (zero shares), don't record a trade.
            // Any partial fill (filled_size > 0) flows through normal recording below.
            if fill.filled_size == 0.0 {
                warn!("FAK order {} did not fill, not recording trade", fill.order_id);
                let now_ms = chrono::Utc::now().timestamp_millis();
                let max_reached = {
                    let mut ws = window_state.write().await;
                    ws.failed_attempts += 1;
                    ws.last_attempt_failed_at_ms = Some(now_ms);
                    if ws.failed_attempts < MAX_ENTRY_ATTEMPTS {
                        ws.pending_retry_signal = Some(signal.clone());
                    } else {
                        ws.pending_retry_signal = None;
                    }
                    ws.failed_attempts >= MAX_ENTRY_ATTEMPTS
                };
                if max_reached {
                    warn!("Max entry attempts ({}) reached for window {}", MAX_ENTRY_ATTEMPTS, current_ts);
                    telegram::notify(&shared_config, &format!("⚠️ Max retries reached for window {}", current_ts)).await;
                }
                let unmatched_signal = EvaluationResult {
                    rejection_reason: "unmatched_fak",
                    signal: None,
                    ..eval_result
                };
                db.insert_signal(&unmatched_signal, current_ts as i64, secs_left, dry_run);
                let msg = format!("⚠️ FAK not filled (order {})", fill.order_id);
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

            // DATA COLLECTION ONLY: TWAP reading captured when the order was
            // placed. NULL if the feed had not produced a reading yet.
            db.record_twap_at_entry(
                current_ts as i64,
                twap_entry.as_ref().map(|(v, _)| v.as_str()),
                twap_entry.as_ref().map(|(_, obs)| *obs),
                &signal.side,
            );

            db.insert_signal(&eval_result, current_ts as i64, secs_left, dry_run);

            // Mark entered, clear any pending retry intent
            {
                let mut ws = window_state.write().await;
                ws.entered = true;
                ws.pending_retry_signal = None;
            }

            if is_retry {
                let attempts = window_state.read().await.failed_attempts;
                info!(
                    "Retry filled on attempt {}/{}",
                    attempts + 1,
                    MAX_ENTRY_ATTEMPTS
                );
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

            // ── DATA COLLECTION ONLY: TWAP vs snapshot comparison ──
            // Recorded before the trade branch below so windows the bot passed
            // on are captured too. Reads no trading state and changes no
            // control flow; the resolution handling below is untouched.
            let already_resolved = trade
                .as_ref()
                .map(|t| t.resolution.is_some())
                .unwrap_or(false);

            if !already_resolved {
                // The close values were captured at the window boundary, not
                // here — the resolution event arrives late and with variable
                // delay. Read them back rather than re-sampling live state.
                let (twap_resolve, snapshot_resolve) = db.get_twap_closes(window_ts);

                db.record_resolution_diagnostics(
                    window_ts,
                    &event.winning_outcome,
                    &event.winning_asset_id,
                    chrono::Utc::now().timestamp_millis(),
                );

                // If the resolved asset is neither token this bot recorded for
                // the window, the token→window mapping attributed it wrongly —
                // a different failure from a mistimed capture, and one that
                // would otherwise masquerade as a bad prediction.
                let (up_token, down_token) = db.get_expected_tokens(window_ts);
                let matches_known = [up_token.as_deref(), down_token.as_deref()]
                    .iter()
                    .flatten()
                    .any(|t| *t == event.winning_asset_id);
                if !matches_known {
                    error!(
                        "Resolution asset {} matches neither token for window {} (up={}, down={})",
                        event.winning_asset_id,
                        window_ts,
                        up_token.as_deref().unwrap_or("N/A"),
                        down_token.as_deref().unwrap_or("N/A"),
                    );
                }

                let snapshot_close = snapshot_resolve
                    .map(|p| format!("{:.2}", p))
                    .unwrap_or_else(|| "N/A".into());
                let twap_close = twap_resolve
                    .as_deref()
                    .and_then(format_e18)
                    .unwrap_or_else(|| "N/A".into());
                let predicted_side = trade
                    .as_ref()
                    .map(|t| t.side.clone())
                    .unwrap_or_else(|| "N/A".into());
                let matched = trade
                    .as_ref()
                    .map(|t| (t.side == event.winning_outcome).to_string())
                    .unwrap_or_else(|| "N/A".into());

                info!(
                    "TWAP compare: window={} snapshot_close={} twap_close={} \
                     my_predicted={} actual_resolution={} match={}",
                    window_ts,
                    snapshot_close,
                    twap_close,
                    predicted_side,
                    event.winning_outcome,
                    matched
                );
            }

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
