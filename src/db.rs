use rusqlite::{params, Connection, Result as SqlResult, Row};
use std::sync::Mutex;
use tracing::{error, info};

use crate::types::{EvaluationResult, TradeRecord, TradingStats};

pub struct Database {
    conn: Mutex<Connection>,
}

const TRADE_SELECT_COLS: &str = "\
    timestamp, window_ts, slug, side, btc_delta_pct, \
    entry_price, shares, cost_usdc, secs_left, \
    resolution, won, payout, profit, resolved_at, order_id, dry_run, \
    ask_price_observed, bid_price_observed, spread_observed, \
    ask_depth, bid_depth, up_trade_count, down_trade_count, \
    opposite_side_ask, \
    binance_price_entry, binance_open_price, \
    rtds_price_entry, rtds_open_price, rtds_stale_at_entry, \
    trend_strength, \
    limit_price, fill_price, fill_attempts, \
    signal_detected_ms, order_sent_ms, order_ack_ms, \
    bot_version, neg_risk";

fn read_trade_row(row: &Row) -> rusqlite::Result<TradeRecord> {
    let won_int: Option<i32> = row.get(10)?;
    let dry_run_int: i32 = row.get(15)?;
    let rtds_stale_int: Option<i32> = row.get(28)?;
    let neg_risk_int: Option<i32> = row.get(37)?;
    Ok(TradeRecord {
        timestamp: row.get(0)?,
        window_ts: row.get(1)?,
        slug: row.get(2)?,
        side: row.get(3)?,
        btc_delta_pct: row.get(4)?,
        entry_price: row.get(5)?,
        shares: row.get(6)?,
        cost_usdc: row.get(7)?,
        secs_left: row.get(8)?,
        resolution: row.get(9)?,
        won: won_int.map(|v| v == 1),
        // col 11 = payout (kept in SQL for DB integrity, not loaded)
        profit: row.get(12)?,
        // col 13 = resolved_at (kept in SQL for DB integrity, not loaded)
        order_id: row.get(14)?,
        dry_run: dry_run_int == 1,
        ask_price_observed: row.get(16)?,
        bid_price_observed: row.get(17)?,
        spread_observed: row.get(18)?,
        ask_depth: row.get(19)?,
        bid_depth: row.get(20)?,
        up_trade_count: row.get(21)?,
        down_trade_count: row.get(22)?,
        opposite_side_ask: row.get(23)?,
        binance_price_entry: row.get(24)?,
        binance_open_price: row.get(25)?,
        rtds_price_entry: row.get(26)?,
        rtds_open_price: row.get(27)?,
        rtds_stale_at_entry: rtds_stale_int.map(|v| v == 1),
        trend_strength: row.get(29)?,
        limit_price: row.get(30)?,
        fill_price: row.get(31)?,
        fill_attempts: row.get(32)?,
        signal_detected_ms: row.get(33)?,
        order_sent_ms: row.get(34)?,
        order_ack_ms: row.get(35)?,
        bot_version: row.get::<_, Option<String>>(36)?.unwrap_or_else(|| "unknown".into()),
        neg_risk: neg_risk_int.map(|v| v == 1).unwrap_or(false),
    })
}

impl Database {
    pub fn new(path: &str) -> SqlResult<Self> {
        let conn = Connection::open(path)?;
        let db = Self {
            conn: Mutex::new(conn),
        };
        db.init_tables()?;
        Ok(db)
    }

    fn init_tables(&self) -> SqlResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS trades (
                id              INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp       INTEGER NOT NULL,
                window_ts       INTEGER NOT NULL,
                slug            TEXT NOT NULL,
                side            TEXT NOT NULL,
                btc_delta_pct   REAL NOT NULL,
                entry_price     REAL NOT NULL,
                shares          REAL NOT NULL,
                cost_usdc       REAL NOT NULL,
                secs_left       INTEGER NOT NULL,
                resolution      TEXT,
                won             INTEGER,
                payout          REAL,
                profit          REAL,
                resolved_at     INTEGER,
                order_id        TEXT,
                dry_run         INTEGER DEFAULT 0
            );

            CREATE TABLE IF NOT EXISTS daily_stats (
                date            TEXT PRIMARY KEY,
                trades          INTEGER DEFAULT 0,
                wins            INTEGER DEFAULT 0,
                losses          INTEGER DEFAULT 0,
                total_cost      REAL DEFAULT 0,
                total_payout    REAL DEFAULT 0,
                net_pnl         REAL DEFAULT 0,
                win_rate        REAL DEFAULT 0
            );

