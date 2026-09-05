use rusqlite::{params, Connection, Result as SqlResult, Row};
use std::sync::Mutex;
use tracing::{error, info};

use crate::types::{EvaluationResult, SignalShadowContext, TradeRecord, TradingStats};

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
    bot_version, neg_risk, \
    twap_delta_pct_at_entry, twap_strike_at_entry, used_twap_strike, \
    twap_source_at_entry, \
    binance_twap_delta_at_entry, binance_twap_strike_at_entry, \
    delta_momentum_at_entry";

/// True if `table` already has a column named `column`, per pragma table_info.
/// Used to guard additive migrations so re-running init is a no-op instead of
/// an error. Returns true on query failure so a broken pragma never causes a
/// blind ALTER.
fn column_exists(conn: &Connection, table: &str, column: &str) -> bool {
    let mut stmt = match conn.prepare(&format!("PRAGMA table_info({})", table)) {
        Ok(s) => s,
        Err(e) => {
            error!("pragma table_info({}) failed: {}", table, e);
            return true;
        }
    };
    let found = stmt.query_map([], |row| row.get::<_, String>(1)).map(|rows| {
        rows.filter_map(|r| r.ok()).any(|name| name == column)
    });
    match found {
        Ok(v) => v,
        Err(e) => {
            error!("pragma table_info({}) read failed: {}", table, e);
            true
        }
    }
}

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
        twap_delta_pct_at_entry: row.get(38)?,
        twap_strike_at_entry: row.get(39)?,
        // NULL on the 144 pre-migration rows: those all predate the flag and
        // were produced by the spot model.
        used_twap_strike: row.get::<_, Option<i32>>(40)?.map(|v| v == 1).unwrap_or(false),
        twap_source_at_entry: row.get(41)?,
        binance_twap_delta_at_entry: row.get(42)?,
        binance_twap_strike_at_entry: row.get(43)?,
        delta_momentum_at_entry: row.get(44)?,
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
            --   too_early, market_resolved, max_attempts, failed_fak, unmatched_fak,
            --   depth_unknown, trend_unavailable
            --   (historical rows may also contain failed_fok / unmatched_fok)
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

            -- DATA COLLECTION ONLY (Chainlink 30s TWAP settlement study).
            -- Nothing in the trading path reads this table.
            --
            -- Kept separate from `trades` deliberately:
            --   * `trades` only has rows for windows the bot actually entered,
            --     so per-window TWAP-vs-snapshot comparison would be lost for
            --     every window the bot passed on.
            --   * `trades` is read positionally (TRADE_SELECT_COLS +
            --     read_trade_row), so widening it would touch the read path
            --     that resolution depends on.
            --
            -- TWAP values are stored as TEXT holding the exact signed E18
            -- fixed-point integer from `payload.full_accuracy_value`. Divide by
            -- 10^18 with integer/decimal arithmetic — never through a float.
            CREATE TABLE IF NOT EXISTS twap_observations (
                window_ts                   INTEGER PRIMARY KEY,
                twap_30_at_open             TEXT,
                twap_30_at_open_ms          INTEGER,
                twap_30_at_entry            TEXT,
                twap_30_at_entry_ms         INTEGER,
                twap_30_at_resolve          TEXT,
                twap_30_at_resolve_ms       INTEGER,
                snapshot_price_at_open      REAL,
                snapshot_price_at_resolve   REAL,
                predicted_side              TEXT,
                actual_resolution           TEXT,
                updated_at                  INTEGER,
                -- Bot wall-clock at each capture, so capture timing can be
                -- verified against the window boundaries. Distinct from the
                -- *_ms columns above, which hold the FEED's Chainlink
                -- observation time (what freshness is judged on).
                open_captured_at_ms         INTEGER,
                resolve_captured_at_ms      INTEGER,
                -- Resolution-mapping diagnostics: lets a bad token->window
                -- attribution be told apart from a capture-timing problem.
                resolution_asset_id         TEXT,
                resolution_event_ms         INTEGER,
                expected_up_token           TEXT,
                expected_down_token         TEXT
            );

            -- PHASE 2 SHADOW QUOTING — MEASUREMENT ONLY.
            -- Nothing that writes this table sends an order. Each row is the
            -- quote the bot WOULD have posted for one side at one instant,
            -- plus the book at that instant, plus (filled in at resolution)
            -- whether a simulated fill landed on the winning side.
            --
            -- fill_was_correct is defined ONLY for rows where would_bid_fill=1:
            -- a bid fill BUYS the side, so correct means that side won. It
            -- is NULL where no bid fill was simulated. Ask-fill correctness is
            -- derivable in SQL from side vs actual_resolution and is left out
            -- rather than overloading one column with two meanings.
            CREATE TABLE IF NOT EXISTS maker_shadow (
                window_ts           INTEGER NOT NULL,
                timestamp_ms        INTEGER NOT NULL,
                secs_left           INTEGER,
                side                TEXT NOT NULL,
                decision_delta      REAL,
                fair_value          REAL,
                our_bid             REAL,
                our_ask             REAL,
                market_bid          REAL,
                market_ask          REAL,
                would_bid_fill      INTEGER,
                would_ask_fill      INTEGER,
                actual_resolution   TEXT,
                fill_was_correct    INTEGER,
                PRIMARY KEY (window_ts, timestamp_ms, side)
            );

            -- UNCONDITIONAL DELTA SAMPLING. The pricing dataset.
            --
            -- One row every DELTA_SAMPLE_INTERVAL_MS for EVERY window, with no
            -- filtering of any kind: no threshold, no trend gate, no side
            -- selection, no pause or resolution check. Written for windows the
            -- bot never trades, which is most of them.
            --
            -- This exists because every other table is conditioned on the
            -- strategy's own gates, and those gates admit only the obvious
            -- moves. A curve fitted on them knows how to price near-certainties
            -- and nothing else -- exactly the wrong half of the distribution
            -- for a market maker, whose business is the uncertain middle.
            --
            -- `maker_shadow` is worse than merely filtered: it is only written
            -- when fair_value() already returns Some, so it can never contain a
            -- cell the current curve cannot price. Refitting from it reproduces
            -- the existing coverage hole exactly. This table breaks that loop.
            --
            -- NULL means genuinely unavailable (stale TWAP, empty book side).
            -- No field is ever substituted or interpolated.
            CREATE TABLE IF NOT EXISTS delta_samples (
                window_ts           INTEGER NOT NULL,
                timestamp_ms        INTEGER NOT NULL,
                secs_left           INTEGER NOT NULL,
                twap_delta_pct      REAL,
                spot_delta_pct      REAL,
                twap_strike         TEXT,
                twap_current        TEXT,
                up_ask              REAL,
                up_bid              REAL,
                down_ask            REAL,
                down_bid            REAL,
                actual_resolution   TEXT,
                PRIMARY KEY (window_ts, timestamp_ms)
            );

            CREATE INDEX IF NOT EXISTS idx_maker_shadow_window
                ON maker_shadow(window_ts);
            CREATE INDEX IF NOT EXISTS idx_maker_shadow_ts
                ON maker_shadow(timestamp_ms);

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

        // TWAP strike migration. Pragma-guarded and additive with NULL
        // defaults — live history in both tables is preserved untouched.
        // Appended at the END of `trades` so no existing positional index in
        // TRADE_SELECT_COLS / read_trade_row shifts.
        let strike_cols: [(&str, &str, &str); 5] = [
            ("trades", "twap_delta_pct_at_entry", "REAL"),
            ("trades", "twap_strike_at_entry", "TEXT"),
            ("trades", "used_twap_strike", "INTEGER"),
            // Lets agreement be measured on REJECTED signals too, not just
            // entered ones.
            ("signals", "twap_delta_pct", "REAL"),
            ("trades", "twap_source_at_entry", "TEXT"),
        ];
        for (table, name, ty) in &strike_cols {
            if !column_exists(&conn, table, name) {
                let sql = format!("ALTER TABLE {} ADD COLUMN {} {}", table, name, ty);
                if let Err(e) = conn.execute(&sql, []) {
                    error!("Failed to add {}.{}: {}", table, name, e);
                }
            }
        }

        // Binance-derived TWAP shadow dataset. Same pragma-guarded additive
        // approach: NULL defaults, idempotent, nothing dropped or recreated.
        // Binance values are REAL (the feed publishes floats); Chainlink values
        // stay TEXT holding raw E18, so the two representations never share a
        // column.
        let binance_twap_cols: [(&str, &str, &str); 32] = [
            ("signals", "binance_twap_delta_pct", "REAL"),
            ("signals", "binance_twap_value", "REAL"),
            ("signals", "binance_twap_strike", "REAL"),
            ("signals", "binance_spot_price", "REAL"),
            ("signals", "chainlink_twap_value", "TEXT"),
            ("signals", "chainlink_twap_strike", "TEXT"),
            ("signals", "binance_buffer_span_ms", "INTEGER"),
            ("signals", "binance_buffer_samples", "INTEGER"),
            // Lead-time measurement (Part 5).
            ("signals", "chainlink_twap_observed_ms", "INTEGER"),
            ("signals", "signal_evaluated_at_ms", "INTEGER"),
            ("signals", "binance_newest_sample_ms", "INTEGER"),
            // Per-window comparison against the settled outcome.
            ("twap_observations", "binance_twap_at_open", "REAL"),
            ("twap_observations", "binance_twap_at_resolve", "REAL"),
            ("twap_observations", "binance_twap_open_ms", "INTEGER"),
            ("twap_observations", "binance_twap_resolve_ms", "INTEGER"),
            // Appended at the END of trades so no existing positional index in
            // TRADE_SELECT_COLS / read_trade_row shifts.
            ("trades", "binance_twap_delta_at_entry", "REAL"),
            ("trades", "binance_twap_strike_at_entry", "REAL"),
            // Delta-momentum filter. The past value and its age are recorded
            // alongside the ratio so a genuine reversal can be told apart from
            // a comparison against a stale or too-recent reading.
            ("signals", "delta_momentum", "REAL"),
            ("signals", "delta_past_value", "REAL"),
            ("signals", "delta_past_age_ms", "INTEGER"),
            ("trades", "delta_momentum_at_entry", "REAL"),
            // Entry-pattern diagnostics, written on every signal including
            // rejections so they can be scored against actual_resolution.
            ("signals", "delta_peak_abs", "REAL"),
            ("signals", "delta_peak_secs_ago", "REAL"),
            ("signals", "delta_rise_time_s", "REAL"),
            ("signals", "delta_5s_ago", "REAL"),
            ("signals", "delta_15s_ago", "REAL"),
            ("signals", "delta_30s_ago", "REAL"),
            ("signals", "ask_5s_ago", "REAL"),
            ("signals", "ask_30s_ago", "REAL"),
            ("signals", "ask_peak_signalled", "REAL"),
            ("signals", "ask_peak_any", "REAL"),
            // The delta that actually drove the threshold and side selection,
            // respecting use_twap_strike. btc_delta_pct and twap_delta_pct are
            // both kept alongside it: in spot mode both are populated, so which
            // one decided cannot be reconstructed from them after the fact.
            ("signals", "decision_delta", "REAL"),
        ];
        for (table, name, ty) in &binance_twap_cols {
            if !column_exists(&conn, table, name) {
                let sql = format!("ALTER TABLE {} ADD COLUMN {} {}", table, name, ty);
                if let Err(e) = conn.execute(&sql, []) {
                    error!("Failed to add {}.{}: {}", table, name, e);
                }
            }
        }

        // TWAP observation columns, added with an explicit pragma guard so a
        // pre-existing twap_observations table (from an earlier build) is
        // widened in place rather than dropped/recreated. Additive and
        // NULL-defaulted; no existing data is touched.
        let twap_cols: [(&str, &str); 18] = [
            ("twap_30_at_open", "TEXT"),
            ("twap_30_at_open_ms", "INTEGER"),
            ("twap_30_at_entry", "TEXT"),
            ("twap_30_at_entry_ms", "INTEGER"),
            ("twap_30_at_resolve", "TEXT"),
            ("twap_30_at_resolve_ms", "INTEGER"),
            ("snapshot_price_at_open", "REAL"),
            ("snapshot_price_at_resolve", "REAL"),
            ("predicted_side", "TEXT"),
            ("actual_resolution", "TEXT"),
            ("updated_at", "INTEGER"),
            ("open_captured_at_ms", "INTEGER"),
            ("resolve_captured_at_ms", "INTEGER"),
            ("resolution_asset_id", "TEXT"),
            ("resolution_event_ms", "INTEGER"),
            ("expected_up_token", "TEXT"),
            ("expected_down_token", "TEXT"),
            // Which feed produced the stored TWAP values for this window.
            ("twap_source", "TEXT"),
        ];
        for (name, ty) in &twap_cols {
            if !column_exists(&conn, "twap_observations", name) {
                let sql = format!(
                    "ALTER TABLE twap_observations ADD COLUMN {} {}",
                    name, ty
                );
                if let Err(e) = conn.execute(&sql, []) {
                    error!("Failed to add twap_observations column {}: {}", name, e);
                }
            }
        }

        // maker_shadow columns, pragma-guarded like every other migration so a
        // table created by an earlier build is widened in place rather than
        // dropped. Additive and NULL-defaulted throughout.
        let maker_shadow_cols: [(&str, &str); 12] = [
            ("secs_left", "INTEGER"),
            ("decision_delta", "REAL"),
            ("fair_value", "REAL"),
            ("our_bid", "REAL"),
            ("our_ask", "REAL"),
            ("market_bid", "REAL"),
            ("market_ask", "REAL"),
            ("would_bid_fill", "INTEGER"),
            ("would_ask_fill", "INTEGER"),
            ("actual_resolution", "TEXT"),
            ("fill_was_correct", "INTEGER"),
            // Reserved so Phase 3 can record a real skew without a schema
            // change; written as 0.0 in shadow mode.
            ("inventory_skew", "REAL"),
        ];
        for (name, ty) in &maker_shadow_cols {
            if !column_exists(&conn, "maker_shadow", name) {
                let sql = format!("ALTER TABLE maker_shadow ADD COLUMN {} {}", name, ty);
                if let Err(e) = conn.execute(&sql, []) {
                    error!("Failed to add maker_shadow column {}: {}", name, e);
                }
            }
        }

        // delta_samples columns, pragma-guarded like every other migration.
        // The PRIMARY KEY columns are created with the table and never altered.
        let delta_sample_cols: [(&str, &str); 10] = [
            ("twap_delta_pct", "REAL"),
            ("spot_delta_pct", "REAL"),
            ("twap_strike", "TEXT"),
            ("twap_current", "TEXT"),
            ("up_ask", "REAL"),
            ("up_bid", "REAL"),
            ("down_ask", "REAL"),
            ("down_bid", "REAL"),
            ("actual_resolution", "TEXT"),
            ("secs_left", "INTEGER"),
        ];
        for (name, ty) in &delta_sample_cols {
            if !column_exists(&conn, "delta_samples", name) {
                let sql = format!("ALTER TABLE delta_samples ADD COLUMN {} {}", name, ty);
                if let Err(e) = conn.execute(&sql, []) {
                    error!("Failed to add delta_samples column {}: {}", name, e);
                }
            }
        }

        info!("Database tables initialized");
        Ok(())
    }

    /// Record the TWAP/snapshot reading at window open. Upsert keyed by
    /// window_ts. `twap` is the exact E18 string, or None when the reading is
    /// stale or absent — stored as NULL rather than interpolated.
    ///
    /// `twap_observed_ms` is the FEED's Chainlink observation time;
    /// `captured_at_ms` is the bot's wall-clock at capture. They answer
    /// different questions and are stored separately.
    // Wide by nature: one parameter per recorded column. Grouping them into a
    // struct would add indirection without removing the fields.
    #[allow(clippy::too_many_arguments)]
    pub fn record_twap_at_open(
        &self,
        window_ts: i64,
        twap: Option<&str>,
        twap_observed_ms: Option<i64>,
        snapshot_price: Option<f64>,
        captured_at_ms: i64,
        source: Option<&str>,
        binance_twap: Option<f64>,
    ) {
        let conn = self.conn.lock().unwrap();
        let now = chrono::Utc::now().timestamp();
        if let Err(e) = conn.execute(
            "INSERT INTO twap_observations (
                window_ts, twap_30_at_open, twap_30_at_open_ms,
                snapshot_price_at_open, open_captured_at_ms, twap_source,
                binance_twap_at_open, binance_twap_open_ms, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(window_ts) DO UPDATE SET
                twap_30_at_open = excluded.twap_30_at_open,
                twap_30_at_open_ms = excluded.twap_30_at_open_ms,
                snapshot_price_at_open = excluded.snapshot_price_at_open,
                open_captured_at_ms = excluded.open_captured_at_ms,
                twap_source = COALESCE(excluded.twap_source, twap_observations.twap_source),
                binance_twap_at_open = excluded.binance_twap_at_open,
                binance_twap_open_ms = excluded.binance_twap_open_ms,
                updated_at = excluded.updated_at",
            params![
                window_ts,
                twap,
                twap_observed_ms,
                snapshot_price,
                captured_at_ms,
                source,
                binance_twap,
                captured_at_ms,
                now
            ],
        ) {
            error!("Failed to record TWAP at open: {}", e);
        }
    }

    /// Record the up/down token ids the bot holds for a window, at discovery
    /// time. Used later to detect a resolution attributed to the wrong window.
    pub fn record_expected_tokens(&self, window_ts: i64, up_token: &str, down_token: &str) {
        let conn = self.conn.lock().unwrap();
        let now = chrono::Utc::now().timestamp();
        if let Err(e) = conn.execute(
            "INSERT INTO twap_observations (
                window_ts, expected_up_token, expected_down_token, updated_at
             ) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(window_ts) DO UPDATE SET
                expected_up_token = excluded.expected_up_token,
                expected_down_token = excluded.expected_down_token,
                updated_at = excluded.updated_at",
            params![window_ts, up_token, down_token, now],
        ) {
            error!("Failed to record expected tokens: {}", e);
        }
    }

    /// The up/down token ids recorded for a window, if any.
    pub fn get_expected_tokens(&self, window_ts: i64) -> (Option<String>, Option<String>) {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT expected_up_token, expected_down_token
             FROM twap_observations WHERE window_ts = ?1",
            params![window_ts],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap_or((None, None))
    }

    /// The boundary-captured close values for a window, for the comparison log.
    /// Returns (twap_30_at_resolve, snapshot_price_at_resolve).
    pub fn get_twap_closes(&self, window_ts: i64) -> (Option<String>, Option<f64>) {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT twap_30_at_resolve, snapshot_price_at_resolve
             FROM twap_observations WHERE window_ts = ?1",
            params![window_ts],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap_or((None, None))
    }

    /// Record the resolution event's own metadata, written when the
    /// market_resolved event arrives (which is later and more variable than the
    /// window boundary — hence kept separate from the close capture).
    pub fn record_resolution_diagnostics(
        &self,
        window_ts: i64,
        actual_resolution: &str,
        resolution_asset_id: &str,
        resolution_event_ms: i64,
    ) {
        let conn = self.conn.lock().unwrap();
        let now = chrono::Utc::now().timestamp();
        if let Err(e) = conn.execute(
            "INSERT INTO twap_observations (
                window_ts, actual_resolution, resolution_asset_id,
                resolution_event_ms, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(window_ts) DO UPDATE SET
                actual_resolution = excluded.actual_resolution,
                resolution_asset_id = excluded.resolution_asset_id,
                resolution_event_ms = excluded.resolution_event_ms,
                updated_at = excluded.updated_at",
            params![
                window_ts,
                actual_resolution,
                resolution_asset_id,
                resolution_event_ms,
                now
            ],
        ) {
            error!("Failed to record resolution diagnostics: {}", e);
        }
    }

    /// Record the TWAP reading at the moment an order was placed.
    pub fn record_twap_at_entry(
        &self,
        window_ts: i64,
        twap: Option<&str>,
        twap_observed_ms: Option<i64>,
        predicted_side: &str,
    ) {
        let conn = self.conn.lock().unwrap();
        let now = chrono::Utc::now().timestamp();
        if let Err(e) = conn.execute(
            "INSERT INTO twap_observations (
                window_ts, twap_30_at_entry, twap_30_at_entry_ms,
                predicted_side, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(window_ts) DO UPDATE SET
                twap_30_at_entry = excluded.twap_30_at_entry,
                twap_30_at_entry_ms = excluded.twap_30_at_entry_ms,
                predicted_side = excluded.predicted_side,
                updated_at = excluded.updated_at",
            params![window_ts, twap, twap_observed_ms, predicted_side, now],
        ) {
            error!("Failed to record TWAP at entry: {}", e);
        }
    }

    /// Record the TWAP/snapshot close, captured at the WINDOW BOUNDARY
    /// (window_ts + WINDOW_SECS) rather than on the market_resolved event,
    /// which arrives late and with variable delay.
    #[allow(clippy::too_many_arguments)]
    pub fn record_twap_at_resolve(
        &self,
        window_ts: i64,
        twap: Option<&str>,
        twap_observed_ms: Option<i64>,
        snapshot_price: Option<f64>,
        captured_at_ms: i64,
        source: Option<&str>,
        binance_twap: Option<f64>,
    ) {
        let conn = self.conn.lock().unwrap();
        let now = chrono::Utc::now().timestamp();
        if let Err(e) = conn.execute(
            "INSERT INTO twap_observations (
                window_ts, twap_30_at_resolve, twap_30_at_resolve_ms,
                snapshot_price_at_resolve, resolve_captured_at_ms, twap_source,
                binance_twap_at_resolve, binance_twap_resolve_ms, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(window_ts) DO UPDATE SET
                twap_30_at_resolve = excluded.twap_30_at_resolve,
                twap_30_at_resolve_ms = excluded.twap_30_at_resolve_ms,
                snapshot_price_at_resolve = excluded.snapshot_price_at_resolve,
                resolve_captured_at_ms = excluded.resolve_captured_at_ms,
                twap_source = COALESCE(excluded.twap_source, twap_observations.twap_source),
                binance_twap_at_resolve = excluded.binance_twap_at_resolve,
                binance_twap_resolve_ms = excluded.binance_twap_resolve_ms,
                updated_at = excluded.updated_at",
            params![
                window_ts,
                twap,
                twap_observed_ms,
                snapshot_price,
                captured_at_ms,
                source,
                binance_twap,
                captured_at_ms,
                now
            ],
        ) {
            error!("Failed to record TWAP at resolve: {}", e);
        }
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
                bot_version, neg_risk,
                twap_delta_pct_at_entry, twap_strike_at_entry, used_twap_strike,
                twap_source_at_entry,
                binance_twap_delta_at_entry, binance_twap_strike_at_entry,
                delta_momentum_at_entry
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
                ?32, ?33,
                ?34, ?35, ?36,
                ?37,
                ?38, ?39,
                ?40
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
                trade.twap_delta_pct_at_entry,
                trade.twap_strike_at_entry,
                trade.used_twap_strike as i32,
                trade.twap_source_at_entry,
                trade.binance_twap_delta_at_entry,
                trade.binance_twap_strike_at_entry,
                trade.delta_momentum_at_entry,
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// `ctx` carries SHADOW-ONLY observability fields (Binance TWAP estimate,
    /// paired Chainlink values, buffer health, lead-time timestamps). Written
    /// on every signal, including rejections — rejections are the larger sample
    /// and where most of the comparison value lives.
    pub fn insert_signal(
        &self,
        eval: &EvaluationResult,
        window_ts: i64,
        secs_left: i64,
        dry_run: bool,
        ctx: &SignalShadowContext,
    ) {
        let conn = self.conn.lock().unwrap();
        let now_ms = chrono::Utc::now().timestamp_millis();
        if let Err(e) = conn.execute(
            "INSERT INTO signals (
                timestamp_ms, window_ts, secs_left, btc_delta_pct,
                ask_price, bid_price, spread, ask_depth, trade_count,
                trend_strength, side, rejection_reason, dry_run,
                twap_delta_pct,
                binance_twap_delta_pct, binance_twap_value, binance_twap_strike,
                binance_spot_price, chainlink_twap_value, chainlink_twap_strike,
                binance_buffer_span_ms, binance_buffer_samples,
                chainlink_twap_observed_ms, signal_evaluated_at_ms,
                binance_newest_sample_ms,
                delta_momentum, delta_past_value, delta_past_age_ms,
                delta_peak_abs, delta_peak_secs_ago, delta_rise_time_s,
                delta_5s_ago, delta_15s_ago, delta_30s_ago,
                ask_5s_ago, ask_30s_ago, ask_peak_signalled, ask_peak_any,
                decision_delta
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14,
                       ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25,
                       ?26, ?27, ?28,
                       ?29, ?30, ?31, ?32, ?33, ?34, ?35, ?36, ?37, ?38,
                       ?39)",
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
                eval.twap_delta_pct,
                // Prefer the value the evaluation itself produced; fall back to
                // the context for rejections raised before evaluate_entry ran.
                eval.binance_twap_delta_pct.or(ctx.binance_twap_delta_pct),
                ctx.binance_twap_value,
                ctx.binance_twap_strike,
                ctx.binance_spot_price,
                ctx.chainlink_twap_value,
                ctx.chainlink_twap_strike,
                ctx.binance_buffer_span_ms,
                ctx.binance_buffer_samples,
                ctx.chainlink_twap_observed_ms,
                ctx.signal_evaluated_at_ms,
                ctx.binance_newest_sample_ms,
                eval.delta_momentum,
                eval.delta_past_value,
                eval.delta_past_age_ms,
                ctx.delta_peak_abs,
                ctx.delta_peak_secs_ago,
                ctx.delta_rise_time_s,
                ctx.delta_5s_ago,
                ctx.delta_15s_ago,
                ctx.delta_30s_ago,
                ctx.ask_5s_ago,
                ctx.ask_30s_ago,
                ctx.ask_peak_signalled,
                ctx.ask_peak_any,
                eval.decision_delta,
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

    // ── PHASE 2 SHADOW QUOTING (measurement only) ──

    /// Record one simulated quote. `INSERT OR REPLACE` because the primary key
    /// is (window_ts, timestamp_ms, side) and a same-millisecond retry should
    /// overwrite rather than error.
    ///
    /// Failures are logged and swallowed: this is a measurement side-channel
    /// and must never be able to disturb the trading loop.
    pub fn insert_maker_shadow(
        &self,
        window_ts: i64,
        timestamp_ms: i64,
        q: &crate::maker::ShadowQuote,
    ) {
        let conn = self.conn.lock().unwrap();
        if let Err(e) = conn.execute(
            "INSERT OR REPLACE INTO maker_shadow (
                window_ts, timestamp_ms, secs_left, side, decision_delta,
                fair_value, our_bid, our_ask, market_bid, market_ask,
                would_bid_fill, would_ask_fill, inventory_skew
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                window_ts,
                timestamp_ms,
                q.secs_left,
                q.side.as_str(),
                q.decision_delta,
                q.fair_value,
                q.our_bid,
                q.our_ask,
                q.market_bid,
                q.market_ask,
                q.would_bid_fill as i32,
                q.would_ask_fill as i32,
                // Phase 2 carries no inventory; the column is reserved.
                0.0_f64,
            ],
        ) {
            error!("Failed to insert maker_shadow row: {}", e);
        }
    }

    /// Label every shadow row of a resolved window with the settled outcome.
    ///
    /// THE KEY MEASUREMENT. `fill_was_correct` is set only where a bid fill was
    /// simulated: buying that side was right exactly when that side won. Rows
    /// with no simulated bid fill keep NULL, so the adverse-selection rate is a
    /// plain AVG over the non-NULL values.
    ///
    /// Returns the number of rows updated.
    pub fn resolve_maker_shadow(&self, window_ts: i64, resolution: &str) -> i64 {
        let conn = self.conn.lock().unwrap();
        match conn.execute(
            "UPDATE maker_shadow
                SET actual_resolution = ?1,
                    fill_was_correct = CASE
                        WHEN would_bid_fill = 1
                        THEN CASE WHEN side = ?1 THEN 1 ELSE 0 END
                        ELSE NULL
                    END
              WHERE window_ts = ?2 AND actual_resolution IS NULL",
            params![resolution, window_ts],
        ) {
            Ok(n) => n as i64,
            Err(e) => {
                error!("Failed to resolve maker_shadow rows: {}", e);
                0
            }
        }
    }

    /// Adverse-selection summary over the most recent `n` RESOLVED shadow rows.
    ///
    /// Unresolved rows are excluded: a fill with no outcome yet cannot be
    /// scored, and counting it would drag every rate toward zero.
    pub fn maker_shadow_summary(&self, n: i64) -> MakerShadowSummary {
        let conn = self.conn.lock().unwrap();
        let sql = "
            WITH recent AS (
                SELECT * FROM maker_shadow
                 WHERE actual_resolution IS NOT NULL
                 ORDER BY timestamp_ms DESC LIMIT ?1
            )
            SELECT
                COUNT(*),
                SUM(would_bid_fill),
                SUM(CASE WHEN would_bid_fill = 1 AND side = actual_resolution
                         THEN 1 ELSE 0 END),
                SUM(would_ask_fill),
                SUM(CASE WHEN would_ask_fill = 1 AND side = actual_resolution
                         THEN 1 ELSE 0 END),
                AVG(CASE WHEN market_bid IS NOT NULL AND market_ask IS NOT NULL
                         THEN ABS(fair_value - (market_bid + market_ask) / 2.0)
                         END),
                MIN(timestamp_ms),
                MAX(timestamp_ms)
            FROM recent";
        conn.query_row(sql, params![n], |row| {
            Ok(MakerShadowSummary {
                rows: row.get::<_, Option<i64>>(0)?.unwrap_or(0),
                bid_fills: row.get::<_, Option<i64>>(1)?.unwrap_or(0),
                bid_fills_on_winner: row.get::<_, Option<i64>>(2)?.unwrap_or(0),
                ask_fills: row.get::<_, Option<i64>>(3)?.unwrap_or(0),
                ask_fills_on_winner: row.get::<_, Option<i64>>(4)?.unwrap_or(0),
                mean_abs_fv_minus_mid: row.get(5)?,
                first_ts_ms: row.get(6)?,
                last_ts_ms: row.get(7)?,
            })
        })
        .unwrap_or_default()
    }

    /// Shadow rows still awaiting a resolution label, for /maker.
    pub fn maker_shadow_pending(&self) -> i64 {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM maker_shadow WHERE actual_resolution IS NULL",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0)
    }
}

