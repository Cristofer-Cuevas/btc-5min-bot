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
        "/set_foklimit" => {
            if let Some(val) = parts.get(1).and_then(|v| v.parse::<f64>().ok()) {
                if val <= 0.05 || val >= 1.00 {
                    "FOK limit price must be > 0.05 and < 1.00".into()
                } else {
                    set_param(config, db, "fok_limit_price", val).await
                }
            } else {
                "Usage: /set_foklimit 0.85".into()
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

    format!(
        "<b>Status</b>\n\
         Window: {} ({}s left)\n\
         BTC: {} (Δ: {})\n\
         Position: {}\n\
         Dry run: {}\n\
         Paused: {}\n\
         Uptime: {}h {}m",
        ws.window_ts,
        secs_left,
        btc_str,
        delta_str,
        if ws.entered { "IN" } else { "WAITING" },
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

async fn build_config_display(config: &SharedConfig) -> String {
    let cfg = config.read().await;
    format!(
        "<b>Configuration</b>\n\
         BTC threshold: {:.2}%\n\
         Max ask price: ${:.2}\n\
         Max spread: ${:.2}\n\
         Bet shares: {:.0}\n\
         FOK limit price: ${:.2}\n\
         Dry run: {}\n\
         Has credentials: {}",
        cfg.btc_threshold_pct,
        cfg.max_ask_price,
        cfg.max_spread,
        cfg.bet_shares,
        cfg.fok_limit_price,
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
        "bet_shares" => {
            let old = cfg.bet_shares;
            cfg.bet_shares = val;
            old
        }
        "fok_limit_price" => {
            let old = cfg.fok_limit_price;
            cfg.fok_limit_price = val;
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

    match trading::place_fok_buy_raw(
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
