use std::sync::Arc;
use tracing::{error, info};

use crate::config::RuntimeConfig;
use crate::db::Database;
use crate::trading;
use crate::types::{EntrySignal, SharedBtcPrice, SharedConfig, SharedSdkClient, SharedWallet, SharedWindowState};

/// Send a message to the configured Telegram chat.
pub async fn send_message(config: &RuntimeConfig, text: &str) {
    let url = format!(
        "https://api.telegram.org/bot{}/sendMessage",
        config.telegram_bot_token
    );
    let client = reqwest::Client::new();
    let params = serde_json::json!({
        "chat_id": config.telegram_chat_id,
        "text": text,
        "parse_mode": "HTML",
    });
    match client.post(&url).json(&params).send().await {
        Ok(resp) => {
            if !resp.status().is_success() {
                error!("Telegram send failed: HTTP {}", resp.status());
            }
        }
        Err(e) => {
            error!("Telegram send error: {}", e);
        }
    }
}

/// Send a notification using shared config.
pub async fn notify(config: &SharedConfig, text: &str) {
    let cfg = config.read().await;
    send_message(&cfg, text).await;
}

/// Start the Telegram command polling loop.
#[allow(clippy::too_many_arguments)]
pub async fn run_telegram_bot(
    config: SharedConfig,
    btc_price: SharedBtcPrice,
    window_state: SharedWindowState,
    db: Arc<Database>,
    start_time: std::time::Instant,
    wallet: SharedWallet,
    http_client: reqwest::Client,
    sdk_client: Option<SharedSdkClient>,
) {
    let token = {
        let cfg = config.read().await;
        cfg.telegram_bot_token.clone()
    };

    let url = format!("https://api.telegram.org/bot{}/getUpdates", token);
    let client = reqwest::Client::new();
    let mut offset: i64 = 0;

    info!("Telegram bot polling started");

    loop {
        let params = serde_json::json!({
            "offset": offset,
            "timeout": 30,
            "allowed_updates": ["message"],
        });

        match client.post(&url).json(&params).send().await {
            Ok(resp) => {
                if let Ok(body) = resp.json::<serde_json::Value>().await {
                    if let Some(updates) = body["result"].as_array() {
                        for update in updates {
                            if let Some(update_id) = update["update_id"].as_i64() {
                                offset = update_id + 1;
                            }
                            handle_update(
                                update,
                                &config,
                                &btc_price,
                                &window_state,
                                &db,
                                start_time,
                                &wallet,
                                &http_client,
                                &sdk_client,
                            )
                            .await;
                        }
                    }
                }
            }
            Err(e) => {
                error!("Telegram poll error: {}", e);
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_update(
    update: &serde_json::Value,
    config: &SharedConfig,
    btc_price: &SharedBtcPrice,
    window_state: &SharedWindowState,
    db: &Arc<Database>,
    start_time: std::time::Instant,
    wallet: &SharedWallet,
    http_client: &reqwest::Client,
    sdk_client: &Option<SharedSdkClient>,
) {
    let message = match update.get("message") {
        Some(m) => m,
        None => return,
    };

    // Verify chat ID
    let chat_id = message["chat"]["id"].as_i64().unwrap_or(0);
    {
        let cfg = config.read().await;
        if chat_id != cfg.telegram_chat_id {
            return;
        }
    }

    let text = message["text"].as_str().unwrap_or("");
    let parts: Vec<&str> = text.split_whitespace().collect();
    let command = parts.first().copied().unwrap_or("");

    let response = match command {
        "/help" | "/start" => build_help(),
        "/status" => build_status(config, btc_price, window_state, start_time).await,
        "/stats" => build_stats(db).await,
        "/config" => build_config_display(config).await,
        "/set_threshold" => {
            if let Some(val) = parts.get(1).and_then(|v| v.parse::<f64>().ok()) {
                set_param(config, db, "btc_threshold_pct", val).await
            } else {
                "Usage: /set_threshold 0.08".into()
            }
        }
        "/set_maxask" => {
            if let Some(val) = parts.get(1).and_then(|v| v.parse::<f64>().ok()) {
                set_param(config, db, "max_ask_price", val).await
            } else {
                "Usage: /set_maxask 0.80".into()
            }
        }
        "/set_spread" => {
            if let Some(val) = parts.get(1).and_then(|v| v.parse::<f64>().ok()) {
                if !(0.0..=1.0).contains(&val) {
                    "Max spread must be between 0.00 and 1.00".into()
                } else {
                    set_param(config, db, "max_spread", val).await
                }
            } else {
                "Usage: /set_spread 0.15".into()
            }
        }
        "/set_shares" => {
            if let Some(val) = parts.get(1).and_then(|v| v.parse::<f64>().ok()) {
                if val < 5.0 {
                    "Minimum 5 shares".into()
                } else {
                    set_param(config, db, "bet_shares", val).await
                }
            } else {
                "Usage: /set_shares 10".into()
            }
        }
        "/set_momentum" => {
            if let Some(val) = parts.get(1).and_then(|v| v.parse::<f64>().ok()) {
                if !(0.0..=5.0).contains(&val) {
                    "Min delta momentum must be between 0.00 and 5.00 (0 disables)".into()
                } else {
                    set_param(config, db, "min_delta_momentum", val).await
                }
            } else {
                "Usage: /set_momentum 0.70".into()
            }
        }
        "/set_slippage" => {
            if let Some(val) = parts.get(1).and_then(|v| v.parse::<f64>().ok()) {
                if !(0.0..=0.20).contains(&val) {
                    "Slippage must be between 0.00 and 0.20".into()
                } else {
                    set_param(config, db, "max_slippage", val).await
                }
            } else {
                "Usage: /set_slippage 0.03".into()
            }
        }
        "/set_trend" => {
            if let Some(val) = parts.get(1).and_then(|v| v.parse::<f64>().ok()) {
                if !(0.0..=1.0).contains(&val) {
                    "Trend strength must be between 0.0 and 1.0".into()
                } else {
                    set_param(config, db, "min_trend_strength", val).await
                }
            } else {
                "Usage: /set_trend 0.20".into()
            }
        }
        "/dryrun" => {
            if let Some(mode) = parts.get(1) {
                let on = *mode == "on";
                let mut cfg = config.write().await;
                let old = cfg.dry_run;
                cfg.dry_run = on;
                db.log_config_change("dry_run", &old.to_string(), &on.to_string());
                format!("Dry run: {} → {}", old, on)
            } else {
                "Usage: /dryrun on|off".into()
            }
        }
        "/pause" => {
            let mut ws = window_state.write().await;
            ws.paused = true;
            "Trading paused. Monitoring continues.".into()
        }
        "/resume" => {
            let mut ws = window_state.write().await;
            ws.paused = false;
            drop(ws);
            db.mark_kill_switch_resumed();
            "Trading resumed.".into()
        }
        "/limits" => build_limits_display(config).await,
        "/last" => {
            let n = parts.get(1).and_then(|v| v.parse::<i64>().ok()).unwrap_or(5);
            build_last_trades(db, n).await
        }
        "/maker" => {
            let n = parts.get(1).and_then(|v| v.parse::<i64>().ok()).unwrap_or(200);
            build_maker_report(config, db, n).await
        }
        "/testorder" => build_testorder(&parts, config, wallet, http_client, sdk_client).await,
        _ => return, // Ignore unknown commands
    };

    let cfg = config.read().await;
    send_message(&cfg, &response).await;
}

async fn build_status(
    config: &SharedConfig,
    btc_price: &SharedBtcPrice,
    window_state: &SharedWindowState,
    start_time: std::time::Instant,
) -> String {
    let cfg = config.read().await;
    let btc = btc_price.read().await;
    let ws = window_state.read().await;
    let uptime = start_time.elapsed();
    let hours = uptime.as_secs() / 3600;
    let mins = (uptime.as_secs() % 3600) / 60;

    let btc_str = btc
        .current_price
        .map(|p| format!("${:.2}", p))
        .unwrap_or_else(|| "N/A".into());

    let delta_str = match (btc.current_price, btc.window_open_price) {
        (Some(curr), Some(open)) => {
            let d = ((curr - open) / open) * 100.0;
            format!("{:+.4}%", d)
        }
        _ => "N/A".into(),
    };

    let secs_left = crate::discovery::secs_remaining();

    // Health of the TWAP feed dependency over recent windows, broken down by
    // source so it is visible how much coverage leans on the fallback.
    let (twap_fresh, twap_samples, twap_rtds, twap_chainlink) = ws.twap_coverage();
    let coverage_str = if twap_samples == 0 {
        "no samples yet".to_string()
    } else {
        format!(
            "{}/{} (rtds {}, chainlink {})",
            twap_fresh, twap_samples, twap_rtds, twap_chainlink
        )
    };

    format!(
        "<b>Status</b>\n\
         Window: {} ({}s left)\n\
         BTC: {} (Δ: {})\n\
         Position: {}\n\
         TWAP coverage: {}\n\
         TWAP strike: {}\n\
         Dry run: {}\n\
         Paused: {}\n\
         Uptime: {}h {}m",
        ws.window_ts,
        secs_left,
        btc_str,
        delta_str,
        if ws.entered { "IN" } else { "WAITING" },
        coverage_str,
        ws.twap_strike
            .as_deref()
            .and_then(crate::types::format_e18)
            .unwrap_or_else(|| "N/A".into()),
        cfg.dry_run,
        ws.paused,
        hours,
        mins,
    )
}

async fn build_stats(db: &Arc<Database>) -> String {
    let today = db.get_stats_today();
    let week = db.get_stats_week();
    let all = db.get_stats_all();

    format!(
        "<b>Trading Stats</b>\n\n\
         <b>Today:</b>\n\
         Trades: {} | W: {} L: {} | WR: {:.0}%\n\
         Cost: ${:.2} | Payout: ${:.2} | P&amp;L: ${:.2}\n\n\
         <b>This Week:</b>\n\
         Trades: {} | W: {} L: {} | WR: {:.0}%\n\
         P&amp;L: ${:.2}\n\n\
         <b>All Time:</b>\n\
         Trades: {} | W: {} L: {} | WR: {:.0}%\n\
         P&amp;L: ${:.2}",
        today.trades, today.wins, today.losses, today.win_rate,
        today.total_cost, today.total_payout, today.net_pnl,
        week.trades, week.wins, week.losses, week.win_rate, week.net_pnl,
        all.trades, all.wins, all.losses, all.win_rate, all.net_pnl,
    )
}

fn build_help() -> String {
    "<b>Commands</b>\n\
     /help — this list\n\
     /status — window, BTC price, position, uptime\n\
     /stats — today / week / all-time P&amp;L\n\
     /config — current strategy params\n\
     /limits — risk limits (consec losses, daily cap)\n\
     /last [N] — last N trades (default 5)\n\
     /maker [N] — shadow-quoting adverse selection (default 200 rows)\n\
     \n\
     <b>Parameters</b>\n\
     /set_threshold &lt;pct&gt; — BTC delta threshold (e.g. 0.08)\n\
     /set_maxask &lt;price&gt; — max ask to enter (e.g. 0.80)\n\
     /set_spread &lt;val&gt; — max bid-ask spread (e.g. 0.15)\n\
     /set_shares &lt;n&gt; — bet size in shares (min 5)\n\
     /set_slippage &lt;val&gt; — limit = ask + slippage (0.00–0.20)\n\
     /set_trend &lt;val&gt; — min trend strength (0.0–1.0)\n\
     /set_momentum &lt;val&gt; — min delta momentum (0.0–5.0, 0 disables)\n\
     \n\
     <b>Control</b>\n\
     /pause — stop entering new trades\n\
     /resume — resume trading\n\
     /dryrun on|off — toggle dry-run mode\n\
     /testorder &lt;token_id&gt; &lt;price&gt; &lt;shares&gt; — fire a test FAK order"
        .into()
}

async fn build_config_display(config: &SharedConfig) -> String {
    let cfg = config.read().await;
    format!(
        "<b>Configuration</b>\n\
         BTC threshold: {:.2}%\n\
         Max ask price: ${:.2}\n\
         Max spread: ${:.2}\n\
         Bet shares: {:.0}\n\
         Max slippage: ${:.2}\n\
         Min trend strength: {:.2}\n\
         Min delta momentum: {:.2}\n\
         Dry run: {}\n\
         Has credentials: {}",
        cfg.btc_threshold_pct,
        cfg.max_ask_price,
        cfg.max_spread,
        cfg.bet_shares,
        cfg.max_slippage,
        cfg.min_trend_strength,
        cfg.min_delta_momentum,
        cfg.dry_run,
        cfg.has_trading_credentials(),
    )
}

async fn build_limits_display(config: &SharedConfig) -> String {
    let cfg = config.read().await;
    format!(
        "<b>Risk Limits</b>\n\
         Max consecutive losses: {}\n\
         Daily loss limit: ${:.2}",
        cfg.max_consecutive_losses, cfg.daily_loss_limit_usdc,
    )
}

async fn set_param(config: &SharedConfig, db: &Arc<Database>, param: &str, val: f64) -> String {
    let mut cfg = config.write().await;
    let old = match param {
        "btc_threshold_pct" => {
            let old = cfg.btc_threshold_pct;
            cfg.btc_threshold_pct = val;
            old
        }
        "max_ask_price" => {
            let old = cfg.max_ask_price;
            cfg.max_ask_price = val;
            old
        }
        "max_spread" => {
            let old = cfg.max_spread;
            cfg.max_spread = val;
            old
        }
        "bet_shares" => {
            let old = cfg.bet_shares;
            cfg.bet_shares = val;
            old
        }
        "max_slippage" => {
            let old = cfg.max_slippage;
            cfg.max_slippage = val;
            old
        }
        "min_trend_strength" => {
            let old = cfg.min_trend_strength;
            cfg.min_trend_strength = val;
            old
        }
        "min_delta_momentum" => {
            let old = cfg.min_delta_momentum;
            cfg.min_delta_momentum = val;
            old
        }
        _ => return format!("Unknown parameter: {}", param),
    };
    db.log_config_change(param, &format!("{:.4}", old), &format!("{:.4}", val));
    format!("{}: {:.4} → {:.4}", param, old, val)
}

async fn build_last_trades(db: &Arc<Database>, n: i64) -> String {
    let trades = db.get_last_trades(n);
    if trades.is_empty() {
        return "No trades yet.".into();
    }

    let mut lines = vec![format!("<b>Last {} Trades</b>\n", trades.len())];
    for t in &trades {
        let result = match t.won {
            Some(true) => format!("✅ +${:.2}", t.profit.unwrap_or(0.0)),
            Some(false) => format!("❌ -${:.2}", t.cost_usdc),
            None => "⏳ pending".into(),
        };
        let dry = if t.dry_run { " [DRY]" } else { "" };
        lines.push(format!(
            "{} {} @ ${:.2} | Δ{:+.3}% | {}s | {}{}",
            t.side, t.shares, t.entry_price, t.btc_delta_pct, t.secs_left, result, dry
        ));
    }
    lines.join("\n")
}

async fn build_testorder(
    parts: &[&str],
    config: &SharedConfig,
    wallet: &SharedWallet,
    _http_client: &reqwest::Client,
    sdk_client: &Option<SharedSdkClient>,
) -> String {
    const USAGE: &str = "Usage: /testorder <token_id> <price> <shares>\nExample: /testorder 11477763... 0.05 5";

    let (token_id, price, shares) = match (parts.get(1), parts.get(2), parts.get(3)) {
        (Some(t), Some(p), Some(s)) => {
            let price: f64 = match p.parse() {
                Ok(v) if (0.01..=0.99).contains(&v) => v,
                _ => return USAGE.into(),
            };
            let shares: f64 = match s.parse() {
                Ok(v) if v >= 5.0 => v,
                _ => return USAGE.into(),
            };
            (t.to_string(), price, shares)
        }
        _ => return USAGE.into(),
    };

    let signal = EntrySignal {
        side: "TestBuy".into(),
        token_id: token_id.clone(),
        btc_delta_pct: 0.0,
        ask_price: price,
        spread: 0.0,
        secs_left: 0,
    };

    let cfg = config.read().await;

    if cfg.dry_run {
        let fill = trading::simulate_trade(&signal, shares);
        info!(
            "TEST ORDER: token={} price={} shares={} result=simulated order_id={}",
            token_id, price, shares, fill.order_id
        );
        return format!(
            "✅ TEST ORDER (DRY RUN): order_id={} fill_price=${:.4} filled={:.2} shares",
            fill.order_id, fill.fill_price, fill.filled_size
        );
    }

    let sdk = match sdk_client.as_ref() {
        Some(c) => c,
        None => return "SDK client not available (dry-run mode or no credentials)".into(),
    };

    match trading::place_fak_buy_raw(
        sdk, &cfg, wallet, &signal, price, shares, "0.01", false,
    )
    .await
    {
        Ok(fill) if fill.fill_price == 0.0 || fill.filled_size == 0.0 => {
            info!(
                "TEST ORDER: token={} price={} shares={} result=unmatched order_id={}",
                token_id, price, shares, fill.order_id
            );
            format!("⚠️ TEST ORDER UNMATCHED: order_id={}", fill.order_id)
        }
        Ok(fill) => {
            info!(
                "TEST ORDER: token={} price={} shares={} result=filled order_id={}",
                token_id, price, shares, fill.order_id
            );
            format!(
                "✅ TEST ORDER: order_id={} fill_price=${:.4} filled={:.2} shares",
                fill.order_id, fill.fill_price, fill.filled_size
            )
        }
        Err(e) => {
            info!(
                "TEST ORDER: token={} price={} shares={} result=error msg={}",
                token_id, price, shares, e
            );
            format!("❌ TEST ORDER FAILED: {}", e)
        }
    }
}

/// Send daily summary at midnight UTC.
/// PHASE 2 shadow-quoting report. Reads `maker_shadow` only; sends no orders.
///
/// The headline number is the bid-fill hit rate. If simulated bid fills land on
/// the losing side materially more often than fair value implies, we are being
/// adversely selected and market making does not work at this latency — that is
/// the Phase 3 gate.
async fn build_maker_report(config: &SharedConfig, db: &Arc<Database>, n: i64) -> String {
    let (mode, half_spread, fv_path) = {
        let cfg = config.read().await;
        (
            cfg.maker_mode.as_str(),
            cfg.maker_half_spread,
            cfg.fairvalue_path.clone(),
        )
    };

    if mode == "off" {
        return format!(
            "<b>Maker shadow</b>\n\
             MAKER_MODE=off — no shadow rows are being written.\n\
             Set MAKER_MODE=shadow (FAIRVALUE_PATH={}) to start measuring.",
            fv_path
        );
    }

    let s = db.maker_shadow_summary(n);
    let pending = db.maker_shadow_pending();

    // Sampling coverage. Reported before the fill stats because it decides
    // whether they mean anything: maker_shadow is only written where the
    // CURRENT curve already prices, so it can never reveal a coverage hole.
    // delta_samples is unfiltered, and its mid-band count is the number that
    // says whether the next refit can close one.
    let cov = db.delta_sample_coverage();
    let mid_pct = if cov.rows > 0 {
        format!("{:.1}%", 100.0 * cov.mid_band_rows as f64 / cov.rows as f64)
    } else {
        "n/a".into()
    };
    let sampling = format!(
        "<b>Unconditional samples</b>\n\
         rows: {} ({} labelled) over {} windows ({} settled)\n\
         uncertain band |Δ|&lt;0.05: {} rows ({}), {} labelled\n\
         (that band is what the signals-based fit had none of)",
        cov.rows, cov.resolved_rows, cov.windows, cov.resolved_windows,
        cov.mid_band_rows, mid_pct, cov.mid_band_resolved,
    );

    if s.rows == 0 {
        return format!(
            "<b>Maker shadow</b> (mode={}, half-spread={:.3})\n\
             No RESOLVED shadow rows yet. {} row(s) awaiting resolution.\n\
             Rows are labelled when their window settles (~2-3 min after close).\n\n\
             {}",
            mode, half_spread, pending, sampling
        );
    }

    let pct = |num: i64, den: i64| -> String {
        if den == 0 {
            "n/a".to_string()
        } else {
            format!("{:.1}%", 100.0 * num as f64 / den as f64)
        }
    };

    let span = match (s.first_ts_ms, s.last_ts_ms) {
        (Some(a), Some(b)) => format!("{:.1}h", (b - a) as f64 / 3_600_000.0),
        _ => "n/a".into(),
    };

    let divergence = match s.mean_abs_fv_minus_mid {
        Some(v) => format!("{:.4}", v),
        None => "n/a (no two-sided book)".into(),
    };

    format!(
        "<b>Maker shadow</b> (mode={}, half-spread={:.3})\n\
         NO ORDERS SENT — measurement only.\n\n\
         Resolved rows: {} (span {}), pending {}\n\n\
         <b>Simulated BID fills</b> (we would have BOUGHT)\n\
         fills: {} of {} rows ({})\n\
         on winning side: {} ({})\n\n\
         <b>Simulated ASK fills</b> (we would have SOLD)\n\
         fills: {} of {} rows ({})\n\
         side went on to win: {} ({}) — high is bad, we sold the winner\n\n\
         <b>Model vs market</b>\n\
         mean |fair_value - mid|: {}\n\
         (large and persistent means the model is wrong, not the market)\n\n\
         {}",
        mode,
        half_spread,
        s.rows,
        span,
        pending,
        s.bid_fills,
        s.rows,
        pct(s.bid_fills, s.rows),
        s.bid_fills_on_winner,
        pct(s.bid_fills_on_winner, s.bid_fills),
        s.ask_fills,
        s.rows,
        pct(s.ask_fills, s.rows),
        s.ask_fills_on_winner,
        pct(s.ask_fills_on_winner, s.ask_fills),
        divergence,
        sampling,
    )
}

pub async fn send_daily_summary(config: &SharedConfig, db: &Arc<Database>) {
    let stats = db.get_stats_today();
    let text = format!(
        "📊 <b>Daily Summary</b>\n\
         Trades: {} | Wins: {} | Losses: {}\n\
         P&amp;L: ${:.2} | Win rate: {:.0}%",
        stats.trades, stats.wins, stats.losses, stats.net_pnl, stats.win_rate,
    );
    notify(config, &text).await;
}