/// Aggregates behind the `/maker` Telegram report.
#[derive(Debug, Clone, Default)]
pub struct MakerShadowSummary {
    pub rows: i64,
    pub bid_fills: i64,
    /// Of the simulated bid fills, how many bought the side that won.
    pub bid_fills_on_winner: i64,
    pub ask_fills: i64,
    /// Of the simulated ask fills, how many SOLD the side that won -- i.e. how
    /// often selling was the wrong side of the trade.
    pub ask_fills_on_winner: i64,
    /// Mean |fair_value - market mid| over rows where both book sides existed.
    pub mean_abs_fv_minus_mid: Option<f64>,
    pub first_ts_ms: Option<i64>,
    pub last_ts_ms: Option<i64>,
}


/// One unconditional observation of market state. Every field is Option
/// because every field can be genuinely unavailable, and a substituted value
/// would be indistinguishable from a real one in the fitted curve.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DeltaSample {
    pub secs_left: i64,
    pub twap_delta_pct: Option<f64>,
    pub spot_delta_pct: Option<f64>,
    pub twap_strike: Option<String>,
    pub twap_current: Option<String>,
    pub up_ask: Option<f64>,
    pub up_bid: Option<f64>,
    pub down_ask: Option<f64>,
    pub down_bid: Option<f64>,
}

