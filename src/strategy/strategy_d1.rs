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
const D1_MIN_ENTRY_EDGE: f64 = 0.005;
const D1_DIRECTIONAL_FAIR_MIN: f64 = 0.54;
const D1_SCOUT_MIN_PRICE_DISCOUNT: f64 = 0.015;
const D1_SCOUT_VOLATILE_MIN_PRICE_DISCOUNT: f64 = 0.025;
const D1_SCOUT_STORM_MIN_PRICE_DISCOUNT: f64 = 0.035;
const D1_VELOCITY_ENTRY_DRIFT_FRACTION: f64 = 0.25;
const D1_LIVE_CONVICTION_BUDGET_MULTIPLIER: f64 = 0.35;
const D1_LIVE_MIN_ABS_Z: f64 = 0.55;
const D1_LIVE_RICH_MIN_ABS_Z: f64 = 0.72;
const D1_LIVE_MIN_ENTRY_EDGE: f64 = 0.03;
const D1_LIVE_MIN_EXPECTED_MOVE_PCT: f64 = 0.030;
const D1_LIVE_MAX_ENTRY_ASK: f64 = 0.65;
const D1_LIVE_RICH_ENTRY_ASK: f64 = 0.57;
const D1_LIVE_MAX_TIME_PCT: f64 = 70.0;
const D1_LIVE_MIN_VELOCITY_CONFIRM_USD_PER_SEC: f64 = 0.10;
const D1_LIVE_MIN_ACCEL_CONFIRM_USD_PER_SEC2: f64 = 0.01;
const D1_MIN_VALID_ATR: f64 = 1.0;
const D1_MIN_LOCK_PAIR_SHARES: f64 = 0.25;
const D1_MIN_TRADE_USD: f64 = 1.0;
const D1_PRESTART_SIGNAL_MIN_CONFIRMATIONS: usize = 2;
const D1_PRESTART_SIGNAL_MAX_NEG_EDGE: f64 = -0.020;
const D1_SLEEP_TIME_PCT: f64 = 5.0;
const D1_SLEEP_MIN_SECONDS: f64 = 25.0;
const D1_HEDGE_MIN_WRONG_PCT: f64 = 0.010;
const D1_HEDGE_BAD_WRONG_PCT: f64 = 0.050;
const D1_HEDGE_SEVERE_WRONG_PCT: f64 = 0.120;
const D1_CROSS_HEDGE_MAX_PAIR_COST: f64 = 1.04;
const D1_CROSS_HEDGE_TARGET_OPENING: f64 = 0.20;
const D1_CROSS_HEDGE_TARGET_LATER: f64 = 0.35;
const D1_STRONG_SELL_CLOSE_HOLD_SECONDS: i64 = 45;
const D1_WEAK_SALVAGE_TIME_PCT: f64 = 85.0;
const D1_WEAK_SALVAGE_MIN_BID: f64 = 0.05;
const D1_WEAK_SALVAGE_MAX_PROBABILITY: f64 = 0.35;
const D1_SELL_MIN_SHARES: f64 = 0.000001;

pub struct DynamicGridD1Strategy {
    pub entered_windows: HashSet<usize>,
    pub states: HashMap<usize, StrategyState>,
    pub first_leg_side: HashMap<usize, String>,
    pub first_leg_price: HashMap<usize, f64>,
    pub sell_steps: HashMap<(usize, String), usize>,
}