            CREATE TABLE IF NOT EXISTS config_log (
                id              INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp       INTEGER NOT NULL,
                param           TEXT NOT NULL,
                old_value       TEXT,
                new_value       TEXT NOT NULL,
                changed_by      TEXT DEFAULT 'telegram'
            );

            -- rejection_reason values:
            --   entered, below_threshold, choppy, rtds_mismatch, ask_too_high,
            --   ask_too_low, spread_wide, trade_count_low, depth_low, too_late,
            --   too_early, market_resolved, max_attempts, failed_fok, unmatched_fok
            CREATE TABLE IF NOT EXISTS signals (
                id              INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp_ms    INTEGER NOT NULL,
                window_ts       INTEGER NOT NULL,
                secs_left       INTEGER NOT NULL,
                btc_delta_pct   REAL,
                ask_price       REAL,
                bid_price       REAL,
                spread          REAL,
                ask_depth       REAL,
                trade_count     INTEGER,
                trend_strength  REAL,
                side            TEXT,
                rejection_reason TEXT NOT NULL,
                dry_run         INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS kill_switch_events (
                id                  INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp           INTEGER NOT NULL,
                reason              TEXT NOT NULL,
                consecutive_losses  INTEGER,
                daily_pnl           REAL,
                resumed_at          INTEGER
            );

            CREATE INDEX IF NOT EXISTS idx_trades_window ON trades(window_ts);
            CREATE INDEX IF NOT EXISTS idx_trades_timestamp ON trades(timestamp);
            CREATE INDEX IF NOT EXISTS idx_signals_window ON signals(window_ts);
            CREATE INDEX IF NOT EXISTS idx_signals_timestamp ON signals(timestamp_ms);
            CREATE INDEX IF NOT EXISTS idx_signals_reason ON signals(rejection_reason);
            ",
        )?;

        // Backward-compatible schema migration: add new columns to existing trades table.
        // Each ALTER is wrapped in error-ignoring logic since SQLite errors if column exists.
        let alter_cols = [
            "ALTER TABLE trades ADD COLUMN ask_price_observed REAL",
            "ALTER TABLE trades ADD COLUMN bid_price_observed REAL",
            "ALTER TABLE trades ADD COLUMN spread_observed REAL",
            "ALTER TABLE trades ADD COLUMN ask_depth REAL",
            "ALTER TABLE trades ADD COLUMN bid_depth REAL",
            "ALTER TABLE trades ADD COLUMN up_trade_count INTEGER",
            "ALTER TABLE trades ADD COLUMN down_trade_count INTEGER",
            "ALTER TABLE trades ADD COLUMN opposite_side_ask REAL",
            "ALTER TABLE trades ADD COLUMN binance_price_entry REAL",
            "ALTER TABLE trades ADD COLUMN binance_open_price REAL",
            "ALTER TABLE trades ADD COLUMN rtds_price_entry REAL",
            "ALTER TABLE trades ADD COLUMN rtds_open_price REAL",
            "ALTER TABLE trades ADD COLUMN rtds_stale_at_entry INTEGER",
            "ALTER TABLE trades ADD COLUMN trend_strength REAL",
            "ALTER TABLE trades ADD COLUMN limit_price REAL",
            "ALTER TABLE trades ADD COLUMN fill_price REAL",
            "ALTER TABLE trades ADD COLUMN fill_attempts INTEGER",
            "ALTER TABLE trades ADD COLUMN signal_detected_ms INTEGER",
            "ALTER TABLE trades ADD COLUMN order_sent_ms INTEGER",
            "ALTER TABLE trades ADD COLUMN order_ack_ms INTEGER",
            "ALTER TABLE trades ADD COLUMN bot_version TEXT",
            "ALTER TABLE trades ADD COLUMN neg_risk INTEGER",
        ];
        for sql in &alter_cols {
            let _ = conn.execute(sql, []);
        }

