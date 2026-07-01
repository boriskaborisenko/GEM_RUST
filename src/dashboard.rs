//! Shared dashboard snapshot for terminal and HTTP server modes.

use crate::config::ExecutionMode;
use crate::live_executor::LiveAccountStatus;
use crate::trader::PortfolioSnapshot;
use crate::window_chart::SnapshotChartPoint;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DashboardSnapshot {
    pub meta: SnapshotMeta,
    pub execution: SnapshotExecution,
    pub live_account: Option<SnapshotLiveAccount>,
    pub portfolio: PortfolioSnapshot,
    pub current_window: Option<SnapshotWindow>,
    pub next_window: Option<SnapshotWindow>,
    pub system_logs: Vec<String>,
    pub session_stats: SnapshotSessionStats,
    pub run_log_dir: String,
    pub updated_at_ms: i64,
    /// ANSI-colored lines — same output as the terminal dashboard.
    pub terminal_lines: Vec<String>,
    /// UP/DOWN ask history for current window (web chart).
    pub chart: Vec<SnapshotChartPoint>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SnapshotMeta {
    pub asset: String,
    pub interval: String,
    pub strategy: String,
    pub started_at_ms: i64,
    pub runtime_ms: i64,
    pub shutdown_pending: bool,
    pub spot_price: Option<f64>,
    pub atr: f64,
    pub llm_enabled: bool,
    pub llm_correct: u32,
    pub llm_wrong: u32,
    pub maintenance_label: String,
    pub maintenance_ok: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SnapshotExecution {
    pub mode: String,
    pub dry_run: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SnapshotLiveAccount {
    pub authenticated: bool,
    pub balance_usd: f64,
    pub allowance_contracts: usize,
    pub ready_to_trade: bool,
    pub dry_run: bool,
    pub signer_address: String,
    pub funder_address: String,
    pub last_error: Option<String>,
    pub updated_at_ms: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SnapshotTrade {
    pub timestamp: i64,
    pub trade_type: String,
    pub side: String,
    pub reason: String,
    pub price: f64,
    pub shares: f64,
    pub usd_value: f64,
    pub available_cash_after: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SnapshotWindow {
    pub window_number: usize,
    pub slug: String,
    pub status: String,
    pub role: String,
    pub up_bid: f64,
    pub up_ask: f64,
    pub down_bid: f64,
    pub down_ask: f64,
    pub spent: f64,
    pub cash_returned: f64,
    pub up_shares: f64,
    pub down_shares: f64,
    pub trade_count: usize,
    pub trades: Vec<SnapshotTrade>,
    pub ptb: Option<f64>,
    pub window_start_ms: i64,
    pub window_end_ms: i64,
    pub time_left_sec: i64,
    pub time_elapsed_pct: f64,
    pub combined_ask: f64,
    pub up_chance_pct: f64,
    pub down_chance_pct: f64,
    pub est_value: f64,
    pub unrealized_pnl: f64,
    pub paired_shares: f64,
    pub paired_floor: f64,
    pub breakeven_gap: f64,
    pub spot_distance: Option<f64>,
    pub spot_distance_pct: Option<f64>,
    pub implied_winner: Option<String>,
    pub cex_micro: Option<SnapshotCexMicro>,
    pub strategy_j: Option<SnapshotStrategyJ>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SnapshotCexMicro {
    pub velocity_3s: Option<f64>,
    pub imbalance_3s: f64,
    pub lead_bps: Option<f64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SnapshotStrategyJ {
    pub clips_done: u32,
    pub clips_max: Option<u32>,
    pub entry_active: bool,
    pub phase: String,
    pub target_profit_usd: f64,
    pub redeem_pnl_proj: f64,
    pub winner: String,
    pub winner_ask: f64,
    pub gap_z: f64,
    pub tier: String,
    pub tier_note: String,
    pub flip_hedge_armed: bool,
    pub tape_hot: bool,
    pub tape_usd: f64,
    pub tape_buys: u32,
    pub tape_need_usd: f64,
    pub tape_need_buys: u32,
    pub ask_depth_usd: f64,
    pub ask_depth_max_cents: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SnapshotSessionStats {
    pub summary_line: String,
}

impl From<&LiveAccountStatus> for SnapshotLiveAccount {
    fn from(a: &LiveAccountStatus) -> Self {
        Self {
            authenticated: a.authenticated,
            balance_usd: a.balance_usd,
            allowance_contracts: a.allowance_contracts,
            ready_to_trade: a.ready_to_trade,
            dry_run: a.dry_run,
            signer_address: a.signer_address.clone(),
            funder_address: a.funder_address.clone(),
            last_error: a.last_error.clone(),
            updated_at_ms: a.updated_at_ms,
        }
    }
}

pub fn window_to_snapshot(
    win: &crate::trader::WindowState,
    spot_price: Option<f64>,
    now_ms: i64,
) -> SnapshotWindow {
    let window_start_ms = chrono::DateTime::parse_from_rfc3339(&win.market.start_time)
        .map(|dt| dt.timestamp_millis())
        .unwrap_or(0);
    let window_end_ms = chrono::DateTime::parse_from_rfc3339(&win.market.end_time)
        .map(|dt| dt.timestamp_millis())
        .unwrap_or(0);
    let duration_ms = (window_end_ms - window_start_ms).max(1);
    let time_left_sec = if window_end_ms > 0 {
        ((window_end_ms - now_ms) / 1000).max(0)
    } else {
        0
    };
    let elapsed_ms = (now_ms - window_start_ms).clamp(0, duration_ms);
    let time_elapsed_pct = (elapsed_ms as f64 / duration_ms as f64) * 100.0;

    let up = &win.prices.up;
    let dn = &win.prices.down;
    let up_ref = if up.ask > 0.0 { up.ask } else { up.bid };
    let dn_ref = if dn.ask > 0.0 { dn.ask } else { dn.bid };
    let combined_ask = up.ask + dn.ask;
    let up_chance_pct = (up_ref * 100.0).clamp(0.0, 100.0);
    let down_chance_pct = (dn_ref * 100.0).clamp(0.0, 100.0);

    let mtm = win.up_shares * up.bid + win.down_shares * dn.bid;
    let est_value = win.cash_returned + mtm;
    let unrealized_pnl = est_value - win.spent;
    let paired_shares = win.up_shares.min(win.down_shares);
    let paired_floor = win.cash_returned + paired_shares;
    let breakeven_gap = paired_floor - win.spent;

    let (spot_distance, spot_distance_pct) = win
        .market
        .get_ptb_deviation(spot_price)
        .map(|(delta, pct)| (Some(delta), Some(pct)))
        .unwrap_or((None, None));

    let implied_winner = match (spot_price, win.market.price_to_beat) {
        (Some(spot), Some(ptb)) if spot > ptb => Some("UP".to_string()),
        (Some(spot), Some(ptb)) if spot < ptb => Some("DOWN".to_string()),
        (Some(_), Some(_)) => Some("TIE".to_string()),
        _ => None,
    };

    SnapshotWindow {
        window_number: win.window_number,
        slug: win.market.slug.clone(),
        status: win.status.clone(),
        role: win.role.clone(),
        up_bid: win.prices.up.bid,
        up_ask: win.prices.up.ask,
        down_bid: win.prices.down.bid,
        down_ask: win.prices.down.ask,
        spent: win.spent,
        cash_returned: win.cash_returned,
        up_shares: win.up_shares,
        down_shares: win.down_shares,
        trade_count: win.trades.len(),
        ptb: win.market.price_to_beat,
        window_start_ms,
        window_end_ms,
        trades: win
            .trades
            .iter()
            .map(|t| SnapshotTrade {
                timestamp: t.timestamp,
                trade_type: t.trade_type.clone(),
                side: t.side.clone(),
                reason: t.reason.clone(),
                price: t.price,
                shares: t.shares,
                usd_value: t.usd_value,
                available_cash_after: t.available_cash_after,
            })
            .collect(),
        time_left_sec,
        time_elapsed_pct,
        combined_ask,
        up_chance_pct,
        down_chance_pct,
        est_value,
        unrealized_pnl,
        paired_shares,
        paired_floor,
        breakeven_gap,
        spot_distance,
        spot_distance_pct,
        implied_winner,
        cex_micro: None,
        strategy_j: None,
    }
}

pub fn execution_mode_label(mode: ExecutionMode) -> &'static str {
    match mode {
        ExecutionMode::Paper => "paper",
        ExecutionMode::Live => "live",
    }
}

impl DashboardSnapshot {
    pub fn bootstrap() -> Self {
        Self {
            meta: SnapshotMeta {
                asset: String::new(),
                interval: String::new(),
                strategy: String::new(),
                started_at_ms: 0,
                runtime_ms: 0,
                shutdown_pending: false,
                spot_price: None,
                atr: 0.0,
                llm_enabled: false,
                llm_correct: 0,
                llm_wrong: 0,
                maintenance_label: "OK".to_string(),
                maintenance_ok: true,
            },
            execution: SnapshotExecution {
                mode: "paper".to_string(),
                dry_run: false,
            },
            live_account: None,
            portfolio: PortfolioSnapshot {
                starting_bank: 0.0,
                available_cash: 0.0,
                overall_realized_pnl: 0.0,
                equity: 0.0,
                total_windows: 0,
                traded_windows: 0,
                open_traded_windows: 0,
                no_trade_windows: 0,
                open_waiting_windows: 0,
                entered_windows: 0,
                closed_windows: 0,
                wins: 0,
                losses: 0,
                h_market_wins: 0,
                h_market_losses: 0,
                h_salvage_escapes: 0,
                h_salvage_wins: 0,
                skipped_windows: 0,
            },
            current_window: None,
            next_window: None,
            system_logs: vec!["Starting…".to_string()],
            session_stats: SnapshotSessionStats {
                summary_line: String::new(),
            },
            run_log_dir: String::new(),
            updated_at_ms: 0,
            terminal_lines: vec!["Starting GEM server…".to_string()],
            chart: vec![],
        }
    }
}