/// Coverage of the fitted grid, for the `/maker` report and the coverage check.
#[derive(Debug, Clone, Default)]
pub struct DeltaSampleCoverage {
    pub rows: i64,
    pub resolved_rows: i64,
    pub windows: i64,
    pub resolved_windows: i64,
    /// Rows in the uncertain middle band, |twap_delta| < 0.05 — the region the
    /// signals-based fit had no observations of.
    pub mid_band_rows: i64,
    pub mid_band_resolved: i64,
}

impl Database {
    // ── UNCONDITIONAL DELTA SAMPLING ──

    /// Record one unconditional market-state sample.
    ///
    /// `INSERT OR IGNORE`: the primary key is (window_ts, timestamp_ms), and a
    /// duplicate is a harmless double-fire, not an error worth logging.
    ///
    /// Failures are logged and swallowed. This is a measurement side-channel
    /// and must never be able to disturb the trading loop.
    pub fn insert_delta_sample(&self, window_ts: i64, timestamp_ms: i64, s: &DeltaSample) {
        let conn = self.conn.lock().unwrap();
        if let Err(e) = conn.execute(
            "INSERT OR IGNORE INTO delta_samples (
                window_ts, timestamp_ms, secs_left,
                twap_delta_pct, spot_delta_pct, twap_strike, twap_current,
                up_ask, up_bid, down_ask, down_bid
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                window_ts,
                timestamp_ms,
                s.secs_left,
                s.twap_delta_pct,
                s.spot_delta_pct,
                s.twap_strike,
                s.twap_current,
                s.up_ask,
                s.up_bid,
                s.down_ask,
                s.down_bid,
            ],
        ) {
            error!("Failed to insert delta_sample: {}", e);
        }
    }

    /// Backfill the settled outcome onto every sample of a window.
    ///
    /// Called for EVERY resolution, including windows the bot never traded --
    /// those are the majority and they carry most of the information, because
    /// they are the ones where no signal ever qualified.
    ///
    /// Idempotent: the `actual_resolution IS NULL` clause means a repeated or
    /// contradictory later resolution cannot relabel settled rows.
    pub fn resolve_delta_samples(&self, window_ts: i64, resolution: &str) -> i64 {
        let conn = self.conn.lock().unwrap();
        match conn.execute(
            "UPDATE delta_samples SET actual_resolution = ?1
              WHERE window_ts = ?2 AND actual_resolution IS NULL",
            params![resolution, window_ts],
        ) {
            Ok(n) => n as i64,
            Err(e) => {
                error!("Failed to backfill delta_samples resolution: {}", e);
                0
            }
        }
    }

    /// Sampling coverage, with the uncertain middle band called out separately.
    ///
    /// The mid-band counts are the whole point: if they stay at zero, something
    /// is still filtering the sampler and the refit will reproduce the same
    /// hole rather than close it.
    pub fn delta_sample_coverage(&self) -> DeltaSampleCoverage {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT
                COUNT(*),
                SUM(CASE WHEN actual_resolution IS NOT NULL THEN 1 ELSE 0 END),
                COUNT(DISTINCT window_ts),
                COUNT(DISTINCT CASE WHEN actual_resolution IS NOT NULL
                                    THEN window_ts END),
                SUM(CASE WHEN twap_delta_pct IS NOT NULL
                          AND ABS(twap_delta_pct) < 0.05 THEN 1 ELSE 0 END),
                SUM(CASE WHEN twap_delta_pct IS NOT NULL
                          AND ABS(twap_delta_pct) < 0.05
                          AND actual_resolution IS NOT NULL THEN 1 ELSE 0 END)
             FROM delta_samples",
            [],
            |row| {
                Ok(DeltaSampleCoverage {
                    rows: row.get::<_, Option<i64>>(0)?.unwrap_or(0),
                    resolved_rows: row.get::<_, Option<i64>>(1)?.unwrap_or(0),
                    windows: row.get::<_, Option<i64>>(2)?.unwrap_or(0),
                    resolved_windows: row.get::<_, Option<i64>>(3)?.unwrap_or(0),
                    mid_band_rows: row.get::<_, Option<i64>>(4)?.unwrap_or(0),
                    mid_band_resolved: row.get::<_, Option<i64>>(5)?.unwrap_or(0),
                })
            },
        )
        .unwrap_or_default()
    }
}

