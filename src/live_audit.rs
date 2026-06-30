use crate::client::{get_now_ms, MarketWindow};
use crate::live_executor::{LiveAccountStatus, LiveExecutionResult};
use crate::strategy::{OrderOperation, OrderSignal};
use rusqlite::{params, Connection, Result};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct LiveAudit {
    path: PathBuf,
}

impl LiveAudit {
    pub fn new(run_log_dir: &str) -> Result<Self> {
        let path = Path::new(run_log_dir).join("live_audit.sqlite3");
        let audit = Self { path };
        audit.ensure_schema()?;
        Ok(audit)
    }

    fn connect(&self) -> Result<Connection> {
        Connection::open(&self.path)
    }

    fn ensure_schema(&self) -> Result<()> {
        let conn = self.connect()?;
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS balance_snapshots (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                ts_ms INTEGER NOT NULL,
                window_number INTEGER,
                mode TEXT NOT NULL,
                clob_balance REAL NOT NULL,
                paper_cash REAL NOT NULL,
                trade_cash REAL NOT NULL,
                pending_redeem_est REAL NOT NULL,
                ready INTEGER NOT NULL,
                error TEXT
            );

            CREATE TABLE IF NOT EXISTS order_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                ts_ms INTEGER NOT NULL,
                window_number INTEGER NOT NULL,
                slug TEXT NOT NULL,
                mode TEXT NOT NULL,
                operation TEXT NOT NULL,
                side TEXT NOT NULL,
                order_type TEXT NOT NULL,
                price REAL NOT NULL,
                amount REAL NOT NULL,
                executed INTEGER NOT NULL,
                submitted INTEGER NOT NULL,
                dry_run INTEGER NOT NULL,
                reject_reason TEXT,
                order_id TEXT,
                status TEXT,
                fill_usd REAL,
                fill_shares REAL,
                fill_avg_price REAL,
                raw_making TEXT,
                raw_taking TEXT,
                reason TEXT NOT NULL
            );
            "#,
        )?;
        Ok(())
    }

    pub fn record_balance(
        &self,
        account: &LiveAccountStatus,
        paper_cash: f64,
        trade_cash: f64,
    ) -> Result<()> {
        let conn = self.connect()?;
        let pending_redeem_est = (paper_cash - account.balance_usd).max(0.0);
        let mode = if account.dry_run { "dry-run" } else { "live" };
        conn.execute(
            r#"
            INSERT INTO balance_snapshots (
                ts_ms, window_number, mode, clob_balance, paper_cash, trade_cash,
                pending_redeem_est, ready, error
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
            "#,
            params![
                get_now_ms(),
                account.window_number.map(|v| v as i64),
                mode,
                account.balance_usd,
                paper_cash,
                trade_cash,
                pending_redeem_est,
                if account.ready_to_trade { 1 } else { 0 },
                account.last_error.as_deref(),
            ],
        )?;
        Ok(())
    }

    pub fn record_order(
        &self,
        window_number: usize,
        market: &MarketWindow,
        signal: &OrderSignal,
        result: &LiveExecutionResult,
    ) -> Result<()> {
        let conn = self.connect()?;
        let operation = match signal.operation() {
            OrderOperation::Buy => "BUY",
            OrderOperation::Sell => "SELL",
        };
        let mode = if result.dry_run { "dry-run" } else { "live" };
        let order_type = result
            .intent
            .as_ref()
            .map(|intent| format!("market_{}", intent.clob_order_type))
            .unwrap_or_else(|| "market".to_string());
        let (fill_usd, fill_shares, fill_avg_price) = result
            .fill
            .as_ref()
            .map(|fill| {
                (
                    Some(fill.amount_usd),
                    Some(fill.shares),
                    Some(fill.avg_price),
                )
            })
            .unwrap_or((None, None, None));

        conn.execute(
            r#"
            INSERT INTO order_events (
                ts_ms, window_number, slug, mode, operation, side, order_type,
                price, amount, executed, submitted, dry_run, reject_reason, order_id,
                status, fill_usd, fill_shares, fill_avg_price, raw_making, raw_taking, reason
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21)
            "#,
            params![
                get_now_ms(),
                window_number as i64,
                market.slug.as_str(),
                mode,
                operation,
                signal.side.as_str(),
                order_type,
                signal.price,
                signal.amount,
                if result.executed { 1 } else { 0 },
                if result.submitted { 1 } else { 0 },
                if result.dry_run { 1 } else { 0 },
                if result.reject_reason.is_empty() {
                    None
                } else {
                    Some(result.reject_reason.as_str())
                },
                result.order_id.as_deref(),
                result.status.as_deref(),
                fill_usd,
                fill_shares,
                fill_avg_price,
                result.raw_making_amount.as_deref(),
                result.raw_taking_amount.as_deref(),
                signal.reason.as_str(),
            ],
        )?;
        Ok(())
    }
}