impl DynamicGridD1Strategy {
    pub fn new() -> Self {
        Self {
            entered_windows: HashSet::new(),
            states: HashMap::new(),
            first_leg_side: HashMap::new(),
            first_leg_price: HashMap::new(),
            sell_steps: HashMap::new(),
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
struct D1FirstSideDistance {
    otm_pct: f64,
    otm_z: f64,
}

#[derive(Debug, Clone, Copy)]
struct D1OppositeHedgePlan {
    target_ratio: f64,
    max_pair_cost: f64,
    label: &'static str,
}

#[derive(Debug, Clone, Copy)]
struct D1SellPlan {
    next_step: usize,
    fraction: f64,
    label: &'static str,
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

fn ptb_relation(spot_price: Option<f64>, market: &MarketWindow) -> Option<&'static str> {
    let (Some(spot), Some(ptb)) = (spot_price, market.price_to_beat) else {
        return None;
    };
    if spot > ptb {
        Some("ABOVE")
    } else if spot < ptb {
        Some("BELOW")
    } else {
        None
    }
}

fn relation_is_favorable_for_side(side: &str, relation: &str) -> bool {
    (side == "UP" && relation == "ABOVE") || (side == "DOWN" && relation == "BELOW")
}

fn update_ptb_cross_state(
    state: &mut StrategyState,
    spot_price: Option<f64>,
    market: &MarketWindow,
) -> bool {
    let Some(relation) = ptb_relation(spot_price, market) else {
        return false;
    };
    match state.ptb_baseline.as_deref() {
        None => {
            state.ptb_baseline = Some(relation.to_string());
            false
        }
        Some(baseline) if baseline != relation => {
            state.ptb_crossed = true;
            true
        }
        _ => false,
    }
}

fn first_side_distance(
    first_side: &str,
    market: &MarketWindow,
    spot_price: Option<f64>,
    current_atr: f64,
    secs_left: i64,
) -> D1FirstSideDistance {
    let (Some(spot), Some(ptb)) = (spot_price, market.price_to_beat) else {
        return D1FirstSideDistance {
            otm_pct: 0.0,
            otm_z: 0.0,
        };
    };
    let otm_distance = if first_side == "UP" {
        (ptb - spot).max(0.0)
    } else {
        (spot - ptb).max(0.0)
    };
    let expected_move = current_atr.max(1.0) * ((secs_left as f64).max(1.0) / 60.0).sqrt();
    D1FirstSideDistance {
        otm_pct: if ptb.abs() > 0.0 {
            (otm_distance / ptb.abs()) * 100.0
        } else {
            0.0
        },
        otm_z: (otm_distance / expected_move).clamp(0.0, 5.0),
    }
}

fn side_itm_distance(
    side: &str,
    market: &MarketWindow,
    spot_price: Option<f64>,
    current_atr: f64,
    secs_left: i64,
) -> D1FirstSideDistance {
    let (Some(spot), Some(ptb)) = (spot_price, market.price_to_beat) else {
        return D1FirstSideDistance {
            otm_pct: 0.0,
            otm_z: 0.0,
        };
    };
    let itm_distance = if side == "UP" {
        (spot - ptb).max(0.0)
    } else {
        (ptb - spot).max(0.0)
    };
    let expected_move = current_atr.max(1.0) * ((secs_left as f64).max(1.0) / 60.0).sqrt();
    D1FirstSideDistance {
        otm_pct: if ptb.abs() > 0.0 {
            (itm_distance / ptb.abs()) * 100.0
        } else {
            0.0
        },
        otm_z: (itm_distance / expected_move).clamp(0.0, 5.0),
    }
}

fn hedge_z_thresholds(atr_regime: D1AtrRegime) -> (f64, f64, f64) {
    match atr_regime {
        D1AtrRegime::Calm => (0.20, 0.45, 0.75),
        D1AtrRegime::Normal => (0.25, 0.55, 0.90),
        D1AtrRegime::Volatile => (0.30, 0.65, 1.00),
        D1AtrRegime::Storm => (0.35, 0.75, 1.10),
    }
}

fn opposite_hedge_plan(
    phase: D1Phase,
    atr_regime: D1AtrRegime,
    pair_cost: f64,
    first_win_probability: f64,
    first_distance: D1FirstSideDistance,
) -> Option<D1OppositeHedgePlan> {
    let (meaningful_z, clear_z, severe_z) = hedge_z_thresholds(atr_regime);

    if pair_cost <= 0.90 {
        let target_ratio = match phase {
            D1Phase::Opening => 0.10,
            D1Phase::Mid => 0.20,
            D1Phase::Late | D1Phase::Final => 0.25,
        };
        return Some(D1OppositeHedgePlan {
            target_ratio,
            max_pair_cost: 0.90,
            label: "cheap_pair",
        });
    }

    let first_meaningfully_wrong = first_distance.otm_z >= meaningful_z
        && first_distance.otm_pct >= D1_HEDGE_MIN_WRONG_PCT
        && first_win_probability <= 0.56;
    let first_clearly_wrong = first_distance.otm_z >= clear_z
        && first_distance.otm_pct >= D1_HEDGE_BAD_WRONG_PCT
        && first_win_probability <= 0.48;
    let first_severely_wrong = first_distance.otm_z >= severe_z
        && first_distance.otm_pct >= D1_HEDGE_SEVERE_WRONG_PCT
        && first_win_probability <= 0.38;

    let (target_ratio, max_pair_cost, label) = if first_severely_wrong {
        match phase {
            D1Phase::Opening => (0.40, 1.02, "severe_wrong"),
            D1Phase::Mid => (0.60, 1.08, "severe_wrong"),
            D1Phase::Late => (0.85, 1.12, "severe_wrong"),
            D1Phase::Final => (1.00, 1.16, "severe_wrong"),
        }
    } else if first_clearly_wrong {
        match phase {
            D1Phase::Opening => (0.25, 0.98, "clear_wrong"),
            D1Phase::Mid => (0.40, 1.03, "clear_wrong"),
            D1Phase::Late => (0.60, 1.07, "clear_wrong"),
            D1Phase::Final => (0.80, 1.10, "clear_wrong"),
        }
    } else if first_meaningfully_wrong {
        match phase {
            D1Phase::Opening => (0.12, 0.95, "meaningful_wrong"),
            D1Phase::Mid => (0.25, 0.98, "meaningful_wrong"),
            D1Phase::Late => (0.35, 1.00, "meaningful_wrong"),
            D1Phase::Final => (0.45, 1.02, "meaningful_wrong"),
        }
    } else {
        return None;
    };

    if pair_cost > max_pair_cost {
        return None;
    }

    Some(D1OppositeHedgePlan {
        target_ratio,
        max_pair_cost,
        label,
    })
}

fn strong_sell_plan(
    phase: D1Phase,
    atr_regime: D1AtrRegime,
    bid: f64,
    side_win_probability: f64,
    itm_distance: D1FirstSideDistance,
    secs_to_end: i64,
    current_step: usize,
) -> Option<D1SellPlan> {
    if secs_to_end <= D1_STRONG_SELL_CLOSE_HOLD_SECONDS {
        return None;
    }

    let (meaningful_z, clear_z, severe_z) = hedge_z_thresholds(atr_regime);
    match current_step {
        0 if !matches!(phase, D1Phase::Opening)
            && bid >= 0.65
            && side_win_probability >= 0.58
            && itm_distance.otm_pct >= D1_HEDGE_MIN_WRONG_PCT
            && itm_distance.otm_z >= meaningful_z =>
        {
            Some(D1SellPlan {
                next_step: 1,
                fraction: 0.20,
                label: "step1_runner_skim",
            })
        }
        1 if matches!(phase, D1Phase::Late | D1Phase::Final)
            && bid >= 0.75
            && side_win_probability >= 0.65
            && itm_distance.otm_pct >= D1_HEDGE_BAD_WRONG_PCT
            && itm_distance.otm_z >= clear_z =>
        {
            Some(D1SellPlan {
                next_step: 2,
                fraction: 0.25,
                label: "step2_profit_lock",
            })
        }
        2 if matches!(phase, D1Phase::Late)
            && bid >= 0.85
            && side_win_probability >= 0.75
            && itm_distance.otm_pct >= D1_HEDGE_SEVERE_WRONG_PCT
            && itm_distance.otm_z >= severe_z =>
        {
            Some(D1SellPlan {
                next_step: 3,
                fraction: 0.20,
                label: "step3_deep_lock",
            })
        }
        _ => None,
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

fn side_bid(side: &str, prices: &PricesState) -> f64 {
    if side == "UP" {
        prices.up.bid
    } else {
        prices.down.bid
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

fn prestart_momentum_side(spot_signal: SpotSignalSnapshot) -> Option<(&'static str, usize)> {
    let up_confirmations = side_entry_confirmations("UP", spot_signal);
    let down_confirmations = side_entry_confirmations("DOWN", spot_signal);
    if up_confirmations >= D1_PRESTART_SIGNAL_MIN_CONFIRMATIONS
        && up_confirmations > down_confirmations
    {
        Some(("UP", up_confirmations))
    } else if down_confirmations >= D1_PRESTART_SIGNAL_MIN_CONFIRMATIONS
        && down_confirmations > up_confirmations
    {
        Some(("DOWN", down_confirmations))
    } else {
        None
    }
}

fn bootstrap_random_side(window_number: usize, market: &MarketWindow) -> &'static str {
    let mut acc = window_number as u64;
    for byte in market.slug.as_bytes() {
        acc = acc.wrapping_mul(131).wrapping_add(*byte as u64);
    }
    if acc % 2 == 0 {
        "UP"
    } else {
        "DOWN"
    }
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
    let expected_move_pct = if ptb.abs() > 0.0 {
        (expected_move / ptb.abs()) * 100.0
    } else {
        0.0
    };
    if expected_move_pct < D1_LIVE_MIN_EXPECTED_MOVE_PCT {
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
            "d1_live_conviction_entry_{}_phase_{}_atr_{}_ask_{:.2}_fair_{:.3}_edge_{:+.3}_gap_z_{:+.2}_dist_pct_{:+.4}_expected_move_pct_{:.4}_conf_{}_budget_mult_{:.2}",
            entry_side.to_lowercase(),
            phase.as_str(),
            atr_regime.as_str(),
            ask,
            fair_side,
            entry_edge,
            gap_z,
            if ptb.abs() > 0.0 {
                ((spot - ptb) / ptb.abs()) * 100.0
            } else {
                0.0
            },
            expected_move_pct,
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
        _llm_forecast: Option<crate::strategy::LlmForecast>,
        _cex_micro: &crate::strategy::CexMicroSnapshot,
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
        let ptb_known = market.price_to_beat.is_some() && spot_price.is_some();
        let atr_regime = D1AtrRegime::from_atr(current_btc_atr);
        let momentum_signal = prestart_momentum_side(spot_signal);
        let (fair_side, fair_ask, fair_edge, fair_probability) = if up_edge >= down_edge {
            ("UP", up_ask, up_edge, fair_up)
        } else {
            ("DOWN", down_ask, down_edge, fair_down)
        };

        let fair_directional = ptb_known
            && fair_probability >= D1_DIRECTIONAL_FAIR_MIN
            && fair_edge >= D1_MIN_ENTRY_EDGE
            && !has_counter_velocity(fair_side, spot_signal);
        let (entry_side, first_ask, entry_edge, fair_probability, entry_source, confirmations) =
            if fair_directional {
                (
                    fair_side.to_string(),
                    fair_ask,
                    fair_edge,
                    fair_probability,
                    "directional".to_string(),
                    0,
                )
            } else if let Some((signal_side, signal_confirmations)) = momentum_signal {
                let signal_ask = side_ask(signal_side, prices);
                let signal_fair = side_fair_probability(signal_side, fair_up);
                let signal_edge = signal_fair - signal_ask;
                if signal_edge < D1_PRESTART_SIGNAL_MAX_NEG_EDGE {
                    return None;
                }
                (
                    signal_side.to_string(),
                    signal_ask,
                    signal_edge,
                    signal_fair,
                    if ptb_known {
                        "momentum_confirmed".to_string()
                    } else {
                        "momentum_no_ptb".to_string()
                    },
                    signal_confirmations,
                )
            } else if window_number == 1 {
                let bootstrap_side = bootstrap_random_side(window_number, market);
                let bootstrap_ask = side_ask(bootstrap_side, prices);
                let bootstrap_fair = side_fair_probability(bootstrap_side, fair_up);
                let bootstrap_edge = bootstrap_fair - bootstrap_ask;
                (
                    bootstrap_side.to_string(),
                    bootstrap_ask,
                    bootstrap_edge,
                    bootstrap_fair,
                    "bootstrap_random_no_signal".to_string(),
                    0,
                )
            } else {
                return None;
            };

        if !(D1_FIRST_LEG_MIN_ASK..=D1_FIRST_LEG_MAX_ASK).contains(&first_ask) {
            return None;
        }

        let price_discount = if entry_side == "UP" {
            down_ask - up_ask
        } else {
            up_ask - down_ask
        };
        let scout_discount_requirement = scout_price_discount_requirement(atr_regime);
        let directional_entry = entry_source == "directional";
        let budget_multiplier = if directional_entry {
            directional_budget_multiplier(atr_regime)
        } else {
            scout_budget_multiplier(atr_regime)
        };

        self.entered_windows.insert(window_number);
        self.first_leg_side
            .insert(window_number, entry_side.clone());
        self.first_leg_price.insert(window_number, first_ask);

        Some(EntrySignal {
            up_ask,
            down_ask,
            budget_multiplier,
            cheaper_side_ratio: 0.50,
            mode: EntryMode::OneSide(entry_side.clone()),
            reason: format!(
                "d1_one_leg_{}_ask_{:.2}_fair_{:.3}_edge_{:+.3}_source_{}_conf_{}_ptb_known_{}_discount_{:.3}_discount_req_{:.3}_combined_{:.2}_atr_{:.2}_atr_regime_{}_budget_mult_{:.2}",
                entry_side.to_lowercase(),
                first_ask,
                fair_probability,
                entry_edge,
                entry_source,
                confirmations,
                ptb_known,
                price_discount,
                scout_discount_requirement,
                combined_ask,
                current_btc_atr,
                atr_regime.as_str(),
                budget_multiplier
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
        _mid_cross: &crate::strategy::MidCrossSnapshot,
        _cex_micro: &crate::strategy::CexMicroSnapshot,
    ) -> Vec<OrderSignal> {
        let mut signals = Vec::new();
        let window_number = win_state.window_number;
        let (duration_sec, elapsed_sec, time_pct) =
            duration_elapsed_and_time_pct(market, secs_to_end);
        let sleep_active = elapsed_sec < sleep_mode_seconds(duration_sec);
        let phase = D1Phase::from_time_pct(time_pct);
        let atr_regime = D1AtrRegime::from_atr(current_atr);

        let (inferred_side, inferred_price, _inferred_shares) = match infer_first_leg(win_state) {
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
            e_conviction_side: None,
            e_tranches_done: 0,
            e_grid_steps_done: 0,
            h_entry_side: None,
            h_entry_done: false,
            h_salvage_done: false,
        });
        let cross_happened_now = update_ptb_cross_state(state, spot_price, market);
        let ptb_crossed = state.ptb_crossed;
        let ptb_baseline = state
            .ptb_baseline
            .clone()
            .unwrap_or_else(|| "NA".to_string());

        if sleep_active {
            return signals;
        }

        let (_first_bid, _first_ask, opposite_side) = side_prices(&first_side, prices);
        let opposite_ask = ask_for_side(opposite_side, prices);
        let first_shares = shares_for_side(&first_side, win_state);
        let opposite_shares = shares_for_side(opposite_side, win_state);
        let first_win_probability = if first_side == "UP" {
            fair_up
        } else {
            1.0 - fair_up
        };
        let first_distance =
            first_side_distance(&first_side, market, spot_price, current_atr, secs_to_end);
        let first_relation = ptb_relation(spot_price, market);
        let first_is_adverse_after_cross = ptb_crossed
            && first_relation
                .map(|relation| !relation_is_favorable_for_side(&first_side, relation))
                .unwrap_or(false);

        if first_shares >= D1_MIN_LOCK_PAIR_SHARES && opposite_ask > 0.0 {
            let pair_cost_from_first = first_price + opposite_ask;
            let current_hedge_ratio = (opposite_shares / first_shares).clamp(0.0, 1.0);
            if first_is_adverse_after_cross && pair_cost_from_first <= D1_CROSS_HEDGE_MAX_PAIR_COST
            {
                let target_ratio = if matches!(phase, D1Phase::Opening) {
                    D1_CROSS_HEDGE_TARGET_OPENING
                } else {
                    D1_CROSS_HEDGE_TARGET_LATER
                };
                let hedge_gap = (target_ratio - current_hedge_ratio).max(0.0);
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
                                "d1_cross_hedge_buy_{}_phase_{}_atr_{}_cross_now_{}_baseline_{}_relation_{}_first_p_{:.3}_first_otm_pct_{:.4}_first_otm_z_{:.2}_hedge_now_{:.2}_hedge_target_{:.2}_hedge_gap_{:.2}_first_{:.2}_opp_{:.2}_pair_cost_{:.2}_max_pair_{:.2}_shares_{:.4}",
                                opposite_side.to_lowercase(),
                                phase.as_str(),
                                atr_regime.as_str(),
                                cross_happened_now,
                                ptb_baseline,
                                first_relation.unwrap_or("NA"),
                                first_win_probability,
                                first_distance.otm_pct,
                                first_distance.otm_z,
                                current_hedge_ratio,
                                target_ratio,
                                hedge_gap,
                                first_price,
                                opposite_ask,
                                pair_cost_from_first,
                                D1_CROSS_HEDGE_MAX_PAIR_COST,
                                lock_shares
                            ),
                        });
                        return signals;
                    }
                }
            }
            if let Some(plan) = opposite_hedge_plan(
                phase,
                atr_regime,
                pair_cost_from_first,
                first_win_probability,
                first_distance,
            ) {
                let hedge_gap = (plan.target_ratio - current_hedge_ratio).max(0.0);
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
                                "d1_opposite_hedge_buy_{}_plan_{}_phase_{}_atr_{}_first_p_{:.3}_first_otm_pct_{:.4}_first_otm_z_{:.2}_hedge_now_{:.2}_hedge_target_{:.2}_hedge_gap_{:.2}_first_{:.2}_opp_{:.2}_pair_cost_{:.2}_max_pair_{:.2}_shares_{:.4}",
                                opposite_side.to_lowercase(),
                                plan.label,
                                phase.as_str(),
                                atr_regime.as_str(),
                                first_win_probability,
                                first_distance.otm_pct,
                                first_distance.otm_z,
                                current_hedge_ratio,
                                plan.target_ratio,
                                hedge_gap,
                                first_price,
                                opposite_ask,
                                pair_cost_from_first,
                                plan.max_pair_cost,
                                lock_shares
                            ),
                        });
                        return signals;
                    }
                }
            }
        }

        let paired_core = win_state.up_shares.min(win_state.down_shares);
        let (meaningful_z, _, _) = hedge_z_thresholds(atr_regime);

        for side in ["UP", "DOWN"] {
            let side_shares = shares_for_side(side, win_state);
            let surplus_shares = (side_shares - paired_core).max(0.0);
            if surplus_shares <= D1_SELL_MIN_SHARES {
                continue;
            }

            let bid = side_bid(side, prices);
            let side_probability = side_fair_probability(side, fair_up);
            let otm_distance =
                first_side_distance(side, market, spot_price, current_atr, secs_to_end);
            let bid_overpays_probability = bid >= side_probability + 0.02;
            let crossed_first_tail = side == first_side && first_is_adverse_after_cross;

            if (time_pct >= D1_WEAK_SALVAGE_TIME_PCT
                || secs_to_end <= D1_STRONG_SELL_CLOSE_HOLD_SECONDS)
                && bid >= D1_WEAK_SALVAGE_MIN_BID
                && side_probability <= D1_WEAK_SALVAGE_MAX_PROBABILITY
                && otm_distance.otm_pct >= D1_HEDGE_MIN_WRONG_PCT
                && otm_distance.otm_z >= meaningful_z
                && bid_overpays_probability
            {
                signals.push(OrderSignal {
                    side: side.to_string(),
                    is_buy: false,
                    amount: surplus_shares,
                    price: bid,
                    reason: format!(
                        "d1_weak_salvage_sell_{}_phase_{}_atr_{}_crossed_first_{}_bid_{:.2}_p_{:.3}_otm_pct_{:.4}_otm_z_{:.2}_sell_{:.4}_keep_paired_{:.4}_time_{:.1}",
                        side.to_lowercase(),
                        phase.as_str(),
                        atr_regime.as_str(),
                        crossed_first_tail,
                        bid,
                        side_probability,
                        otm_distance.otm_pct,
                        otm_distance.otm_z,
                        surplus_shares,
                        paired_core,
                        time_pct
                    ),
                });
                return signals;
            }
        }

        for side in ["UP", "DOWN"] {
            let side_shares = shares_for_side(side, win_state);
            let surplus_shares = (side_shares - paired_core).max(0.0);
            if surplus_shares <= D1_SELL_MIN_SHARES {
                continue;
            }

            let bid = side_bid(side, prices);
            let side_probability = side_fair_probability(side, fair_up);
            let itm_distance =
                side_itm_distance(side, market, spot_price, current_atr, secs_to_end);
            let step_key = (window_number, side.to_string());
            let current_step = *self.sell_steps.get(&step_key).unwrap_or(&0);

            if let Some(plan) = strong_sell_plan(
                phase,
                atr_regime,
                bid,
                side_probability,
                itm_distance,
                secs_to_end,
                current_step,
            ) {
                let sell_amount = (side_shares * plan.fraction).min(surplus_shares);
                if sell_amount > D1_SELL_MIN_SHARES {
                    self.sell_steps.insert(step_key, plan.next_step);
                    signals.push(OrderSignal {
                        side: side.to_string(),
                        is_buy: false,
                        amount: sell_amount,
                        price: bid,
                        reason: format!(
                            "d1_strong_runner_sell_{}_{}_phase_{}_atr_{}_bid_{:.2}_p_{:.3}_itm_pct_{:.4}_itm_z_{:.2}_step_{}_fraction_{:.2}_sell_{:.4}_keep_paired_{:.4}_time_{:.1}",
                            side.to_lowercase(),
                            plan.label,
                            phase.as_str(),
                            atr_regime.as_str(),
                            bid,
                            side_probability,
                            itm_distance.otm_pct,
                            itm_distance.otm_z,
                            plan.next_step,
                            plan.fraction,
                            sell_amount,
                            paired_core,
                            time_pct
                        ),
                    });
                    return signals;
                }
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
