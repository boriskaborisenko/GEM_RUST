use crate::client::{MarketWindow, PricesState};
use crate::config::Config;
use crate::strategy::{
    EntryMode, EntrySignal, OrderSignal, SpotSignalSnapshot, StrategyState, TradeStrategy,
};
use crate::trader::WindowState;
use std::collections::{HashMap, HashSet};

const D1_ENTRY_BUDGET_MULTIPLIER: f64 = 0.35;
const D1_SCOUT_BUDGET_MULTIPLIER: f64 = 0.15;
const D1_SCOUT_VOLATILE_BUDGET_MULTIPLIER: f64 = 0.10;
const D1_SCOUT_STORM_BUDGET_MULTIPLIER: f64 = 0.10;
const D1_FIRST_LEG_MIN_ASK: f64 = 0.48;
const D1_FIRST_LEG_MAX_ASK: f64 = 0.52;
const D1_MAX_PRESTART_COMBINED_ASK: f64 = 1.03;
const D1_LOCK_PAIR_MAX_COST: f64 = 0.99;
const D1_MIN_ENTRY_EDGE: f64 = 0.005;
const D1_DIRECTIONAL_FAIR_MIN: f64 = 0.54;
const D1_SCOUT_MAX_FAIR: f64 = 0.53;
const D1_SCOUT_MIN_PRICE_DISCOUNT: f64 = 0.015;
const D1_SCOUT_VOLATILE_MIN_PRICE_DISCOUNT: f64 = 0.025;
const D1_SCOUT_STORM_MIN_PRICE_DISCOUNT: f64 = 0.035;
const D1_VELOCITY_ENTRY_DRIFT_FRACTION: f64 = 0.25;
const D1_LIVE_CONVICTION_BUDGET_MULTIPLIER: f64 = 0.35;
const D1_LIVE_MIN_ABS_Z: f64 = 0.55;
const D1_LIVE_RICH_MIN_ABS_Z: f64 = 0.72;
const D1_LIVE_MIN_ENTRY_EDGE: f64 = 0.03;
const D1_LIVE_MIN_EXPECTED_MOVE_USD: f64 = 20.0;
const D1_LIVE_MAX_ENTRY_ASK: f64 = 0.65;
const D1_LIVE_RICH_ENTRY_ASK: f64 = 0.57;
const D1_LIVE_MAX_TIME_PCT: f64 = 70.0;
const D1_LIVE_MIN_VELOCITY_CONFIRM_USD_PER_SEC: f64 = 0.10;
const D1_LIVE_MIN_ACCEL_CONFIRM_USD_PER_SEC2: f64 = 0.01;
const D1_MIN_VALID_ATR: f64 = 1.0;
const D1_RUNAWAY_OPPOSITE_MAX_ASK: f64 = 0.68;
const D1_REPAIR_PAIR_MAX_COST: f64 = 0.96;
const D1_REPAIR_MAX_ROUNDS: usize = 2;
const D1_REPAIR_MAX_PAIR_FRACTION: f64 = 0.75;
const D1_REPAIR_MAX_USD_FRACTION_OF_SPENT: f64 = 0.75;
const D1_MIN_REPAIR_PAIR_SHARES: f64 = 0.25;
const D1_MIN_INSURANCE_HEDGE_FRACTION: f64 = 0.08;
const D1_MIN_LOCK_PAIR_SHARES: f64 = 0.25;
const D1_MIN_TRADE_USD: f64 = 1.0;
const D1_SLEEP_TIME_PCT: f64 = 5.0;
const D1_SLEEP_MIN_SECONDS: f64 = 25.0;

pub struct DynamicGridD1Strategy {
    pub entered_windows: HashSet<usize>,
    pub states: HashMap<usize, StrategyState>,
    pub first_leg_side: HashMap<usize, String>,
    pub first_leg_price: HashMap<usize, f64>,
    pub repair_rounds: HashMap<usize, usize>,
    pub pending_repair_side: HashMap<usize, String>,
    pub pending_repair_shares: HashMap<usize, f64>,
    pub pending_repair_price: HashMap<usize, f64>,
}