        info!("Database tables initialized");
        Ok(())
    }

    pub fn insert_trade(&self, trade: &TradeRecord) -> SqlResult<i64> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO trades (
                timestamp, window_ts, slug, side, btc_delta_pct,
                entry_price, shares, cost_usdc, secs_left, order_id, dry_run,
                ask_price_observed, bid_price_observed, spread_observed,
                ask_depth, bid_depth, up_trade_count, down_trade_count,
                opposite_side_ask,
                binance_price_entry, binance_open_price,
                rtds_price_entry, rtds_open_price, rtds_stale_at_entry,
                trend_strength,
                limit_price, fill_price, fill_attempts,
                signal_detected_ms, order_sent_ms, order_ack_ms,
                bot_version, neg_risk
             ) VALUES (
                ?1, ?2, ?3, ?4, ?5,
                ?6, ?7, ?8, ?9, ?10, ?11,
                ?12, ?13, ?14,
                ?15, ?16, ?17, ?18,
                ?19,
                ?20, ?21,
                ?22, ?23, ?24,
                ?25,
                ?26, ?27, ?28,
                ?29, ?30, ?31,
                ?32, ?33
             )",
            params![
                trade.timestamp,
                trade.window_ts,
                trade.slug,
                trade.side,
                trade.btc_delta_pct,
                trade.entry_price,
                trade.shares,
                trade.cost_usdc,
                trade.secs_left,
                trade.order_id,
                trade.dry_run as i32,
                trade.ask_price_observed,
                trade.bid_price_observed,
                trade.spread_observed,
                trade.ask_depth,
                trade.bid_depth,
                trade.up_trade_count,
                trade.down_trade_count,
                trade.opposite_side_ask,
                trade.binance_price_entry,
                trade.binance_open_price,
                trade.rtds_price_entry,
                trade.rtds_open_price,
                trade.rtds_stale_at_entry.map(|v| v as i32),
                trade.trend_strength,
                trade.limit_price,
                trade.fill_price,
                trade.fill_attempts,
                trade.signal_detected_ms,
                trade.order_sent_ms,
                trade.order_ack_ms,
                trade.bot_version,
                trade.neg_risk as i32,
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn insert_signal(
        &self,
        eval: &EvaluationResult,
        window_ts: i64,
        secs_left: i64,
        dry_run: bool,
    ) {
        let conn = self.conn.lock().unwrap();
        let now_ms = chrono::Utc::now().timestamp_millis();
        if let Err(e) = conn.execute(
            "INSERT INTO signals (
                timestamp_ms, window_ts, secs_left, btc_delta_pct,
                ask_price, bid_price, spread, ask_depth, trade_count,
                trend_strength, side, rejection_reason, dry_run
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                now_ms,
                window_ts,
                secs_left,
                eval.btc_delta_pct,
                eval.ask_price,
                eval.bid_price,
                eval.spread,
                eval.ask_depth,
                eval.trade_count.map(|v| v as i32),
                eval.trend_strength,
                eval.side,
                eval.rejection_reason,
                dry_run as i32,
            ],
        ) {
            error!("Failed to insert signal: {}", e);
        }
    }

    pub fn insert_kill_switch_event(
        &self,
        reason: &str,
        consecutive_losses: Option<i64>,
        daily_pnl: Option<f64>,
    ) {
        let conn = self.conn.lock().unwrap();
        let now = chrono::Utc::now().timestamp();
        if let Err(e) = conn.execute(
            "INSERT INTO kill_switch_events (timestamp, reason, consecutive_losses, daily_pnl)
             VALUES (?1, ?2, ?3, ?4)",
            params![now, reason, consecutive_losses, daily_pnl],
        ) {
            error!("Failed to insert kill switch event: {}", e);
        }
    }

    pub fn mark_kill_switch_resumed(&self) {
        let conn = self.conn.lock().unwrap();
        let now = chrono::Utc::now().timestamp();
        if let Err(e) = conn.execute(
            "UPDATE kill_switch_events SET resumed_at = ?1
             WHERE resumed_at IS NULL ORDER BY id DESC LIMIT 1",
            params![now],
        ) {
            error!("Failed to mark kill switch resumed: {}", e);
        }
    }

    fn update_daily_stats(&self, conn: &Connection, date: &str) -> SqlResult<()> {
        conn.execute(
            "INSERT INTO daily_stats (date, trades, wins, losses, total_cost, total_payout, net_pnl, win_rate)
             SELECT
                 ?1,
                 COUNT(*),
                 COALESCE(SUM(CASE WHEN won = 1 THEN 1 ELSE 0 END), 0),
                 COALESCE(SUM(CASE WHEN won = 0 THEN 1 ELSE 0 END), 0),
                 COALESCE(SUM(cost_usdc), 0),
                 COALESCE(SUM(CASE WHEN payout IS NOT NULL THEN payout ELSE 0 END), 0),
                 COALESCE(SUM(CASE WHEN profit IS NOT NULL THEN profit ELSE 0 END), 0),
                 CASE WHEN COUNT(CASE WHEN won IS NOT NULL THEN 1 END) > 0
                      THEN CAST(SUM(CASE WHEN won = 1 THEN 1 ELSE 0 END) AS REAL) /
                           COUNT(CASE WHEN won IS NOT NULL THEN 1 END)
                      ELSE 0 END
             FROM trades
             WHERE date(timestamp, 'unixepoch') = ?1
             ON CONFLICT(date) DO UPDATE SET
                 trades = excluded.trades,
                 wins = excluded.wins,
                 losses = excluded.losses,
                 total_cost = excluded.total_cost,
                 total_payout = excluded.total_payout,
                 net_pnl = excluded.net_pnl,
                 win_rate = excluded.win_rate",
            params![date],
        )?;
        Ok(())
    }

    pub fn get_stats_today(&self) -> TradingStats {
        self.get_stats_for_period("date(timestamp, 'unixepoch') = date('now')")
    }

    pub fn get_stats_week(&self) -> TradingStats {
        self.get_stats_for_period(
            "date(timestamp, 'unixepoch') >= date('now', '-7 days')",
        )
    }

    pub fn get_stats_all(&self) -> TradingStats {
        self.get_stats_for_period("1=1")
    }

    pub fn get_stats_today_live(&self) -> TradingStats {
        self.get_stats_for_period("date(timestamp, 'unixepoch') = date('now') AND dry_run = 0")
    }

    fn get_stats_for_period(&self, where_clause: &str) -> TradingStats {
        let conn = self.conn.lock().unwrap();
        let query = format!(
            "SELECT
                COUNT(*),
                COALESCE(SUM(CASE WHEN won = 1 THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN won = 0 THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(cost_usdc), 0),
                COALESCE(SUM(CASE WHEN payout IS NOT NULL THEN payout ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN profit IS NOT NULL THEN profit ELSE 0 END), 0)
             FROM trades WHERE {}",
            where_clause
        );
        conn.query_row(&query, [], |row| {
            let trades: i64 = row.get(0)?;
            let wins: i64 = row.get(1)?;
            let losses: i64 = row.get(2)?;
            let total_cost: f64 = row.get(3)?;
            let total_payout: f64 = row.get(4)?;
            let net_pnl: f64 = row.get(5)?;
            let resolved = wins + losses;
            let win_rate = if resolved > 0 {
                wins as f64 / resolved as f64 * 100.0
            } else {
                0.0
            };
            Ok(TradingStats {
                trades,
                wins,
                losses,
                total_cost,
                total_payout,
                net_pnl,
                win_rate,
            })
        })
        .unwrap_or_default()
    }

    pub fn get_last_trades(&self, n: i64) -> Vec<TradeRecord> {
        let conn = self.conn.lock().unwrap();
        let query = format!(
            "SELECT {} FROM trades ORDER BY id DESC LIMIT ?1",
            TRADE_SELECT_COLS
        );
        let mut stmt = conn.prepare(&query).unwrap();

        stmt.query_map(params![n], read_trade_row)
            .unwrap()
            .filter_map(|r| r.ok())
            .collect()
    }

    pub fn log_config_change(&self, param: &str, old_value: &str, new_value: &str) {
        let conn = self.conn.lock().unwrap();
        let now = chrono::Utc::now().timestamp();
        if let Err(e) = conn.execute(
            "INSERT INTO config_log (timestamp, param, old_value, new_value) VALUES (?1, ?2, ?3, ?4)",
            params![now, param, old_value, new_value],
        ) {
            error!("Failed to log config change: {}", e);
        }
    }

    pub fn resolve_trade_by_window_ts(
        &self,
        window_ts: i64,
        resolution: &str,
        won: bool,
        payout: f64,
        profit: f64,
    ) -> SqlResult<usize> {
        let conn = self.conn.lock().unwrap();
        let now = chrono::Utc::now().timestamp();
        let updated = conn.execute(
            "UPDATE trades SET resolution = ?1, won = ?2, payout = ?3, profit = ?4, resolved_at = ?5
             WHERE window_ts = ?6 AND resolution IS NULL",
            params![resolution, won as i32, payout, profit, now, window_ts],
        )?;

        let date = chrono::Utc::now().format("%Y-%m-%d").to_string();
        self.update_daily_stats(&conn, &date)?;
        Ok(updated)
    }

    pub fn get_trade_by_window_ts(&self, window_ts: i64) -> Option<TradeRecord> {
        let conn = self.conn.lock().unwrap();
        let query = format!(
            "SELECT {} FROM trades WHERE window_ts = ?1 ORDER BY id DESC LIMIT 1",
            TRADE_SELECT_COLS
        );
        conn.query_row(&query, params![window_ts], read_trade_row)
            .ok()
    }

    pub fn get_recent_consecutive_losses(&self) -> i64 {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT won FROM trades WHERE won IS NOT NULL AND dry_run = 0 ORDER BY id DESC",
            )
            .unwrap();

        let mut count: i64 = 0;
        let rows = stmt
            .query_map([], |row| {
                let won: i32 = row.get(0)?;
                Ok(won)
            })
            .unwrap();

        for row in rows {
            match row {
                Ok(0) => count += 1,
                _ => break,
            }
        }
        count
    }

}