#[cfg(test)]
mod migration_tests {
    use super::*;
    use crate::fairvalue::Side;
    use crate::maker::ShadowQuote;

    fn tmp_path(name: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "btc5min-migration-{}-{}.db",
            name,
            std::process::id()
        ));
        let _ = std::fs::remove_file(&p);
        p
    }

    /// A database as it existed BEFORE this change: the original `trades` and
    /// `signals` shapes, with rows in them. Running `Database::new` over this
    /// must widen it without losing anything.
    fn legacy_db(path: &std::path::Path) {
        let conn = Connection::open(path).expect("open legacy");
        conn.execute_batch(
            "CREATE TABLE trades (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp INTEGER NOT NULL,
                window_ts INTEGER NOT NULL,
                slug TEXT NOT NULL,
                side TEXT NOT NULL,
                btc_delta_pct REAL NOT NULL,
                entry_price REAL NOT NULL,
                shares REAL NOT NULL,
                cost_usdc REAL NOT NULL,
                secs_left INTEGER NOT NULL,
                resolution TEXT,
                won INTEGER,
                payout REAL,
                profit REAL,
                resolved_at INTEGER,
                order_id TEXT,
                dry_run INTEGER DEFAULT 0
             );
             CREATE TABLE signals (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp_ms INTEGER NOT NULL,
                window_ts INTEGER NOT NULL,
                secs_left INTEGER NOT NULL,
                btc_delta_pct REAL,
                ask_price REAL,
                bid_price REAL,
                spread REAL,
                ask_depth REAL,
                trade_count INTEGER,
                trend_strength REAL,
                side TEXT,
                rejection_reason TEXT NOT NULL,
                dry_run INTEGER NOT NULL
             );
             INSERT INTO trades (timestamp, window_ts, slug, side, btc_delta_pct,
                                 entry_price, shares, cost_usdc, secs_left,
                                 resolution, won, payout, profit, dry_run)
             VALUES (1, 300, 'w-300', 'Up', 0.09, 0.62, 5, 3.1, 90,
                     'Up', 1, 5.0, 1.9, 0),
                    (2, 600, 'w-600', 'Down', -0.11, 0.55, 5, 2.75, 60,
                     'Up', 0, 0.0, -2.75, 0);
             INSERT INTO signals (timestamp_ms, window_ts, secs_left,
                                  btc_delta_pct, rejection_reason, dry_run)
             VALUES (1000, 300, 90, 0.09, 'entered', 0),
                    (2000, 300, 80, 0.05, 'below_threshold', 0),
                    (3000, 600, 60, -0.11, 'entered', 0);",
        )
        .expect("seed legacy");
    }

    fn count(db: &Database, table: &str) -> i64 {
        let conn = db.conn.lock().unwrap();
        conn.query_row(&format!("SELECT COUNT(*) FROM {}", table), [], |r| r.get(0))
            .unwrap_or(-1)
    }

    /// The migration must be purely additive: existing rows survive untouched
    /// and the new table appears.
    #[test]
    fn migration_is_additive_on_a_legacy_schema() {
        let path = tmp_path("legacy");
        legacy_db(&path);

        let db = Database::new(path.to_str().unwrap()).expect("migrate");

        assert_eq!(count(&db, "trades"), 2, "trades rows were lost");
        assert_eq!(count(&db, "signals"), 3, "signals rows were lost");
        assert_eq!(count(&db, "maker_shadow"), 0, "maker_shadow should start empty");

        // The values in the pre-existing rows are unchanged.
        {
            let conn = db.conn.lock().unwrap();
            let (side, profit): (String, f64) = conn
                .query_row(
                    "SELECT side, profit FROM trades WHERE window_ts = 300",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .expect("legacy trade still readable");
            assert_eq!(side, "Up");
            assert!((profit - 1.9).abs() < 1e-9);
        }

        // And the columns this change depends on now exist.
        {
            let conn = db.conn.lock().unwrap();
            assert!(column_exists(&conn, "signals", "decision_delta"));
            assert!(column_exists(&conn, "maker_shadow", "fill_was_correct"));
            assert!(column_exists(&conn, "maker_shadow", "inventory_skew"));
        }

        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    /// Re-running init over an already-migrated database is a no-op, so a
    /// restart never re-ALTERs or duplicates anything.
    #[test]
    fn migration_is_idempotent() {
        let path = tmp_path("idempotent");
        legacy_db(&path);

        let db = Database::new(path.to_str().unwrap()).expect("first migrate");
        drop(db);
        let db = Database::new(path.to_str().unwrap()).expect("second migrate");
        let db2 = Database::new(path.to_str().unwrap()).expect("third migrate");

        assert_eq!(count(&db, "trades"), 2);
        assert_eq!(count(&db, "signals"), 3);
        assert_eq!(count(&db2, "maker_shadow"), 0);

        drop(db);
        drop(db2);
        let _ = std::fs::remove_file(&path);
    }

    fn quote(side: Side, bid_fill: bool) -> ShadowQuote {
        ShadowQuote {
            side,
            decision_delta: 0.08,
            secs_left: 90,
            fair_value: 0.60,
            our_bid: 0.56,
            our_ask: 0.64,
            market_bid: Some(0.55),
            market_ask: if bid_fill { Some(0.56) } else { Some(0.60) },
            would_bid_fill: bid_fill,
            would_ask_fill: false,
        }
    }

    /// THE KEY MEASUREMENT, end to end: a simulated bid fill on the side that
    /// wins scores 1, one on the side that loses scores 0, and a row with no
    /// simulated fill is left unscored rather than counted as a miss.
    #[test]
    fn resolution_scores_bid_fills_against_the_actual_outcome() {
        let path = tmp_path("scoring");
        let db = Database::new(path.to_str().unwrap()).expect("open");

        // Up filled, Down filled, and an Up row with no fill.
        db.insert_maker_shadow(300, 1_000, &quote(Side::Up, true));
        db.insert_maker_shadow(300, 1_001, &quote(Side::Down, true));
        db.insert_maker_shadow(300, 1_002, &quote(Side::Up, false));

        let updated = db.resolve_maker_shadow(300, "Up");
        assert_eq!(updated, 3, "every row in the window must be labelled");

        let conn_scores = {
            let conn = db.conn.lock().unwrap();
            let mut stmt = conn
                .prepare(
                    "SELECT timestamp_ms, fill_was_correct FROM maker_shadow
                      ORDER BY timestamp_ms",
                )
                .unwrap();
            let rows: Vec<(i64, Option<i64>)> = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap()
                .filter_map(|r| r.ok())
                .collect();
            rows
        };
        assert_eq!(
            conn_scores,
            vec![(1_000, Some(1)), (1_001, Some(0)), (1_002, None)],
            "bid-fill scoring is wrong"
        );

        // The summary agrees: two bid fills, one of them on the winner.
        let s = db.maker_shadow_summary(200);
        assert_eq!(s.rows, 3);
        assert_eq!(s.bid_fills, 2);
        assert_eq!(s.bid_fills_on_winner, 1);
        assert_eq!(db.maker_shadow_pending(), 0);

        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    /// Resolution must not relabel a window that already settled.
    #[test]
    fn resolution_does_not_relabel_already_resolved_rows() {
        let path = tmp_path("relabel");
        let db = Database::new(path.to_str().unwrap()).expect("open");

        db.insert_maker_shadow(300, 1_000, &quote(Side::Up, true));
        assert_eq!(db.resolve_maker_shadow(300, "Up"), 1);
        // A second, contradictory resolution for the same window changes nothing.
        assert_eq!(db.resolve_maker_shadow(300, "Down"), 0);

        let s = db.maker_shadow_summary(200);
        assert_eq!(s.bid_fills_on_winner, 1, "row was relabelled");

        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    /// Unresolved rows are excluded from the summary: a fill with no outcome
    /// cannot be scored, and counting it would drag every rate toward zero.
    #[test]
    fn summary_ignores_unresolved_rows() {
        let path = tmp_path("pending");
        let db = Database::new(path.to_str().unwrap()).expect("open");

        db.insert_maker_shadow(300, 1_000, &quote(Side::Up, true));
        db.insert_maker_shadow(600, 2_000, &quote(Side::Up, true));
        db.resolve_maker_shadow(300, "Up");

        let s = db.maker_shadow_summary(200);
        assert_eq!(s.rows, 1, "unresolved row leaked into the summary");
        assert_eq!(s.bid_fills, 1);
        assert_eq!(db.maker_shadow_pending(), 1);

        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    /// An empty table must not panic or report bogus aggregates.
    #[test]
    fn summary_on_an_empty_table_is_all_zeroes() {
        let path = tmp_path("empty");
        let db = Database::new(path.to_str().unwrap()).expect("open");
        let s = db.maker_shadow_summary(200);
        assert_eq!(s.rows, 0);
        assert_eq!(s.bid_fills, 0);
        assert_eq!(s.mean_abs_fv_minus_mid, None);
        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    /// Manual check against a COPY of the production database. Ignored by
    /// default because it needs a real file; run it before deploying:
    ///
    ///   LIVE_DB_COPY=/path/to/copy-of-trades.db cargo test -- --ignored
    ///
    /// It asserts nothing about specific counts (they grow); it prints them and
    /// verifies the migration did not destroy rows or leave the schema short.
    #[test]
    #[ignore = "needs LIVE_DB_COPY pointing at a copy of the production database"]
    fn migration_preserves_a_live_database_copy() {
        let path = match std::env::var("LIVE_DB_COPY") {
            Ok(p) => p,
            Err(_) => panic!("set LIVE_DB_COPY to a COPY of the production database"),
        };

        // Counts BEFORE the migration runs.
        let before: Vec<(String, i64)> = {
            let conn = Connection::open(&path).expect("open copy");
            // -1 means the table does not exist yet on this copy, which is a
            // legitimate pre-migration state and is asserted against below.
            [
                "trades",
                "signals",
                "twap_observations",
                "daily_stats",
                "maker_shadow",
                "delta_samples",
            ]
            .iter()
            .map(|t| {
                let n: i64 = conn
                    .query_row(&format!("SELECT COUNT(*) FROM {}", t), [], |r| r.get(0))
                    .unwrap_or(-1);
                ((*t).to_string(), n)
            })
            .collect()
        };

        let db = Database::new(&path).expect("migrate live copy");

        for (table, n_before) in &before {
            let n_after = count(&db, table);
            if *n_before < 0 {
                // Table created by this migration: it must now exist and be
                // empty, never populated with invented rows.
                println!("{:<20} before=(absent) after={}", table, n_after);
                assert_eq!(
                    n_after, 0,
                    "{} was created by the migration but is not empty",
                    table
                );
                continue;
            }
            println!("{:<20} before={} after={}", table, n_before, n_after);
            assert_eq!(
                *n_before, n_after,
                "migration changed the row count of {}",
                table
            );
        }

        let conn = db.conn.lock().unwrap();
        assert!(column_exists(&conn, "maker_shadow", "fill_was_correct"));
        assert!(column_exists(&conn, "signals", "decision_delta"));
        assert!(column_exists(&conn, "delta_samples", "twap_delta_pct"));
        assert!(column_exists(&conn, "delta_samples", "actual_resolution"));
    }
}

#[cfg(test)]
mod delta_sample_tests {
    use super::*;

    fn tmp(name: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("btc5min-ds-{}-{}.db", name, std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    fn sample(delta: Option<f64>, secs_left: i64) -> DeltaSample {
        DeltaSample {
            secs_left,
            twap_delta_pct: delta,
            spot_delta_pct: delta,
            twap_strike: Some("65000000000000000000000".into()),
            twap_current: Some("65010000000000000000000".into()),
            up_ask: Some(0.52),
            up_bid: Some(0.50),
            down_ask: Some(0.50),
            down_bid: Some(0.48),
        }
    }

    /// THE POINT OF THE TABLE: a delta of ~0, which no strategy gate would ever
    /// let through, is stored and retrievable. This is the band the
    /// signals-based and maker_shadow-based fits had no observations of.
    #[test]
    fn near_zero_deltas_are_recorded() {
        let path = tmp("midband");
        let db = Database::new(path.to_str().unwrap()).expect("open");

        for (i, d) in [0.0, 0.001, -0.004, 0.012, -0.031, 0.049].iter().enumerate() {
            db.insert_delta_sample(300, 1_000 + i as i64, &sample(Some(*d), 200));
        }

        let cov = db.delta_sample_coverage();
        assert_eq!(cov.rows, 6);
        assert_eq!(
            cov.mid_band_rows, 6,
            "every one of these is inside |delta| < 0.05 and must be counted"
        );

        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    /// A window the bot never traded still gets its samples labelled. Those
    /// windows are the majority and carry the observations the model lacks.
    #[test]
    fn untraded_windows_are_backfilled_at_resolution() {
        let path = tmp("untraded");
        let db = Database::new(path.to_str().unwrap()).expect("open");

        // No trade is ever inserted for window 300.
        db.insert_delta_sample(300, 1_000, &sample(Some(0.01), 250));
        db.insert_delta_sample(300, 6_000, &sample(Some(0.02), 245));

        let labelled = db.resolve_delta_samples(300, "Up");
        assert_eq!(labelled, 2, "untraded window was not backfilled");
        assert!(db.get_trade_by_window_ts(300).is_none(), "fixture sanity");

        let cov = db.delta_sample_coverage();
        assert_eq!(cov.resolved_rows, 2);
        assert_eq!(cov.resolved_windows, 1);
        assert_eq!(cov.mid_band_resolved, 2);

        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    /// Backfill is idempotent and never relabels a settled sample.
    #[test]
    fn backfill_does_not_relabel() {
        let path = tmp("relabel");
        let db = Database::new(path.to_str().unwrap()).expect("open");

        db.insert_delta_sample(300, 1_000, &sample(Some(0.01), 250));
        assert_eq!(db.resolve_delta_samples(300, "Up"), 1);
        assert_eq!(db.resolve_delta_samples(300, "Down"), 0);

        let conn_res: String = {
            let conn = db.conn.lock().unwrap();
            conn.query_row(
                "SELECT actual_resolution FROM delta_samples WHERE window_ts = 300",
                [],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(conn_res, "Up");

        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    /// Unavailable inputs are stored as NULL, never substituted. A filled-in
    /// value would be indistinguishable from a real observation in the fit.
    #[test]
    fn unavailable_fields_are_null_not_substituted() {
        let path = tmp("nulls");
        let db = Database::new(path.to_str().unwrap()).expect("open");

        // Stale TWAP and an entirely empty book.
        db.insert_delta_sample(
            300,
            1_000,
            &DeltaSample {
                secs_left: 120,
                ..Default::default()
            },
        );

        let conn = db.conn.lock().unwrap();
        let (twap, spot, up_ask, strike): (
            Option<f64>,
            Option<f64>,
            Option<f64>,
            Option<String>,
        ) = conn
            .query_row(
                "SELECT twap_delta_pct, spot_delta_pct, up_ask, twap_strike
                   FROM delta_samples WHERE window_ts = 300",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(twap, None);
        assert_eq!(spot, None);
        assert_eq!(up_ask, None);
        assert_eq!(strike, None);
        drop(conn);

        // A NULL delta is not counted as being in the mid band.
        assert_eq!(db.delta_sample_coverage().mid_band_rows, 0);

        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    /// A duplicate (window_ts, timestamp_ms) is ignored rather than erroring,
    /// and does not double-count.
    #[test]
    fn duplicate_samples_are_ignored() {
        let path = tmp("dupe");
        let db = Database::new(path.to_str().unwrap()).expect("open");
        db.insert_delta_sample(300, 1_000, &sample(Some(0.01), 250));
        db.insert_delta_sample(300, 1_000, &sample(Some(0.99), 250));
        assert_eq!(db.delta_sample_coverage().rows, 1);
        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    /// Samples span the whole window, including the stretch before any signal
    /// could qualify and after a window would have resolved.
    #[test]
    fn samples_span_the_entire_window() {
        let path = tmp("span");
        let db = Database::new(path.to_str().unwrap()).expect("open");

        // 60 samples at 5s over a 300s window, delta drifting through zero.
        for i in 0..60i64 {
            let secs_left = 300 - i * 5;
            let delta = (i as f64 - 30.0) * 0.004; // -0.12 .. +0.116
            db.insert_delta_sample(300, i * 5_000, &sample(Some(delta), secs_left));
        }
        db.resolve_delta_samples(300, "Up");

        let cov = db.delta_sample_coverage();
        assert_eq!(cov.rows, 60);
        assert_eq!(cov.resolved_rows, 60);
        // Roughly a quarter of that sweep lies inside |delta| < 0.05.
        assert!(
            cov.mid_band_rows >= 20,
            "expected substantial mid-band coverage, got {}",
            cov.mid_band_rows
        );

        let conn = db.conn.lock().unwrap();
        let (lo, hi): (i64, i64) = conn
            .query_row(
                "SELECT MIN(secs_left), MAX(secs_left) FROM delta_samples",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((lo, hi), (5, 300), "samples must span the full window");

        drop(conn);
        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    /// The sampler's cadence is the ONLY gate. This mirrors the main-loop
    /// condition exactly: nothing about threshold, trend, side, pause or
    /// resolution participates, so a state that every strategy gate rejects
    /// still produces a sample on schedule.
    #[test]
    fn cadence_is_the_only_gate() {
        let mut last: Option<i64> = None;
        let mut written = 0;
        let mut now = 0i64;
        while now < 300_000 {
            let due = match last {
                Some(l) => now.saturating_sub(l) >= crate::constants::DELTA_SAMPLE_INTERVAL_MS,
                None => true,
            };
            if due {
                written += 1;
                last = Some(now);
            }
            now += 250;
        }
        assert_eq!(
            written, 60,
            "expected 60 samples per 300s window at a 5s cadence, got {}",
            written
        );
    }
}