impl DynamicGridD1Strategy {
    pub fn new() -> Self {
        Self {
            entered_windows: HashSet::new(),
            states: HashMap::new(),
            first_leg_side: HashMap::new(),
            first_leg_price: HashMap::new(),
            repair_rounds: HashMap::new(),
            pending_repair_side: HashMap::new(),
            pending_repair_shares: HashMap::new(),
            pending_repair_price: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum D1Phase {
    Opening,
    Mid,
    Late,
    Final,
}

impl D1Phase {
    fn from_time_pct(time_pct: f64) -> Self {
        if time_pct < 25.0 {
            Self::Opening
        } else if time_pct < 60.0 {
            Self::Mid
        } else if time_pct < 85.0 {
            Self::Late
        } else {
            Self::Final
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Opening => "opening",
            Self::Mid => "mid",
            Self::Late => "late",
            Self::Final => "final",
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum D1AtrRegime {
    Calm,
    Normal,
    Volatile,
    Storm,
}

impl D1AtrRegime {
    fn from_atr(atr: f64) -> Self {
        if atr < 20.0 {
            Self::Calm
        } else if atr < 45.0 {
            Self::Normal
        } else if atr < 90.0 {
            Self::Volatile
        } else {
            Self::Storm
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Calm => "calm",
            Self::Normal => "normal",
            Self::Volatile => "volatile",
            Self::Storm => "storm",
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct D1PhasePolicy {
    lock_pair_max_cost: f64,
    runaway_bid_soft: f64,
    runaway_bid_hard: f64,
    runaway_min_time_pct: f64,
    runaway_opposite_max_ask: f64,
    repair_pair_max_cost: f64,
}

fn phase_policy(phase: D1Phase, atr_regime: D1AtrRegime) -> D1PhasePolicy {
    let mut policy = match phase {
        D1Phase::Opening => D1PhasePolicy {
            lock_pair_max_cost: 0.990,
            runaway_bid_soft: 0.30,
            runaway_bid_hard: 0.26,
            runaway_min_time_pct: 18.0,
            runaway_opposite_max_ask: 0.72,
            repair_pair_max_cost: 0.950,
        },
        D1Phase::Mid => D1PhasePolicy {
            lock_pair_max_cost: 0.995,
            runaway_bid_soft: 0.36,
            runaway_bid_hard: 0.31,
            runaway_min_time_pct: 25.0,
            runaway_opposite_max_ask: D1_RUNAWAY_OPPOSITE_MAX_ASK,
            repair_pair_max_cost: D1_REPAIR_PAIR_MAX_COST,
        },
        D1Phase::Late => D1PhasePolicy {
            lock_pair_max_cost: 1.000,
            runaway_bid_soft: 0.41,
            runaway_bid_hard: 0.36,
            runaway_min_time_pct: 60.0,
            runaway_opposite_max_ask: 0.64,
            repair_pair_max_cost: 0.970,
        },
        D1Phase::Final => D1PhasePolicy {
            lock_pair_max_cost: 1.005,
            runaway_bid_soft: 0.48,
            runaway_bid_hard: 0.42,
            runaway_min_time_pct: 85.0,
            runaway_opposite_max_ask: 0.58,
            repair_pair_max_cost: 0.980,
        },
    };

    match atr_regime {
        D1AtrRegime::Calm => {
            policy.runaway_bid_soft -= 0.03;
            policy.runaway_bid_hard -= 0.03;
            policy.runaway_min_time_pct += 6.0;
        }
        D1AtrRegime::Volatile => {
            policy.runaway_bid_soft += 0.03;
            policy.runaway_bid_hard += 0.02;
            policy.runaway_min_time_pct -= 4.0;
        }
        D1AtrRegime::Storm => {
            policy.runaway_bid_soft += 0.06;
            policy.runaway_bid_hard += 0.04;
            policy.runaway_min_time_pct -= 8.0;
            policy.lock_pair_max_cost += 0.005;
        }
        D1AtrRegime::Normal => {}
    }

    policy
}

fn insurance_quality(insurance_cost: f64) -> f64 {
    if insurance_cost <= 0.0 {
        1.0
    } else if insurance_cost <= 0.03 {
        0.75
    } else if insurance_cost <= 0.06 {
        0.45
    } else if insurance_cost <= 0.10 {
        0.22
    } else if insurance_cost <= 0.15 {
        0.08
    } else {
        0.0
    }
}

fn atr_pressure(atr_regime: D1AtrRegime) -> f64 {
    match atr_regime {
        D1AtrRegime::Calm => 0.10,
        D1AtrRegime::Normal => 0.25,
        D1AtrRegime::Volatile => 0.48,
        D1AtrRegime::Storm => 0.70,
    }
}

fn directional_budget_multiplier(atr_regime: D1AtrRegime) -> f64 {
    match atr_regime {
        D1AtrRegime::Calm => D1_ENTRY_BUDGET_MULTIPLIER * 0.85,
        D1AtrRegime::Normal => D1_ENTRY_BUDGET_MULTIPLIER,
        D1AtrRegime::Volatile => D1_ENTRY_BUDGET_MULTIPLIER * 0.70,
        D1AtrRegime::Storm => D1_ENTRY_BUDGET_MULTIPLIER * 0.50,
    }
}

fn scout_price_discount_requirement(atr_regime: D1AtrRegime) -> f64 {
    match atr_regime {
        D1AtrRegime::Calm | D1AtrRegime::Normal => D1_SCOUT_MIN_PRICE_DISCOUNT,
        D1AtrRegime::Volatile => D1_SCOUT_VOLATILE_MIN_PRICE_DISCOUNT,
        D1AtrRegime::Storm => D1_SCOUT_STORM_MIN_PRICE_DISCOUNT,
    }
}

fn scout_budget_multiplier(atr_regime: D1AtrRegime) -> f64 {
    match atr_regime {
        D1AtrRegime::Calm | D1AtrRegime::Normal => D1_SCOUT_BUDGET_MULTIPLIER,
        D1AtrRegime::Volatile => D1_SCOUT_VOLATILE_BUDGET_MULTIPLIER,
        D1AtrRegime::Storm => D1_SCOUT_STORM_BUDGET_MULTIPLIER,
    }
}

fn time_pressure(time_pct: f64) -> f64 {
    if time_pct < 25.0 {
        0.10
    } else if time_pct < 60.0 {
        0.32
    } else if time_pct < 85.0 {
        0.66
    } else {
        1.0
    }
}

fn insurance_risk_pressure(
    first_bid: f64,
    first_win_probability: f64,
    time_pct: f64,
    atr_regime: D1AtrRegime,
    policy: D1PhasePolicy,
    first_otm_z: f64,
) -> f64 {
    let prob_pressure = ((0.50 - first_win_probability) / 0.50).clamp(0.0, 1.0);
    let distance_pressure = ((first_otm_z - 0.25) / 0.95).clamp(0.0, 1.0);
    let market_pressure = if first_bid <= policy.runaway_bid_hard {
        1.0
    } else if first_bid <= policy.runaway_bid_soft && time_pct >= policy.runaway_min_time_pct {
        0.72
    } else if time_pct >= 85.0 && first_win_probability < 0.32 {
        0.35
    } else {
        0.0
    };

    if market_pressure <= 0.0 {
        return 0.0;
    }
    if distance_pressure <= 0.0 && first_win_probability > 0.35 {
        return 0.0;
    }

    let raw_pressure = prob_pressure * 0.48
        + time_pressure(time_pct) * 0.20
        + atr_pressure(atr_regime) * 0.12
        + distance_pressure * 0.20;
    (raw_pressure * market_pressure).clamp(0.0, 1.0)
}

fn first_side_otm_z(
    first_side: &str,
    market: &MarketWindow,
    spot_price: Option<f64>,
    current_atr: f64,
    secs_left: i64,
) -> f64 {
    let (Some(spot), Some(ptb)) = (spot_price, market.price_to_beat) else {
        return 0.0;
    };
    let otm_distance = if first_side == "UP" {
        (ptb - spot).max(0.0)
    } else {
        (spot - ptb).max(0.0)
    };
    let expected_move = current_atr.max(1.0) * ((secs_left as f64).max(1.0) / 60.0).sqrt();
    (otm_distance / expected_move).clamp(0.0, 5.0)
}

fn target_hedge_ratio(
    phase: D1Phase,
    atr_regime: D1AtrRegime,
    pair_cost: f64,
    first_win_probability: f64,
    first_otm_z: f64,
) -> f64 {
    let target: f64 = match phase {
        D1Phase::Opening => {
            if pair_cost <= 0.90 {
                0.25
            } else if pair_cost <= 0.95 && (first_otm_z >= 0.20 || first_win_probability < 0.70) {
                0.35
            } else if pair_cost <= 0.98 && (first_otm_z >= 0.35 || first_win_probability < 0.45) {
                0.20
            } else if pair_cost <= 1.00 && first_win_probability < 0.40 && first_otm_z >= 0.60 {
                0.25
            } else {
                0.0
            }
        }
        D1Phase::Mid => {
            if first_win_probability < 0.32 && first_otm_z >= 0.75 {
                0.70
            } else if pair_cost <= 0.90 {
                0.35
            } else if pair_cost <= 0.95 && (first_otm_z >= 0.20 || first_win_probability < 0.70) {
                0.60
            } else if pair_cost <= 0.98 && (first_otm_z >= 0.35 || first_win_probability < 0.45) {
                0.40
            } else if pair_cost <= 1.00 && first_win_probability < 0.42 && first_otm_z >= 0.60 {
                0.35
            } else {
                0.0
            }
        }
        D1Phase::Late => {
            if first_win_probability < 0.25 && first_otm_z >= 1.00 {
                1.00
            } else if first_win_probability < 0.35 && first_otm_z >= 0.65 {
                0.80
            } else if pair_cost <= 0.92 {
                0.35
            } else if pair_cost <= 0.98 && (first_otm_z >= 0.20 || first_win_probability < 0.70) {
                0.75
            } else {
                0.0
            }
        }
        D1Phase::Final => {
            if first_win_probability < 0.25 && first_otm_z >= 0.90 {
                1.00
            } else if first_win_probability < 0.38 && first_otm_z >= 0.55 {
                0.75
            } else if pair_cost <= 0.92 {
                0.35
            } else if pair_cost <= 0.98 && (first_otm_z >= 0.20 || first_win_probability < 0.65) {
                0.50
            } else {
                0.0
            }
        }
    };

    match atr_regime {
        D1AtrRegime::Storm if matches!(phase, D1Phase::Opening) => target.min(0.25_f64),
        D1AtrRegime::Volatile if matches!(phase, D1Phase::Opening) => target.min(0.30_f64),
        _ => target,
    }
}

fn normal_cdf(x: f64) -> f64 {
    let sign = if x < 0.0 { -1.0 } else { 1.0 };
    let x = x.abs() / std::f64::consts::SQRT_2;
    let t = 1.0 / (1.0 + 0.3275911 * x);
    let a1 = 0.254829592;
    let a2 = -0.284496736;
    let a3 = 1.421413741;
    let a4 = -1.453152027;
    let a5 = 1.061405429;
    let erf = sign * (1.0 - (((((a5 * t + a4) * t) + a3) * t + a2) * t + a1) * t * (-x * x).exp());
    ((1.0 + erf) * 0.5).clamp(0.0, 1.0)
}

fn spot_velocity(spot_signal: SpotSignalSnapshot) -> Option<f64> {
    spot_signal
        .smoothed_velocity_usd_per_sec
        .or(spot_signal.raw_velocity_usd_per_sec)
}

fn expected_move_usd(current_atr: f64, secs_left: i64) -> f64 {
    current_atr.max(1.0) * ((secs_left as f64).max(1.0) / 60.0).sqrt()
}

fn side_ask(side: &str, prices: &PricesState) -> f64 {
    if side == "UP" {
        prices.up.ask
    } else {
        prices.down.ask
    }
}

fn side_fair_probability(side: &str, fair_up: f64) -> f64 {
    if side == "UP" {
        fair_up
    } else {
        1.0 - fair_up
    }
}

fn side_entry_confirmations(side: &str, spot_signal: SpotSignalSnapshot) -> usize {
    let side_sign = if side == "UP" { 1.0 } else { -1.0 };
    let mut count = 0;
    if spot_signal
        .raw_velocity_usd_per_sec
        .map(|velocity| velocity * side_sign >= D1_LIVE_MIN_VELOCITY_CONFIRM_USD_PER_SEC)
        .unwrap_or(false)
    {
        count += 1;
    }
    if spot_signal
        .smoothed_velocity_usd_per_sec
        .map(|velocity| velocity * side_sign >= D1_LIVE_MIN_VELOCITY_CONFIRM_USD_PER_SEC)
        .unwrap_or(false)
    {
        count += 1;
    }
    if spot_signal
        .acceleration_usd_per_sec2
        .map(|accel| accel * side_sign >= D1_LIVE_MIN_ACCEL_CONFIRM_USD_PER_SEC2)
        .unwrap_or(false)
    {
        count += 1;
    }
    count
}

fn has_counter_velocity(side: &str, spot_signal: SpotSignalSnapshot) -> bool {
    let side_sign = if side == "UP" { 1.0 } else { -1.0 };
    spot_signal
        .smoothed_velocity_usd_per_sec
        .or(spot_signal.raw_velocity_usd_per_sec)
        .map(|velocity| velocity * side_sign < -D1_LIVE_MIN_VELOCITY_CONFIRM_USD_PER_SEC)
        .unwrap_or(false)
}

fn fair_probability_up(
    market: &MarketWindow,
    spot_price: Option<f64>,
    current_atr: f64,
    secs_left: i64,
    spot_signal: SpotSignalSnapshot,
) -> f64 {
    let (Some(spot), Some(ptb)) = (spot_price, market.price_to_beat) else {
        return 0.50;
    };
    let secs_left = (secs_left as f64).max(1.0);
    let expected_move = current_atr.max(1.0) * (secs_left / 60.0).sqrt();
    let velocity_drift =
        spot_velocity(spot_signal).unwrap_or(0.0) * secs_left * D1_VELOCITY_ENTRY_DRIFT_FRACTION;
    normal_cdf(((spot - ptb) + velocity_drift) / expected_move)
}

fn duration_elapsed_and_time_pct(market: &MarketWindow, secs_to_end: i64) -> (f64, f64, f64) {
    let duration_sec = market_duration_sec(market);
    let elapsed_sec = (duration_sec - secs_to_end as f64).clamp(0.0, duration_sec);
    (
        duration_sec,
        elapsed_sec,
        (elapsed_sec / duration_sec) * 100.0,
    )
}

fn sleep_mode_seconds(duration_sec: f64) -> f64 {
    (duration_sec * (D1_SLEEP_TIME_PCT / 100.0)).max(D1_SLEEP_MIN_SECONDS)
}

fn market_duration_sec(market: &MarketWindow) -> f64 {
    match (
        chrono::DateTime::parse_from_rfc3339(&market.start_time),
        chrono::DateTime::parse_from_rfc3339(&market.end_time),
    ) {
        (Ok(s), Ok(e)) => ((e.timestamp_millis() - s.timestamp_millis()) as f64 / 1000.0).max(1.0),
        _ => 900.0,
    }
}

fn infer_first_leg(win_state: &WindowState) -> Option<(String, f64, f64)> {
    let first_buy = win_state
        .trades
        .iter()
        .find(|trade| trade.trade_type == "BUY")?;
    Some((first_buy.side.clone(), first_buy.price, first_buy.shares))
}

fn side_prices<'a>(side: &str, prices: &'a PricesState) -> (f64, f64, &'static str) {
    if side == "UP" {
        (prices.up.bid, prices.up.ask, "DOWN")
    } else {
        (prices.down.bid, prices.down.ask, "UP")
    }
}

fn ask_for_side(side: &str, prices: &PricesState) -> f64 {
    if side == "UP" {
        prices.up.ask
    } else {
        prices.down.ask
    }
}

fn shares_for_side(side: &str, win_state: &WindowState) -> f64 {
    if side == "UP" {
        win_state.up_shares
    } else {
        win_state.down_shares
    }
}

fn has_matching_stage1_trade(win_state: &WindowState, side: &str, shares: f64, price: f64) -> bool {
    win_state.trades.iter().rev().any(|trade| {
        trade.trade_type == "BUY"
            && trade.side == side
            && trade.reason.starts_with("d1_repair_pair_stage1_")
            && (trade.shares - shares).abs() <= 0.0001
            && (trade.price - price).abs() <= 0.0001
    })
}

fn live_conviction_entry_signal(
    config: &Config,
    prices: &PricesState,
    market: &MarketWindow,
    win_state: &WindowState,
    spot_price: Option<f64>,
    secs_to_end: i64,
    current_atr: f64,
    spot_signal: SpotSignalSnapshot,
    phase: D1Phase,
    atr_regime: D1AtrRegime,
    time_pct: f64,
) -> Option<OrderSignal> {
    if win_state.spent > 0.0 || win_state.status != "SKIPPED" {
        return None;
    }
    if time_pct > D1_LIVE_MAX_TIME_PCT {
        return None;
    }
    let (Some(spot), Some(ptb)) = (spot_price, market.price_to_beat) else {
        return None;
    };
    let expected_move = expected_move_usd(current_atr, secs_to_end);
    if expected_move < D1_LIVE_MIN_EXPECTED_MOVE_USD {
        return None;
    }
    let gap_z = (spot - ptb) / expected_move;
    if !gap_z.is_finite() || gap_z.abs() < D1_LIVE_MIN_ABS_Z {
        return None;
    }

    let entry_side = if gap_z >= 0.0 { "UP" } else { "DOWN" };
    let ask = side_ask(entry_side, prices);
    if ask <= 0.0 || ask > D1_LIVE_MAX_ENTRY_ASK {
        return None;
    }
    let confirmations = side_entry_confirmations(entry_side, spot_signal);
    if ask > D1_LIVE_RICH_ENTRY_ASK && (gap_z.abs() < D1_LIVE_RICH_MIN_ABS_Z || confirmations == 0)
    {
        return None;
    }
    if has_counter_velocity(entry_side, spot_signal) {
        return None;
    }

    let fair_up = fair_probability_up(market, spot_price, current_atr, secs_to_end, spot_signal);
    let fair_side = side_fair_probability(entry_side, fair_up);
    let entry_edge = fair_side - ask;
    if entry_edge < D1_LIVE_MIN_ENTRY_EDGE {
        return None;
    }

    let budget = (config.session.min_window_budget * D1_LIVE_CONVICTION_BUDGET_MULTIPLIER)
        .clamp(D1_MIN_TRADE_USD, config.session.max_window_budget);
    if budget < D1_MIN_TRADE_USD {
        return None;
    }

    Some(OrderSignal {
        side: entry_side.to_string(),
        is_buy: true,
        amount: budget,
        price: ask,
        reason: format!(
            "d1_live_conviction_entry_{}_phase_{}_atr_{}_ask_{:.2}_fair_{:.3}_edge_{:+.3}_gap_z_{:+.2}_dist_usd_{:+.2}_expected_move_{:.2}_conf_{}_budget_mult_{:.2}",
            entry_side.to_lowercase(),
            phase.as_str(),
            atr_regime.as_str(),
            ask,
            fair_side,
            entry_edge,
            gap_z,
            spot - ptb,
            expected_move,
            confirmations,
            D1_LIVE_CONVICTION_BUDGET_MULTIPLIER
        ),
    })
}

impl TradeStrategy for DynamicGridD1Strategy {
    fn check_pre_start_entry(
        &mut self,
        config: &Config,
        prices: &PricesState,
        market: &MarketWindow,
        spot_price: Option<f64>,
        window_number: usize,
        secs_to_start: i64,
        current_btc_atr: f64,
        spot_signal: SpotSignalSnapshot,
    ) -> Option<EntrySignal> {
        if !config.pre_start_entry.enabled || self.entered_windows.contains(&window_number) {
            return None;
        }

        if secs_to_start < config.pre_start_entry.min_seconds_before_start
            || secs_to_start > config.pre_start_entry.max_seconds_before_start
        {
            return None;
        }

        if current_btc_atr < config.min_btc_atr || current_btc_atr < D1_MIN_VALID_ATR {
            return None;
        }

        let up_ask = prices.up.ask;
        let down_ask = prices.down.ask;
        if up_ask <= 0.0 || down_ask <= 0.0 {
            return None;
        }

        let combined_ask = up_ask + down_ask;
        if combined_ask > D1_MAX_PRESTART_COMBINED_ASK {
            return None;
        }

        let fair_up = fair_probability_up(
            market,
            spot_price,
            current_btc_atr,
            secs_to_start + market_duration_sec(market) as i64,
            spot_signal,
        );
        let fair_down = 1.0 - fair_up;
        let up_edge = fair_up - up_ask;
        let down_edge = fair_down - down_ask;
        let (entry_side, first_ask, entry_edge, fair_probability) = if up_edge >= down_edge {
            ("UP", up_ask, up_edge, fair_up)
        } else {
            ("DOWN", down_ask, down_edge, fair_down)
        };

        if !(D1_FIRST_LEG_MIN_ASK..=D1_FIRST_LEG_MAX_ASK).contains(&first_ask) {
            return None;
        }

        let edge_is_too_weak = entry_edge < D1_MIN_ENTRY_EDGE;
        let selected_side_is_expensive = (entry_side == "UP" && up_ask > down_ask + 0.01)
            || (entry_side == "DOWN" && down_ask > up_ask + 0.01);
        if edge_is_too_weak && selected_side_is_expensive {
            return None;
        }

        let price_discount = if entry_side == "UP" {
            down_ask - up_ask
        } else {
            up_ask - down_ask
        };
        let atr_regime = D1AtrRegime::from_atr(current_btc_atr);
        let scout_discount_requirement = scout_price_discount_requirement(atr_regime);
        let directional_entry = fair_probability >= D1_DIRECTIONAL_FAIR_MIN;
        let scout_entry = !directional_entry
            && fair_probability <= D1_SCOUT_MAX_FAIR
            && price_discount >= scout_discount_requirement
            && first_ask <= 0.49;
        if !directional_entry && !scout_entry {
            return None;
        }
        let budget_multiplier = if directional_entry {
            directional_budget_multiplier(atr_regime)
        } else {
            scout_budget_multiplier(atr_regime)
        };

        self.entered_windows.insert(window_number);
        self.first_leg_side
            .insert(window_number, entry_side.to_string());
        self.first_leg_price.insert(window_number, first_ask);

        Some(EntrySignal {
            up_ask,
            down_ask,
            budget_multiplier,
            cheaper_side_ratio: 0.50,
            mode: EntryMode::OneSide(entry_side.to_string()),
            reason: format!(
                "d1_one_leg_{}_ask_{:.2}_fair_{:.3}_edge_{:+.3}_discount_{:.3}_discount_req_{:.3}_combined_{:.2}_atr_{:.2}_atr_regime_{}_budget_mult_{:.2}{}",
                entry_side.to_lowercase(),
                first_ask,
                fair_probability,
                entry_edge,
                price_discount,
                scout_discount_requirement,
                combined_ask,
                current_btc_atr,
                atr_regime.as_str(),
                budget_multiplier,
                if directional_entry { "_directional" } else { "_scout" }
            ),
        })
    }

    fn process_live_tick(
        &mut self,
        _config: &Config,
        prices: &PricesState,
        spot_price: Option<f64>,
        market: &MarketWindow,
        win_state: &WindowState,
        secs_to_end: i64,
        current_atr: f64,
        spot_signal: SpotSignalSnapshot,
    ) -> Vec<OrderSignal> {
        let mut signals = Vec::new();
        let window_number = win_state.window_number;
        let (duration_sec, elapsed_sec, time_pct) =
            duration_elapsed_and_time_pct(market, secs_to_end);
        let sleep_active = elapsed_sec < sleep_mode_seconds(duration_sec);
        let phase = D1Phase::from_time_pct(time_pct);
        let atr_regime = D1AtrRegime::from_atr(current_atr);
        let policy = phase_policy(phase, atr_regime);

        let (inferred_side, inferred_price, inferred_shares) = match infer_first_leg(win_state) {
            Some(first_leg) => first_leg,
            None => {
                if sleep_active {
                    return signals;
                }
                if let Some(signal) = live_conviction_entry_signal(
                    _config,
                    prices,
                    market,
                    win_state,
                    spot_price,
                    secs_to_end,
                    current_atr,
                    spot_signal,
                    phase,
                    atr_regime,
                    time_pct,
                ) {
                    signals.push(signal);
                }
                return signals;
            }
        };
        let first_side = self
            .first_leg_side
            .entry(window_number)
            .or_insert(inferred_side)
            .clone();
        let first_price = *self
            .first_leg_price
            .entry(window_number)
            .or_insert(inferred_price);
        let fair_up =
            fair_probability_up(market, spot_price, current_atr, secs_to_end, spot_signal);

        let state = self.states.entry(window_number).or_insert(StrategyState {
            up_sold: false,
            down_sold: false,
            first_sold_side: None,
            ptb_crossed: false,
            ptb_baseline: None,
        });

        if sleep_active {
            return signals;
        }

        if let Some(pending_side) = self.pending_repair_side.get(&window_number).cloned() {
            let pending_shares = *self
                .pending_repair_shares
                .get(&window_number)
                .unwrap_or(&0.0);
            let pending_price = *self
                .pending_repair_price
                .get(&window_number)
                .unwrap_or(&1.0);
            let stage1_side = if pending_side == "UP" { "DOWN" } else { "UP" };
            if !has_matching_stage1_trade(win_state, stage1_side, pending_shares, pending_price) {
                self.pending_repair_side.remove(&window_number);
                self.pending_repair_shares.remove(&window_number);
                self.pending_repair_price.remove(&window_number);
                return signals;
            }

            let current_ask = ask_for_side(&pending_side, prices);
            let staged_pair_cost = pending_price + current_ask;
            if pending_shares > 0.000001
                && current_ask > 0.0
                && staged_pair_cost <= policy.repair_pair_max_cost + 0.015
            {
                let buy_usd = pending_shares * current_ask;
                if buy_usd < D1_MIN_TRADE_USD {
                    self.pending_repair_side.remove(&window_number);
                    self.pending_repair_shares.remove(&window_number);
                    self.pending_repair_price.remove(&window_number);
                    return signals;
                }
                self.pending_repair_side.remove(&window_number);
                self.pending_repair_shares.remove(&window_number);
                self.pending_repair_price.remove(&window_number);
                signals.push(OrderSignal {
                    side: pending_side.clone(),
                    is_buy: true,
                    amount: buy_usd,
                    price: current_ask,
                    reason: format!(
                        "d1_repair_pair_stage2_{}_phase_{}_atr_{}_first_leg_{:.2}_second_{:.2}_pair_cost_{:.2}_shares_{:.4}",
                        pending_side.to_lowercase(),
                        phase.as_str(),
                        atr_regime.as_str(),
                        pending_price,
                        current_ask,
                        staged_pair_cost,
                        pending_shares
                    ),
                });
                return signals;
            }
            if time_pct >= 92.0 || staged_pair_cost > 1.02 {
                self.pending_repair_side.remove(&window_number);
                self.pending_repair_shares.remove(&window_number);
                self.pending_repair_price.remove(&window_number);
            }
        }

        let (first_bid, _first_ask, opposite_side) = side_prices(&first_side, prices);
        let opposite_ask = ask_for_side(opposite_side, prices);
        let first_shares = shares_for_side(&first_side, win_state);
        let opposite_shares = shares_for_side(opposite_side, win_state);
        let naked_first_shares = (first_shares - opposite_shares).max(0.0);
        let first_win_probability = if first_side == "UP" {
            fair_up
        } else {
            1.0 - fair_up
        };
        let first_otm_z =
            first_side_otm_z(&first_side, market, spot_price, current_atr, secs_to_end);

        if first_shares >= D1_MIN_LOCK_PAIR_SHARES && opposite_ask > 0.0 {
            let pair_cost_from_first = first_price + opposite_ask;
            if pair_cost_from_first <= policy.lock_pair_max_cost.min(D1_LOCK_PAIR_MAX_COST + 0.01) {
                let current_hedge_ratio = (opposite_shares / first_shares).clamp(0.0, 1.0);
                let target_hedge_ratio = target_hedge_ratio(
                    phase,
                    atr_regime,
                    pair_cost_from_first,
                    first_win_probability,
                    first_otm_z,
                );
                let hedge_gap = (target_hedge_ratio - current_hedge_ratio).max(0.0);
                if hedge_gap > 0.0 {
                    let lock_shares = first_shares * hedge_gap;
                    let buy_usd = lock_shares * opposite_ask;
                    if lock_shares >= D1_MIN_LOCK_PAIR_SHARES && buy_usd >= D1_MIN_TRADE_USD {
                        signals.push(OrderSignal {
                            side: opposite_side.to_string(),
                            is_buy: true,
                            amount: buy_usd,
                            price: opposite_ask,
                            reason: format!(
                                "d1_target_hedge_buy_{}_phase_{}_atr_{}_first_p_{:.3}_first_otm_z_{:.2}_hedge_now_{:.2}_hedge_target_{:.2}_hedge_gap_{:.2}_first_{:.2}_opp_{:.2}_pair_cost_{:.2}_shares_{:.4}",
                                opposite_side.to_lowercase(),
                                phase.as_str(),
                                atr_regime.as_str(),
                                first_win_probability,
                                first_otm_z,
                                current_hedge_ratio,
                                target_hedge_ratio,
                                hedge_gap,
                                first_price,
                                opposite_ask,
                                pair_cost_from_first,
                                lock_shares
                            ),
                        });
                        return signals;
                    }
                }
            }

            let insurance_cost = (pair_cost_from_first - 1.0).max(0.0);
            let insurance_quality = insurance_quality(insurance_cost);
            let risk_pressure = insurance_risk_pressure(
                first_bid,
                first_win_probability,
                time_pct,
                atr_regime,
                policy,
                first_otm_z,
            );
            let hedge_fraction = (insurance_quality * risk_pressure).clamp(0.0, 1.0);
            if hedge_fraction >= D1_MIN_INSURANCE_HEDGE_FRACTION
                && opposite_ask <= policy.runaway_opposite_max_ask
            {
                let hedge_shares = naked_first_shares * hedge_fraction;
                let buy_usd = hedge_shares * opposite_ask;
                if buy_usd >= D1_MIN_TRADE_USD {
                    let locked_loss = ((first_price + opposite_ask) - 1.0).max(0.0) * hedge_shares;
                    signals.push(OrderSignal {
                        side: opposite_side.to_string(),
                        is_buy: true,
                        amount: buy_usd,
                        price: opposite_ask,
                        reason: format!(
                            "d1_insurance_grid_buy_{}_phase_{}_atr_{}_first_bid_{:.2}_first_p_{:.3}_first_otm_z_{:.2}_risk_{:.2}_ins_cost_{:.3}_quality_{:.2}_hedge_{:.2}_soft_{:.2}_hard_{:.2}_first_{:.2}_opp_{:.2}_locked_loss_{:.2}_shares_{:.4}_time_{:.1}",
                            opposite_side.to_lowercase(),
                            phase.as_str(),
                            atr_regime.as_str(),
                            first_bid,
                            first_win_probability,
                            first_otm_z,
                            risk_pressure,
                            insurance_cost,
                            insurance_quality,
                            hedge_fraction,
                            policy.runaway_bid_soft,
                            policy.runaway_bid_hard,
                            first_price,
                            opposite_ask,
                            locked_loss,
                            hedge_shares,
                            time_pct
                        ),
                    });
                    return signals;
                }
            }
        }

        let paired_shares = win_state.up_shares.min(win_state.down_shares);
        let terminal_gap = (win_state.spent - win_state.cash_returned - paired_shares).max(0.0);
        let combined_ask = prices.up.ask + prices.down.ask;
        let repair_edge = 1.0 - combined_ask;
        let repair_round = *self.repair_rounds.get(&window_number).unwrap_or(&0);

        if paired_shares > 0.0
            && terminal_gap > 0.25
            && combined_ask > 0.0
            && combined_ask <= policy.repair_pair_max_cost
            && repair_edge > 0.0
            && repair_round < D1_REPAIR_MAX_ROUNDS
        {
            let needed_pair_shares = terminal_gap / repair_edge;
            let max_pair_shares_by_first = inferred_shares * D1_REPAIR_MAX_PAIR_FRACTION;
            let max_pair_shares_by_usd =
                (win_state.spent * D1_REPAIR_MAX_USD_FRACTION_OF_SPENT) / combined_ask;
            let repair_pair_shares = needed_pair_shares
                .min(max_pair_shares_by_first)
                .min(max_pair_shares_by_usd);

            if repair_pair_shares >= D1_MIN_REPAIR_PAIR_SHARES {
                let (first_repair_side, first_repair_ask, second_repair_side) =
                    if prices.up.ask <= prices.down.ask {
                        ("UP", prices.up.ask, "DOWN")
                    } else {
                        ("DOWN", prices.down.ask, "UP")
                    };
                let buy_usd = repair_pair_shares * first_repair_ask;
                if buy_usd < D1_MIN_TRADE_USD {
                    return signals;
                }
                self.repair_rounds.insert(window_number, repair_round + 1);
                self.pending_repair_side
                    .insert(window_number, second_repair_side.to_string());
                self.pending_repair_shares
                    .insert(window_number, repair_pair_shares);
                self.pending_repair_price
                    .insert(window_number, first_repair_ask);
                signals.push(OrderSignal {
                    side: first_repair_side.to_string(),
                    is_buy: true,
                    amount: buy_usd,
                    price: first_repair_ask,
                    reason: format!(
                        "d1_repair_pair_stage1_{}_round_{}_phase_{}_atr_{}_combined_{:.2}_edge_{:.2}_gap_{:.2}_pair_shares_{:.4}_pending_{}",
                        first_repair_side.to_lowercase(),
                        repair_round + 1,
                        phase.as_str(),
                        atr_regime.as_str(),
                        combined_ask,
                        repair_edge,
                        terminal_gap,
                        repair_pair_shares,
                        second_repair_side.to_lowercase()
                    ),
                });
                return signals;
            }
        }

        if win_state.up_shares <= 0.000001 {
            state.up_sold = true;
        }
        if win_state.down_shares <= 0.000001 {
            state.down_sold = true;
        }

        signals
    }

    fn get_strategy_state(&self, window_number: usize) -> Option<StrategyState> {
        self.states.get(&window_number).cloned()
    }
}
