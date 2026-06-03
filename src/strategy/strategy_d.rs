use crate::client::{MarketWindow, PricesState};
use crate::config::Config;
use crate::strategy::{
    EntryMode, EntrySignal, OrderSignal, SpotSignalSnapshot, StrategyState, TradeStrategy,
};
use crate::trader::WindowState;
use std::collections::HashMap;

const ATR_ULTRA_LOW_MAX: f64 = 8.0;
const ATR_LOW_MAX: f64 = 18.0;
const ATR_HIGH_MIN: f64 = 45.0;
const ATR_EXTREME_MIN: f64 = 90.0;

const ENTRY_MAX_ASK_SPREAD: f64 = 0.16;
const ENTRY_ULTRA_LOW_ATR_MAX_COMBINED_ASK: f64 = 1.00;
const ENTRY_LOW_ATR_MAX_COMBINED_ASK: f64 = 1.01;
const ENTRY_HIGH_ATR_MAX_COMBINED_ASK: f64 = 1.04;
const ENTRY_EXTREME_ATR_MAX_COMBINED_ASK: f64 = 1.02;
const ENTRY_DIRECTIONAL_MAX_COMBINED_ASK: f64 = 1.02;

const WEAK_SCALP_MAX_TRANCHES_PER_SIDE: usize = 2;
const WEAK_SCALP_USD_FRACTION_OF_SPENT: f64 = 0.035;
const WEAK_SCALP_MIN_USD: f64 = 0.35;
const WEAK_SCALP_MIN_STRONG_REMAINING_FRACTION: f64 = 0.20;
const WEAK_SCALP_MIN_RECOVERED_RATIO: f64 = 0.40;
const REDEEM_HOLD_RELEASE_BID: f64 = 0.90;
const STRONG_GRID_STEP_SELL_FRACTION: f64 = 0.25;
const STRONG_GRID_RUNNER_FRACTION: f64 = 0.35;
const CAPITAL_PROTECTED_RATIO: f64 = 0.70;
const PAIRED_FLOOR_MAX_SACRIFICE_RATIO: f64 = 0.08;
const PAIRED_FLOOR_PROTECTED_MAX_SACRIFICE_RATIO: f64 = 0.12;
const WEAK_EXIT_MIN_INSURANCE_FRACTION: f64 = 0.08;
const WEAK_EXIT_NEAR_INSURANCE_FRACTION: f64 = 0.30;
const WEAK_EXIT_MODERATE_INSURANCE_FRACTION: f64 = 0.25;
const WEAK_EXIT_FAR_INSURANCE_FRACTION: f64 = 0.14;
const INSURANCE_TAIL_RELEASE_TIME_PCT: f64 = 90.0;
const WIN_PROB_SELL_EDGE: f64 = 0.01;
const SPOT_VELOCITY_PROB_DRIFT_FRACTION: f64 = 0.35;
const STRONG_STEP_1_MIN_TIME_PCT: f64 = 20.0;
const STRONG_STEP_2_MIN_TIME_PCT: f64 = 45.0;
const STRONG_STEP_3_MIN_TIME_PCT: f64 = 75.0;
const STRONG_STEP_1_EARLY_EDGE: f64 = 0.12;
const STRONG_STEP_2_EARLY_EDGE: f64 = 0.18;
const STRONG_STEP_3_EARLY_EDGE: f64 = 0.24;
const STRONG_STEP_1_MIN_EDGE: f64 = 0.02;
const STRONG_STEP_2_MIN_EDGE: f64 = 0.04;
const STRONG_STEP_3_MIN_EDGE: f64 = 0.06;
const WEAK_CORE_SELL_MIN_TIME_PCT: f64 = 60.0;
const WEAK_CORE_SELL_EDGE: f64 = 0.08;
const WEAK_CORE_PANIC_PROB: f64 = 0.10;

// ─── СТРАТЕГИЯ Д: Dynamic Grid + WeakScalp + Time-Decay Crossover Block ───
pub struct DynamicGridStrategy {
    pub entered_windows: std::collections::HashSet<usize>,
    pub states: HashMap<usize, StrategyState>,
    pub up_steps_hit: HashMap<usize, usize>, // Ступени сетки UP (0..3)
    pub dn_steps_hit: HashMap<usize, usize>, // Ступени сетки DOWN (0..3)
    pub scalp_tranches: HashMap<(usize, String), usize>,
    pub scalp_active_shares: HashMap<(usize, String), f64>,
    pub scalp_active_cost: HashMap<(usize, String), f64>,
    pub weak_exit_insurance_reserve: HashMap<(usize, String), f64>,
}

impl DynamicGridStrategy {
    pub fn new() -> Self {
        Self {
            entered_windows: std::collections::HashSet::new(),
            states: HashMap::new(),
            up_steps_hit: HashMap::new(),
            dn_steps_hit: HashMap::new(),
            scalp_tranches: HashMap::new(),
            scalp_active_shares: HashMap::new(),
            scalp_active_cost: HashMap::new(),
            weak_exit_insurance_reserve: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeviationZone {
    Unknown,
    Near,
    Moderate,
    Far,
    Runaway,
}

impl DeviationZone {
    fn as_str(self) -> &'static str {
        match self {
            DeviationZone::Unknown => "unknown",
            DeviationZone::Near => "near",
            DeviationZone::Moderate => "moderate",
            DeviationZone::Far => "far",
            DeviationZone::Runaway => "runaway",
        }
    }

    fn is_far_or_runaway(self) -> bool {
        matches!(self, DeviationZone::Far | DeviationZone::Runaway)
    }
}

#[derive(Debug, Clone, Copy)]
struct PtbDeviation {
    known: bool,
    signed_usd: f64,
    abs_usd: f64,
    pct_abs: f64,
    zone: DeviationZone,
}

#[derive(Debug, Clone, Copy)]
struct EntryRegime {
    budget_multiplier: f64,
    cheaper_side_ratio: f64,
    max_combined_ask: f64,
    reason: &'static str,
}

fn entry_regime(current_btc_atr: f64) -> EntryRegime {
    if current_btc_atr < ATR_ULTRA_LOW_MAX {
        EntryRegime {
            budget_multiplier: 0.25,
            cheaper_side_ratio: 0.50,
            max_combined_ask: ENTRY_ULTRA_LOW_ATR_MAX_COMBINED_ASK,
            reason: "d_v3_entry_ultra_low_atr_micro",
        }
    } else if current_btc_atr < ATR_LOW_MAX {
        EntryRegime {
            budget_multiplier: 0.55,
            cheaper_side_ratio: 0.52,
            max_combined_ask: ENTRY_LOW_ATR_MAX_COMBINED_ASK,
            reason: "d_v3_entry_low_atr_scout",
        }
    } else if current_btc_atr < ATR_HIGH_MIN {
        EntryRegime {
            budget_multiplier: 1.00,
            cheaper_side_ratio: 0.56,
            max_combined_ask: ENTRY_HIGH_ATR_MAX_COMBINED_ASK,
            reason: "d_v3_entry_normal_atr_balanced",
        }
    } else if current_btc_atr < ATR_EXTREME_MIN {
        EntryRegime {
            budget_multiplier: 0.85,
            cheaper_side_ratio: 0.50,
            max_combined_ask: ENTRY_HIGH_ATR_MAX_COMBINED_ASK,
            reason: "d_v3_entry_high_atr_delta_neutral",
        }
    } else {
        EntryRegime {
            budget_multiplier: 0.45,
            cheaper_side_ratio: 0.50,
            max_combined_ask: ENTRY_EXTREME_ATR_MAX_COMBINED_ASK,
            reason: "d_v3_entry_extreme_atr_micro_neutral",
        }
    }
}

fn live_leader_is_up(up_bid: f64, dn_bid: f64, initial_up: f64, initial_dn: f64) -> bool {
    if up_bid >= dn_bid + 0.02 {
        true
    } else if dn_bid >= up_bid + 0.02 {
        false
    } else {
        initial_up >= initial_dn
    }
}

fn ptb_deviation(market: &MarketWindow, spot_price: Option<f64>) -> PtbDeviation {
    let Some(spot) = spot_price else {
        return PtbDeviation {
            known: false,
            signed_usd: 0.0,
            abs_usd: 0.0,
            pct_abs: 0.25,
            zone: DeviationZone::Unknown,
        };
    };
    let Some(ptb) = market.price_to_beat else {
        return PtbDeviation {
            known: false,
            signed_usd: 0.0,
            abs_usd: 0.0,
            pct_abs: 0.25,
            zone: DeviationZone::Unknown,
        };
    };
    if ptb <= 0.0 {
        return PtbDeviation {
            known: false,
            signed_usd: 0.0,
            abs_usd: 0.0,
            pct_abs: 0.25,
            zone: DeviationZone::Unknown,
        };
    }

    let signed_usd = spot - ptb;
    let abs_usd = signed_usd.abs();
    let pct_abs = (abs_usd / ptb) * 100.0;

    let asset = market.asset.to_uppercase();
    let (near_usd, moderate_usd, far_usd) = match asset.as_str() {
        "BTC" => (25.0, 100.0, 250.0),
        "ETH" => (2.0, 8.0, 20.0),
        "SOL" => (0.20, 0.80, 2.00),
        _ => (25.0, 100.0, 250.0),
    };

    let zone = if abs_usd <= near_usd || pct_abs <= 0.03 {
        DeviationZone::Near
    } else if abs_usd <= moderate_usd || pct_abs <= 0.10 {
        DeviationZone::Moderate
    } else if abs_usd <= far_usd || pct_abs <= 0.25 {
        DeviationZone::Far
    } else {
        DeviationZone::Runaway
    };

    PtbDeviation {
        known: true,
        signed_usd,
        abs_usd,
        pct_abs,
        zone,
    }
}

fn strong_side_is_itm(is_up_strong: bool, dev: PtbDeviation) -> bool {
    dev.known && ((is_up_strong && dev.signed_usd > 0.0) || (!is_up_strong && dev.signed_usd < 0.0))
}

fn spot_velocity(spot_signal: SpotSignalSnapshot) -> Option<f64> {
    spot_signal
        .smoothed_velocity_usd_per_sec
        .or(spot_signal.raw_velocity_usd_per_sec)
}

fn redeem_hold_counter_velocity_limit(current_atr: f64) -> f64 {
    (current_atr / 12.0).clamp(1.0, 8.0)
}

fn velocity_bias_for_side(
    is_up_side: bool,
    spot_signal: SpotSignalSnapshot,
    current_atr: f64,
) -> i8 {
    let Some(velocity) = spot_velocity(spot_signal) else {
        return 0;
    };
    let limit = (current_atr / 20.0).clamp(0.50, 4.0);
    if is_up_side {
        if velocity > limit {
            1
        } else if velocity < -limit {
            -1
        } else {
            0
        }
    } else if velocity < -limit {
        1
    } else if velocity > limit {
        -1
    } else {
        0
    }
}

fn velocity_bias_label(bias: i8) -> &'static str {
    match bias {
        1 => "with",
        -1 => "against",
        _ => "neutral",
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

fn side_win_probability(
    is_up_side: bool,
    dev: PtbDeviation,
    current_atr: f64,
    secs_to_end: i64,
    spot_signal: SpotSignalSnapshot,
) -> f64 {
    if !dev.known {
        return 0.50;
    }

    let secs_left = (secs_to_end as f64).max(1.0);
    let expected_move = current_atr.max(1.0) * (secs_left / 60.0).sqrt();
    let velocity_drift =
        spot_velocity(spot_signal).unwrap_or(0.0) * secs_left * SPOT_VELOCITY_PROB_DRIFT_FRACTION;
    let effective_delta = dev.signed_usd + velocity_drift;
    let up_probability = normal_cdf(effective_delta / expected_move);

    if is_up_side {
        up_probability
    } else {
        1.0 - up_probability
    }
}

fn should_sell_by_probability(bid: f64, win_probability: f64) -> bool {
    bid >= (win_probability + WIN_PROB_SELL_EDGE).clamp(0.0, 1.0)
}

fn sell_edge(bid: f64, win_probability: f64) -> f64 {
    bid - win_probability
}

fn strong_grid_step_allowed(
    step_idx: usize,
    time_pct: f64,
    bid: f64,
    win_probability: f64,
) -> bool {
    let edge = sell_edge(bid, win_probability);
    let (min_time_pct, early_edge, min_edge) = match step_idx {
        0 => (
            STRONG_STEP_1_MIN_TIME_PCT,
            STRONG_STEP_1_EARLY_EDGE,
            STRONG_STEP_1_MIN_EDGE,
        ),
        1 => (
            STRONG_STEP_2_MIN_TIME_PCT,
            STRONG_STEP_2_EARLY_EDGE,
            STRONG_STEP_2_MIN_EDGE,
        ),
        _ => (
            STRONG_STEP_3_MIN_TIME_PCT,
            STRONG_STEP_3_EARLY_EDGE,
            STRONG_STEP_3_MIN_EDGE,
        ),
    };

    (time_pct >= min_time_pct && edge >= min_edge) || edge >= early_edge
}

fn adjust_strong_grid_target(base: f64, step_idx: usize, velocity_bias: i8, time_pct: f64) -> f64 {
    let adjustment = match (velocity_bias, step_idx) {
        (1, 0) if time_pct < 70.0 => 0.01,
        (1, 1) if time_pct < 80.0 => 0.03,
        (1, 2) if time_pct < 90.0 => 0.04,
        (-1, 0) => -0.02,
        (-1, 1) => -0.03,
        (-1, 2) => -0.05,
        _ => 0.0,
    };
    (base + adjustment).clamp(0.08, 0.94)
}

fn adjust_weak_exit_target(base: f64, velocity_bias: i8, time_pct: f64) -> f64 {
    let adjustment = match velocity_bias {
        1 if time_pct < 60.0 => 0.04,
        1 if time_pct < 80.0 => 0.02,
        -1 if time_pct < 60.0 => -0.03,
        -1 => -0.04,
        _ => 0.0,
    };
    (base + adjustment).clamp(0.06, 0.70)
}

fn capital_protected_weak_exit_target(base: f64, time_pct: f64) -> f64 {
    let cap = if time_pct < 30.0 {
        0.42
    } else if time_pct < 60.0 {
        0.28
    } else {
        0.18
    };
    base.min(cap).clamp(0.06, 0.70)
}

fn terminal_floor(cash_returned: f64, up_shares: f64, down_shares: f64) -> f64 {
    cash_returned + up_shares.min(down_shares)
}

fn max_paired_floor_sacrifice(spent: f64, cash_returned: f64) -> f64 {
    if spent <= 0.0 {
        return 0.0;
    }
    let recovered_ratio = cash_returned / spent;
    let sacrifice_ratio = if recovered_ratio >= CAPITAL_PROTECTED_RATIO {
        PAIRED_FLOOR_PROTECTED_MAX_SACRIFICE_RATIO
    } else {
        PAIRED_FLOOR_MAX_SACRIFICE_RATIO
    };
    spent * sacrifice_ratio
}

fn strong_grid_sell_amount(
    initial_shares: f64,
    current_shares: f64,
    other_side_shares: f64,
    step_idx: usize,
    redeem_hold_active: bool,
    bid: f64,
    cash_returned: f64,
    spent: f64,
) -> f64 {
    if current_shares <= 0.0 {
        return 0.0;
    }

    if redeem_hold_active && step_idx >= 1 {
        return (initial_shares * 0.20).min(current_shares);
    }

    match step_idx {
        0 | 1 => (initial_shares * STRONG_GRID_STEP_SELL_FRACTION).min(current_shares),
        _ if bid >= REDEEM_HOLD_RELEASE_BID => current_shares,
        _ => {
            let reserve = initial_shares * STRONG_GRID_RUNNER_FRACTION;
            let proposed_sell = (current_shares - reserve).max(0.0);
            if proposed_sell <= 0.0 {
                return 0.0;
            }

            let surplus_sell = proposed_sell.min((current_shares - other_side_shares).max(0.0));
            let paired_sell = proposed_sell - surplus_sell;
            if paired_sell <= 0.0 || spent <= 0.0 {
                return proposed_sell;
            }

            let floor_before = terminal_floor(cash_returned, current_shares, other_side_shares);
            let current_after = (current_shares - proposed_sell).max(0.0);
            let floor_after = terminal_floor(
                cash_returned + proposed_sell * bid,
                current_after,
                other_side_shares,
            );

            if floor_after >= spent || floor_after >= floor_before {
                proposed_sell
            } else {
                let max_floor_loss = max_paired_floor_sacrifice(spent, cash_returned);
                let floor_loss_per_share = (1.0 - bid).max(0.01);
                let paired_allowed_by_floor = (max_floor_loss / floor_loss_per_share).max(0.0);
                (surplus_sell + paired_sell.min(paired_allowed_by_floor)).min(proposed_sell)
            }
        }
    }
}

fn weak_exit_insurance_fraction(
    dev_zone: DeviationZone,
    time_pct: f64,
    capital_protected: bool,
) -> f64 {
    if time_pct >= 90.0 || dev_zone == DeviationZone::Runaway {
        return 0.0;
    }

    let zone_fraction = match dev_zone {
        DeviationZone::Near => WEAK_EXIT_NEAR_INSURANCE_FRACTION,
        DeviationZone::Moderate => WEAK_EXIT_MODERATE_INSURANCE_FRACTION,
        DeviationZone::Far => WEAK_EXIT_FAR_INSURANCE_FRACTION,
        DeviationZone::Unknown => WEAK_EXIT_MODERATE_INSURANCE_FRACTION,
        DeviationZone::Runaway => 0.0,
    };

    let time_cap: f64 = if time_pct < 30.0 {
        0.35
    } else if time_pct < 60.0 {
        0.30
    } else if time_pct < 80.0 {
        0.18
    } else {
        0.10
    };

    let protection_cap: f64 = if capital_protected { 0.18 } else { 0.35 };
    zone_fraction
        .min(time_cap)
        .min(protection_cap)
        .max(WEAK_EXIT_MIN_INSURANCE_FRACTION)
}

fn weak_exit_sell_plan(
    initial_shares: f64,
    current_shares: f64,
    other_side_shares: f64,
    bid: f64,
    side_win_probability: f64,
    dev_zone: DeviationZone,
    time_pct: f64,
    capital_protected: bool,
    spent: f64,
    cash_returned: f64,
) -> (f64, f64, f64) {
    if current_shares <= 0.0 {
        return (0.0, 0.0, 0.0);
    }

    let reserve_fraction = weak_exit_insurance_fraction(dev_zone, time_pct, capital_protected);
    let reserve_base = if initial_shares > 0.0 {
        initial_shares
    } else {
        current_shares
    };
    let insurance_reserve = (reserve_base * reserve_fraction).min(current_shares);
    let paired_core = current_shares.min(other_side_shares.max(0.0));
    let surplus = (current_shares - paired_core).max(0.0);
    let edge = sell_edge(bid, side_win_probability);
    let cash_gap = (spent - cash_returned).max(0.0);
    let cash_gap_shares = if bid > 0.0 { cash_gap / bid } else { 0.0 };

    // Preserve the redeem-capable paired core unless the market is clearly
    // overpaying the side's win probability, or the side is very unlikely late.
    let core_sell_fraction = if edge >= STRONG_STEP_3_EARLY_EDGE
        || (time_pct >= 80.0 && side_win_probability <= WEAK_CORE_PANIC_PROB)
    {
        0.35
    } else if time_pct >= WEAK_CORE_SELL_MIN_TIME_PCT && edge >= WEAK_CORE_SELL_EDGE {
        0.20
    } else if time_pct >= 80.0 && edge >= 0.04 {
        0.12
    } else {
        0.0
    };
    let core_sell = paired_core
        .min(cash_gap_shares)
        .min(paired_core * core_sell_fraction);

    let planned_sell = (surplus + core_sell).min(current_shares);
    let sell_amount = planned_sell.min((current_shares - insurance_reserve).max(0.0));
    let reserve_shares = (current_shares - sell_amount).max(0.0);

    (sell_amount, reserve_shares, reserve_fraction)
}

fn insurance_tail_release_allowed(
    bid: f64,
    time_pct: f64,
    side_is_itm: bool,
    side_win_probability: f64,
) -> bool {
    !side_is_itm
        && (bid >= REDEEM_HOLD_RELEASE_BID
            || (time_pct >= INSURANCE_TAIL_RELEASE_TIME_PCT
                && should_sell_by_probability(bid, side_win_probability)))
}

fn cap_sell_by_insurance_tail(
    proposed_sell: f64,
    current_shares: f64,
    reserve_shares: f64,
    bid: f64,
    time_pct: f64,
    side_is_itm: bool,
    side_win_probability: f64,
) -> (f64, bool) {
    if proposed_sell <= 0.0 || current_shares <= 0.0 || reserve_shares <= 0.0 {
        return (proposed_sell.max(0.0), false);
    }

    if insurance_tail_release_allowed(bid, time_pct, side_is_itm, side_win_probability) {
        return (proposed_sell.min(current_shares), false);
    }

    let max_sell_without_tail = (current_shares - reserve_shares).max(0.0);
    let capped_sell = proposed_sell.min(max_sell_without_tail);
    (capped_sell, capped_sell + 0.000001 < proposed_sell)
}

fn emergency_surplus_sell_amount(
    side_shares: f64,
    other_side_shares: f64,
    side_is_itm: bool,
    side_bid: f64,
    side_win_probability: f64,
) -> f64 {
    if side_shares <= 0.0
        || side_is_itm
        || !should_sell_by_probability(side_bid, side_win_probability)
    {
        return 0.0;
    }

    if other_side_shares <= 0.0 {
        side_shares
    } else {
        (side_shares - other_side_shares).max(0.0)
    }
}

fn weak_scalp_ask_cap(current_atr: f64) -> f64 {
    if current_atr >= ATR_EXTREME_MIN {
        0.18
    } else if current_atr >= ATR_HIGH_MIN {
        0.20
    } else if current_atr >= ATR_LOW_MAX {
        0.22
    } else {
        0.12
    }
}

fn weak_scalp_max_deviation_pct(current_atr: f64) -> f64 {
    if current_atr >= ATR_EXTREME_MIN {
        0.06
    } else if current_atr >= ATR_HIGH_MIN {
        0.08
    } else if current_atr >= ATR_LOW_MAX {
        0.06
    } else {
        0.025
    }
}

fn weak_scalp_exit_target(avg_entry_price: f64, current_atr: f64) -> f64 {
    let profit_step = if current_atr >= ATR_HIGH_MIN {
        0.08
    } else {
        0.06
    };
    (avg_entry_price + profit_step).clamp(0.14, 0.42)
}

fn scalp_key(window_number: usize, side: &str) -> (usize, String) {
    (window_number, side.to_string())
}

fn velocity_blocks_redeem_hold(
    is_up_strong: bool,
    spot_signal: SpotSignalSnapshot,
    current_atr: f64,
) -> bool {
    let Some(velocity) = spot_velocity(spot_signal) else {
        return false;
    };
    let limit = redeem_hold_counter_velocity_limit(current_atr);
    (is_up_strong && velocity < -limit) || (!is_up_strong && velocity > limit)
}

fn should_hold_winner_for_redeem(
    is_up_strong: bool,
    dev: PtbDeviation,
    time_pct: f64,
    secs_to_end: i64,
    strong_bid: f64,
    current_atr: f64,
    spot_signal: SpotSignalSnapshot,
) -> bool {
    if strong_bid < 0.66 || !strong_side_is_itm(is_up_strong, dev) {
        return false;
    }
    if velocity_blocks_redeem_hold(is_up_strong, spot_signal, current_atr) {
        return false;
    }

    let enough_conviction = dev.zone == DeviationZone::Runaway
        || (dev.zone == DeviationZone::Far && current_atr >= 18.0)
        || (dev.zone == DeviationZone::Moderate && time_pct >= 70.0);
    let close_enough_to_redeem = time_pct >= 45.0 || secs_to_end <= 240;

    enough_conviction && close_enough_to_redeem
}

impl TradeStrategy for DynamicGridStrategy {
    /**
     * Check pre-start entry conditions for a NEXT window.
     */
    fn check_pre_start_entry(
        &mut self,
        config: &Config,
        prices: &PricesState,
        _market: &MarketWindow,
        _spot_price: Option<f64>,
        window_number: usize,
        secs_to_start: i64,
        current_btc_atr: f64,
        _spot_signal: SpotSignalSnapshot,
    ) -> Option<EntrySignal> {
        if !config.pre_start_entry.enabled {
            return None;
        }
        if self.entered_windows.contains(&window_number) {
            return None;
        }

        // Регулируем закуп строго в заданном временном диапазоне [120 сек - 5 сек] перед стартом
        if secs_to_start < config.pre_start_entry.min_seconds_before_start
            || secs_to_start > config.pre_start_entry.max_seconds_before_start
        {
            return None;
        }

        if current_btc_atr < config.min_btc_atr {
            return None;
        }

        let regime = entry_regime(current_btc_atr);

        let up_ask = prices.up.ask;
        let dn_ask = prices.down.ask;

        if up_ask <= 0.0 || dn_ask <= 0.0 {
            return None;
        }

        // Допускаем сбалансированное отклонение на базе динамических порогов из конфига
        let min_ask = config.pre_start_entry.min_side_ask;
        let max_ask = config.pre_start_entry.max_side_ask;
        if up_ask < min_ask || up_ask > max_ask || dn_ask < min_ask || dn_ask > max_ask {
            return None;
        }

        let ask_spread = (up_ask - dn_ask).abs();
        let combined_ask = up_ask + dn_ask;
        let directional_entry = ask_spread > 0.04;
        let max_combined_ask = if directional_entry {
            regime
                .max_combined_ask
                .min(ENTRY_DIRECTIONAL_MAX_COMBINED_ASK)
        } else {
            regime.max_combined_ask
        };
        if ask_spread > ENTRY_MAX_ASK_SPREAD || combined_ask > max_combined_ask {
            return None;
        }
        if directional_entry && current_btc_atr < 30.0 {
            return None;
        }

        let directional_budget_multiplier = if !directional_entry {
            1.0
        } else if ask_spread >= 0.12 {
            0.55
        } else if ask_spread >= 0.08 {
            0.70
        } else {
            0.85
        };
        let entry_budget_multiplier = regime.budget_multiplier * directional_budget_multiplier;
        let entry_cheaper_side_ratio = if directional_entry {
            0.50
        } else {
            regime.cheaper_side_ratio
        };

        self.entered_windows.insert(window_number);
        Some(EntrySignal {
            up_ask,
            down_ask: dn_ask,
            budget_multiplier: entry_budget_multiplier,
            cheaper_side_ratio: entry_cheaper_side_ratio,
            mode: EntryMode::Both,
            reason: format!(
                "{}{}_atr_{:.2}_combined_{:.2}_spread_{:.2}_budget_mult_{:.2}",
                regime.reason,
                if directional_entry {
                    "_directional"
                } else {
                    ""
                },
                current_btc_atr,
                combined_ask,
                ask_spread,
                entry_budget_multiplier
            ),
        })
    }

    /**
     * Evaluate live tick exit/buy rules for a CURRENT window.
     */
    fn process_live_tick(
        &mut self,
        config: &Config,
        prices: &PricesState,
        spot_price: Option<f64>,
        market: &MarketWindow,
        win_state: &WindowState,
        secs_to_end: i64,
        current_atr: f64,
        spot_signal: SpotSignalSnapshot,
    ) -> Vec<OrderSignal> {
        let mut signals = vec![];
        let window_number = win_state.window_number;

        let up_bid = prices.up.bid;
        let dn_bid = prices.down.bid;
        let up_ask = prices.up.ask;
        let dn_ask = prices.down.ask;
        let mut projected_up_shares = win_state.up_shares;
        let mut projected_down_shares = win_state.down_shares;
        let mut projected_cash_returned = win_state.cash_returned;

        // Рассчитываем временной фильтр в процентах (0.0% - 100.0%)
        let duration_ms = match (
            chrono::DateTime::parse_from_rfc3339(&market.start_time),
            chrono::DateTime::parse_from_rfc3339(&market.end_time),
        ) {
            (Ok(s), Ok(e)) => (e.timestamp_millis() - s.timestamp_millis()) as f64,
            _ => 900_000.0, // 15m fallback
        };
        let duration_sec = duration_ms / 1000.0;
        let elapsed_sec = (duration_sec - secs_to_end as f64).clamp(0.0, duration_sec);
        let time_pct = (elapsed_sec / duration_sec) * 100.0;

        // PTB deviation uses both absolute dollars and percent.
        let dev = ptb_deviation(market, spot_price);

        // Сильная сторона определяется текущим лидерством по bid, а не только стартовым перекосом.
        let is_up_strong = live_leader_is_up(
            up_bid,
            dn_bid,
            win_state.initial_up_shares,
            win_state.initial_down_shares,
        );

        let initial_up = win_state.initial_up_shares;
        let initial_dn = win_state.initial_down_shares;
        let is_strong_side_cheaper = if is_up_strong {
            initial_up > initial_dn
        } else {
            initial_dn > initial_up
        };

        let recovered_ratio = if win_state.spent > 0.0 {
            win_state.cash_returned / win_state.spent
        } else {
            0.0
        };
        let capital_protected = recovered_ratio >= CAPITAL_PROTECTED_RATIO;

        let strong_bid = if is_up_strong { up_bid } else { dn_bid };
        let up_win_probability =
            side_win_probability(true, dev, current_atr, secs_to_end, spot_signal);
        let down_win_probability =
            side_win_probability(false, dev, current_atr, secs_to_end, spot_signal);
        let redeem_hold_active = should_hold_winner_for_redeem(
            is_up_strong,
            dev,
            time_pct,
            secs_to_end,
            strong_bid,
            current_atr,
            spot_signal,
        );

        let state = self.states.entry(window_number).or_insert(StrategyState {
            up_sold: false,
            down_sold: false,
            first_sold_side: None,
            ptb_crossed: false,
            ptb_baseline: None,
        });

        // ─── EMERGENCY RULE: 15% remaining time stop (Unconditional!) ───────
        let emergency_time_threshold = (duration_sec * 0.15) as i64; // 135s for 15m

        if secs_to_end <= emergency_time_threshold && secs_to_end > -10 {
            // Late salvage sells only OTM surplus. Paired core stays for redeem.
            let up_is_itm = strong_side_is_itm(true, dev);
            let down_is_itm = strong_side_is_itm(false, dev);
            let up_emergency_sell = emergency_surplus_sell_amount(
                projected_up_shares,
                projected_down_shares,
                up_is_itm,
                up_bid,
                up_win_probability,
            );
            if up_emergency_sell > 0.000001
                && up_bid >= 0.20
                && !state.up_sold
                && !(redeem_hold_active && is_up_strong)
            {
                projected_up_shares = (projected_up_shares - up_emergency_sell).max(0.0);
                projected_cash_returned += up_emergency_sell * up_bid;
                if projected_up_shares <= 0.000001 {
                    state.up_sold = true;
                    self.weak_exit_insurance_reserve
                        .insert(scalp_key(window_number, "UP"), 0.0);
                    self.scalp_active_shares
                        .insert(scalp_key(window_number, "UP"), 0.0);
                    self.scalp_active_cost
                        .insert(scalp_key(window_number, "UP"), 0.0);
                }
                signals.push(OrderSignal {
                    side: "UP".to_string(),
                    is_buy: false,
                    amount: up_emergency_sell,
                    price: up_bid,
                    reason: format!(
                        "emergency_15pct_time_stop_otm_surplus_bid_ge_0.20_p_{:.2}_keep_paired_{:.4}",
                        up_win_probability,
                        projected_up_shares.min(projected_down_shares),
                    ),
                });
            }
            let down_emergency_sell = emergency_surplus_sell_amount(
                projected_down_shares,
                projected_up_shares,
                down_is_itm,
                dn_bid,
                down_win_probability,
            );
            if down_emergency_sell > 0.000001
                && dn_bid >= 0.20
                && !state.down_sold
                && !(redeem_hold_active && !is_up_strong)
            {
                projected_down_shares = (projected_down_shares - down_emergency_sell).max(0.0);
                projected_cash_returned += down_emergency_sell * dn_bid;
                if projected_down_shares <= 0.000001 {
                    state.down_sold = true;
                    self.weak_exit_insurance_reserve
                        .insert(scalp_key(window_number, "DOWN"), 0.0);
                    self.scalp_active_shares
                        .insert(scalp_key(window_number, "DOWN"), 0.0);
                    self.scalp_active_cost
                        .insert(scalp_key(window_number, "DOWN"), 0.0);
                }
                signals.push(OrderSignal {
                    side: "DOWN".to_string(),
                    is_buy: false,
                    amount: down_emergency_sell,
                    price: dn_bid,
                    reason: format!(
                        "emergency_15pct_time_stop_otm_surplus_bid_ge_0.20_p_{:.2}_keep_paired_{:.4}",
                        down_win_probability,
                        projected_up_shares.min(projected_down_shares),
                    ),
                });
            }
        }

        // ─── ТРЕХМЕРНАЯ АДАПТИВНАЯ СЕТКА СИЛЬНОЙ СТОРОНЫ (TVDS STRONG GRID) ───
        // Ступень 1: частичный de-risk. Не продаем слишком много, чтобы оставить upside до redeem.
        let step1_target = if time_pct < 30.0 && current_atr >= 30.0 {
            0.62 // Резкий старт: не спешим продавать лидера
        } else if time_pct >= 60.0 || current_atr < 15.0 {
            0.54 // Затухание или тухляк: выходим быстрее
        } else {
            0.58 // Стандарт
        };

        // Ступень 2: обычный take-profit, но redeem-hold может ее заблокировать.
        let step2_target = if time_pct < 60.0 && current_atr >= 30.0 {
            0.72 // Сильный мид-гейм импульс
        } else if time_pct >= 80.0 || current_atr < 15.0 {
            0.60 // Сброс перед финалом / при затухании
        } else {
            0.66 // Стандарт
        };

        // Ступень 3 (Раннер — 20%):
        let step3_target = if is_strong_side_cheaper {
            // Крупная дешевая сторона (60%): даем прибыли течь при мощном тренде
            if dev.zone.is_far_or_runaway() && current_atr >= 30.0 {
                0.92 // Держим до победных 92 центов
            } else if time_pct >= 80.0 || current_atr < 15.0 {
                0.68 // Сейв в боковике/конце
            } else {
                0.75 // Стандарт
            }
        } else {
            // Менее объемная дорогая сторона (40%): выходим умеренно
            if dev.zone.is_far_or_runaway() && current_atr >= 30.0 {
                0.85
            } else if time_pct >= 80.0 || current_atr < 15.0 {
                0.68
            } else {
                0.75
            }
        };

        let strong_velocity_bias = velocity_bias_for_side(is_up_strong, spot_signal, current_atr);
        let strong_velocity_label = velocity_bias_label(strong_velocity_bias);
        let strong_grid = vec![
            adjust_strong_grid_target(step1_target, 0, strong_velocity_bias, time_pct),
            adjust_strong_grid_target(step2_target, 1, strong_velocity_bias, time_pct),
            adjust_strong_grid_target(step3_target, 2, strong_velocity_bias, time_pct),
        ];

        // ─── 1. МОНИТОРИНГ СЕТКИ ПРОДАЖ ДЛЯ СИЛЬНОЙ СТОРОНЫ ───
        if is_up_strong {
            // UP - Сильная нога
            if !state.up_sold && projected_up_shares > 0.0 {
                let current_step = self.up_steps_hit.entry(window_number).or_insert(0);
                if *current_step < strong_grid.len() {
                    let target = strong_grid[*current_step];
                    if up_bid >= target
                        && strong_grid_step_allowed(
                            *current_step,
                            time_pct,
                            up_bid,
                            up_win_probability,
                        )
                    {
                        let hold_blocks_mid_exit = redeem_hold_active
                            && *current_step >= 1
                            && up_bid < REDEEM_HOLD_RELEASE_BID;
                        if !hold_blocks_mid_exit {
                            let proposed_sell_amount = strong_grid_sell_amount(
                                win_state.initial_up_shares,
                                projected_up_shares,
                                projected_down_shares,
                                *current_step,
                                redeem_hold_active,
                                up_bid,
                                projected_cash_returned,
                                win_state.spent,
                            );
                            let up_reserve = *self
                                .weak_exit_insurance_reserve
                                .get(&scalp_key(window_number, "UP"))
                                .unwrap_or(&0.0);
                            let up_is_itm = strong_side_is_itm(true, dev);
                            let (sell_amount, insurance_tail_capped) = cap_sell_by_insurance_tail(
                                proposed_sell_amount,
                                projected_up_shares,
                                up_reserve,
                                up_bid,
                                time_pct,
                                up_is_itm,
                                up_win_probability,
                            );

                            if sell_amount > 0.0 {
                                if redeem_hold_active && *current_step >= 1 {
                                    *current_step = strong_grid.len();
                                } else {
                                    *current_step += 1;
                                }
                                projected_up_shares = (projected_up_shares - sell_amount).max(0.0);
                                projected_cash_returned += sell_amount * up_bid;
                                if !redeem_hold_active
                                    && *current_step >= strong_grid.len()
                                    && projected_up_shares <= 0.000001
                                {
                                    state.up_sold = true;
                                }
                                if state.first_sold_side.is_none() {
                                    state.first_sold_side = Some("UP".to_string());
                                }

                                signals.push(OrderSignal {
                                    side: "UP".to_string(),
                                    is_buy: false,
                                    amount: sell_amount,
                                    price: up_bid,
                                    reason: format!(
                                        "tvds_strong_grid_exit_step_{}_{:.2}_p_{:.2}_edge_{:.2}_zone_{}_usd_{:.2}_pct_{:.4}{}{}{}_vel_{}",
                                        *current_step,
                                        target,
                                        up_win_probability,
                                        sell_edge(up_bid, up_win_probability),
                                        dev.zone.as_str(),
                                        dev.abs_usd,
                                        dev.pct_abs,
                                        if redeem_hold_active { "_redeem_hold_runner" } else { "" },
                                        if insurance_tail_capped {
                                            "_insurance_tail_kept"
                                        } else {
                                            ""
                                        },
                                        if *current_step >= strong_grid.len()
                                            && up_bid < REDEEM_HOLD_RELEASE_BID
                                            && !insurance_tail_capped
                                            && sell_amount + 0.000001 < proposed_sell_amount
                                        {
                                            "_paired_floor_protected"
                                        } else {
                                            ""
                                        },
                                        strong_velocity_label
                                    ),
                                });
                            }
                        }
                    }
                }
            }
        } else {
            // DOWN - Сильная нога
            if !state.down_sold && projected_down_shares > 0.0 {
                let current_step = self.dn_steps_hit.entry(window_number).or_insert(0);
                if *current_step < strong_grid.len() {
                    let target = strong_grid[*current_step];
                    if dn_bid >= target
                        && strong_grid_step_allowed(
                            *current_step,
                            time_pct,
                            dn_bid,
                            down_win_probability,
                        )
                    {
                        let hold_blocks_mid_exit = redeem_hold_active
                            && *current_step >= 1
                            && dn_bid < REDEEM_HOLD_RELEASE_BID;
                        if !hold_blocks_mid_exit {
                            let proposed_sell_amount = strong_grid_sell_amount(
                                win_state.initial_down_shares,
                                projected_down_shares,
                                projected_up_shares,
                                *current_step,
                                redeem_hold_active,
                                dn_bid,
                                projected_cash_returned,
                                win_state.spent,
                            );
                            let down_reserve = *self
                                .weak_exit_insurance_reserve
                                .get(&scalp_key(window_number, "DOWN"))
                                .unwrap_or(&0.0);
                            let down_is_itm = strong_side_is_itm(false, dev);
                            let (sell_amount, insurance_tail_capped) = cap_sell_by_insurance_tail(
                                proposed_sell_amount,
                                projected_down_shares,
                                down_reserve,
                                dn_bid,
                                time_pct,
                                down_is_itm,
                                down_win_probability,
                            );

                            if sell_amount > 0.0 {
                                if redeem_hold_active && *current_step >= 1 {
                                    *current_step = strong_grid.len();
                                } else {
                                    *current_step += 1;
                                }
                                projected_down_shares =
                                    (projected_down_shares - sell_amount).max(0.0);
                                projected_cash_returned += sell_amount * dn_bid;
                                if !redeem_hold_active
                                    && *current_step >= strong_grid.len()
                                    && projected_down_shares <= 0.000001
                                {
                                    state.down_sold = true;
                                }
                                if state.first_sold_side.is_none() {
                                    state.first_sold_side = Some("DOWN".to_string());
                                }

                                signals.push(OrderSignal {
                                    side: "DOWN".to_string(),
                                    is_buy: false,
                                    amount: sell_amount,
                                    price: dn_bid,
                                    reason: format!(
                                        "tvds_strong_grid_exit_step_{}_{:.2}_p_{:.2}_edge_{:.2}_zone_{}_usd_{:.2}_pct_{:.4}{}{}{}_vel_{}",
                                        *current_step,
                                        target,
                                        down_win_probability,
                                        sell_edge(dn_bid, down_win_probability),
                                        dev.zone.as_str(),
                                        dev.abs_usd,
                                        dev.pct_abs,
                                        if redeem_hold_active { "_redeem_hold_runner" } else { "" },
                                        if insurance_tail_capped {
                                            "_insurance_tail_kept"
                                        } else {
                                            ""
                                        },
                                        if *current_step >= strong_grid.len()
                                            && dn_bid < REDEEM_HOLD_RELEASE_BID
                                            && !insurance_tail_capped
                                            && sell_amount + 0.000001 < proposed_sell_amount
                                        {
                                            "_paired_floor_protected"
                                        } else {
                                            ""
                                        },
                                        strong_velocity_label
                                    ),
                                });
                            }
                        }
                    }
                }
            }
        }

        // ─── 2. WEAKSCALP: микро-докупка слабой стороны только на реальном отскоке ───
        let weak_side = if is_up_strong { "DOWN" } else { "UP" };
        let weak_is_up = weak_side == "UP";
        let weak_bid = if weak_is_up { up_bid } else { dn_bid };
        let weak_ask = if weak_is_up { up_ask } else { dn_ask };
        let strong_remaining = if is_up_strong {
            projected_up_shares
        } else {
            projected_down_shares
        };
        let strong_initial = if is_up_strong {
            win_state.initial_up_shares
        } else {
            win_state.initial_down_shares
        };
        let weak_velocity_bias = velocity_bias_for_side(weak_is_up, spot_signal, current_atr);
        let weak_velocity_label = velocity_bias_label(weak_velocity_bias);
        let weak_key = scalp_key(window_number, weak_side);
        let active_scalp_shares = *self.scalp_active_shares.get(&weak_key).unwrap_or(&0.0);
        let active_scalp_cost = *self.scalp_active_cost.get(&weak_key).unwrap_or(&0.0);
        let mut scalp_exit_side: Option<String> = None;

        if active_scalp_shares > 0.0 && active_scalp_cost > 0.0 {
            let avg_entry_price = active_scalp_cost / active_scalp_shares;
            let scalp_target = weak_scalp_exit_target(avg_entry_price, current_atr);
            if weak_bid >= scalp_target {
                let scalp_sell_amount = active_scalp_shares.min(if weak_is_up {
                    projected_up_shares
                } else {
                    projected_down_shares
                });
                self.scalp_active_shares.insert(weak_key.clone(), 0.0);
                self.scalp_active_cost.insert(weak_key.clone(), 0.0);
                scalp_exit_side = Some(weak_side.to_string());
                if weak_is_up {
                    projected_up_shares = (projected_up_shares - scalp_sell_amount).max(0.0);
                } else {
                    projected_down_shares = (projected_down_shares - scalp_sell_amount).max(0.0);
                }
                projected_cash_returned += scalp_sell_amount * weak_bid;
                signals.push(OrderSignal {
                    side: weak_side.to_string(),
                    is_buy: false,
                    amount: scalp_sell_amount,
                    price: weak_bid,
                    reason: format!(
                        "weak_scalp_exit_{}_target_{:.2}_avg_{:.2}_vel_{}",
                        weak_side.to_lowercase(),
                        scalp_target,
                        avg_entry_price,
                        weak_velocity_label
                    ),
                });
            }
        } else if time_pct <= 55.0
            && !capital_protected
            && recovered_ratio >= WEAK_SCALP_MIN_RECOVERED_RATIO
            && dev.known
            && matches!(dev.zone, DeviationZone::Near | DeviationZone::Moderate)
            && dev.pct_abs <= weak_scalp_max_deviation_pct(current_atr)
            && weak_ask > 0.0
            && weak_ask <= weak_scalp_ask_cap(current_atr)
            && strong_initial > 0.0
            && strong_remaining / strong_initial >= WEAK_SCALP_MIN_STRONG_REMAINING_FRACTION
            && weak_velocity_bias == 1
        {
            let tranche_count = *self.scalp_tranches.get(&weak_key).unwrap_or(&0);
            if tranche_count < WEAK_SCALP_MAX_TRANCHES_PER_SIDE {
                let buy_usd = win_state.spent * WEAK_SCALP_USD_FRACTION_OF_SPENT;
                if buy_usd >= WEAK_SCALP_MIN_USD {
                    let buy_shares = buy_usd / weak_ask;
                    self.scalp_tranches
                        .insert(weak_key.clone(), tranche_count + 1);
                    self.scalp_active_shares
                        .insert(weak_key.clone(), buy_shares);
                    self.scalp_active_cost.insert(weak_key.clone(), buy_usd);
                    signals.push(OrderSignal {
                        side: weak_side.to_string(),
                        is_buy: true,
                        amount: buy_usd,
                        price: weak_ask,
                        reason: format!(
                            "weak_scalp_buy_{}_tranche_{}_askcap_{:.2}_devpct_{:.4}_recovered_{:.2}_vel_{}",
                            weak_side.to_lowercase(),
                            tranche_count + 1,
                            weak_scalp_ask_cap(current_atr),
                            dev.pct_abs,
                            recovered_ratio,
                            weak_velocity_label
                        ),
                    });
                }
            }
        }

        // ─── 3. ТРИГГЕР ПЕРЕСЕЧЕНИЯ ЛИНИИ СТАРТА (CROSSOVER БЛОК) ───
        if let Some(ref first_sold) = state.first_sold_side {
            let second_side = if first_sold == "UP" { "DOWN" } else { "UP" };
            let second_is_up = second_side == "UP";
            let second_bid = if second_side == "UP" { up_bid } else { dn_bid };
            let second_shares = if second_is_up {
                projected_up_shares
            } else {
                projected_down_shares
            };
            let second_initial_shares = if second_is_up {
                win_state.initial_up_shares
            } else {
                win_state.initial_down_shares
            };
            let second_sold = if second_is_up {
                state.up_sold
            } else {
                state.down_sold
            };
            let second_is_current_strong = second_is_up == is_up_strong;
            let second_is_itm = strong_side_is_itm(second_is_up, dev);
            let other_shares = if second_is_up {
                projected_down_shares
            } else {
                projected_up_shares
            };
            let second_win_probability = if second_is_up {
                up_win_probability
            } else {
                down_win_probability
            };
            let second_edge = sell_edge(second_bid, second_win_probability);

            if second_shares > 0.0
                && !second_sold
                && !second_is_current_strong
                && !second_is_itm
                && scalp_exit_side.as_deref() != Some(second_side)
            {
                // Инициализируем бейслайн спота к страйку при первой продаже
                if state.ptb_baseline.is_none() {
                    if let (Some(spot), Some(ptb)) = (spot_price, market.price_to_beat) {
                        let rel = if spot < ptb {
                            "BELOW".to_string()
                        } else {
                            "ABOVE".to_string()
                        };
                        state.ptb_baseline = Some(rel);
                    }
                }

                // Мониторим пересечение спот-курсом уровня страйка обратно (crossover)
                if !state.ptb_crossed {
                    if let (Some(spot), Some(ptb), Some(ref baseline)) =
                        (spot_price, market.price_to_beat, &state.ptb_baseline)
                    {
                        if first_sold == "UP" {
                            if baseline == "ABOVE" && spot < ptb {
                                state.ptb_crossed = true;
                            }
                            if spot > ptb {
                                state.ptb_baseline = Some("ABOVE".to_string());
                            }
                        } else if first_sold == "DOWN" {
                            if baseline == "BELOW" && spot > ptb {
                                state.ptb_crossed = true;
                            }
                            if spot < ptb {
                                state.ptb_baseline = Some("BELOW".to_string());
                            }
                        }
                    }
                }

                // ─── АДАПТИВНАЯ МОДЕЛЬ ВЫХОДА ДЛЯ СЛАБОЙ СТОРОНЫ (TVDS MATRIX) ───
                let mut should_sell = false;
                let mut reason_str = String::new();

                // 1. Определение целевого Bid на основе матрицы TVDS (Время × Волатильность × Отклонение)
                let (weak_target, zone_desc) = if time_pct < 30.0 {
                    // Ранняя фаза (Времени полно, распада нет): ждем глубокого разворота
                    if current_atr >= 30.0 {
                        (0.65, "early_high_vol_wait_reversal") // Высокая волатильность: ждем полноценный взлет
                    } else if current_atr < 15.0 {
                        (0.40, "early_sluggish_exit_fast") // Боковик: выходим при первой же возможности
                    } else {
                        (0.50, "early_normal_vol_wait")
                    }
                } else if time_pct < 60.0 {
                    // Средняя фаза (Разгар битвы)
                    if dev.zone == DeviationZone::Near {
                        (0.65, "mid_near_strike_wait_reversal") // Сверхблизко: выжидаем полноценный разворот
                    } else if dev.zone == DeviationZone::Moderate {
                        (0.30, "mid_moderate_take_30") // Умеренно: фиксируем отличные x2 от закупа по 0.15
                    } else {
                        // Спот ушел далеко
                        if current_atr >= 30.0 {
                            (0.22, "mid_far_high_vol_wait") // Волатильно: ждем небольшого отскока
                        } else if current_atr < 15.0 {
                            (0.17, "mid_far_sluggish_dump_17") // Затухание: сливаем по 0.17 для сохранения кэша
                        } else {
                            (0.20, "mid_far_normal_vol_wait")
                        }
                    }
                } else if time_pct < 80.0 {
                    // Поздняя фаза (Пошел сильный временной распад)
                    if dev.zone == DeviationZone::Near {
                        (0.45, "late_near_strike_wait_reversal")
                    } else if dev.zone == DeviationZone::Moderate || dev.zone == DeviationZone::Far
                    {
                        (0.20, "late_moderate_take_20") // Быстро сбрасываем в легкий плюс
                    } else {
                        (0.12, "late_far_dump_12") // Спасаем крохи
                    }
                } else if time_pct < 90.0 {
                    // Финальная фаза (Последние минуты, распад критический)
                    if dev.zone == DeviationZone::Near {
                        (0.30, "end_near_strike_reversal_hope")
                    } else {
                        (0.10, "end_far_emergency_dump_10") // Шансов почти нет: забираем 10 центов вместо 0
                    }
                } else {
                    // Фаза экспирации (Последний шанс перед сгоранием в ноль)
                    (0.08, "expiration_unconditional_dump_08")
                };
                let weak_velocity_bias =
                    velocity_bias_for_side(second_side == "UP", spot_signal, current_atr);
                let weak_velocity_label = velocity_bias_label(weak_velocity_bias);
                let mut weak_target =
                    adjust_weak_exit_target(weak_target, weak_velocity_bias, time_pct);
                if capital_protected {
                    weak_target = capital_protected_weak_exit_target(weak_target, time_pct);
                }

                if second_bid >= weak_target
                    && should_sell_by_probability(second_bid, second_win_probability)
                {
                    should_sell = true;
                    reason_str = format!(
                        "tvds_weak_exit_{}_bid_ge_{:.2}_p_{:.2}_edge_{:.2}_zone_{}_usd_{:.2}_pct_{:.4}_vel_{}{}",
                        zone_desc,
                        weak_target,
                        second_win_probability,
                        second_edge,
                        dev.zone.as_str(),
                        dev.abs_usd,
                        dev.pct_abs,
                        weak_velocity_label,
                        if capital_protected {
                            format!("_capital_protected_{:.2}", recovered_ratio)
                        } else {
                            String::new()
                        }
                    );
                }

                // 2. Дополнительная страховка: Окупаемость раунда по формуле безубытка в поздней фазе (60% - 80%)
                if !should_sell && time_pct >= 60.0 && time_pct < 80.0 {
                    let min_safe_price =
                        (win_state.spent - projected_cash_returned) / second_shares;
                    if min_safe_price > 0.0
                        && min_safe_price < 0.65
                        && second_bid >= min_safe_price
                        && should_sell_by_probability(second_bid, second_win_probability)
                    {
                        should_sell = true;
                        reason_str = format!(
                            "tvds_late_exact_breakeven_bid_ge_{:.2}_p_{:.2}_edge_{:.2}",
                            min_safe_price, second_win_probability, second_edge
                        );
                    }
                }

                if should_sell {
                    let (sell_amount, reserve_shares, reserve_fraction) = weak_exit_sell_plan(
                        second_initial_shares,
                        second_shares,
                        other_shares,
                        second_bid,
                        second_win_probability,
                        dev.zone,
                        time_pct,
                        capital_protected,
                        win_state.spent,
                        projected_cash_returned,
                    );

                    if sell_amount <= 0.000001 {
                        return signals;
                    }

                    let sold_all_second = sell_amount >= second_shares - 0.000001;
                    if sold_all_second {
                        if second_side == "UP" {
                            state.up_sold = true;
                        } else {
                            state.down_sold = true;
                        }
                    }

                    let second_key = scalp_key(window_number, second_side);
                    let active_shares = *self.scalp_active_shares.get(&second_key).unwrap_or(&0.0);
                    let active_cost = *self.scalp_active_cost.get(&second_key).unwrap_or(&0.0);
                    if active_shares > 0.0 {
                        let new_active_shares = (active_shares - sell_amount)
                            .max(0.0)
                            .min((second_shares - sell_amount).max(0.0));
                        let new_active_cost = if new_active_shares > 0.0 {
                            active_cost * (new_active_shares / active_shares)
                        } else {
                            0.0
                        };
                        self.scalp_active_shares
                            .insert(second_key.clone(), new_active_shares);
                        self.scalp_active_cost.insert(second_key, new_active_cost);
                    }

                    if sold_all_second || reserve_shares <= 0.000001 {
                        self.weak_exit_insurance_reserve
                            .insert(scalp_key(window_number, second_side), 0.0);
                    } else {
                        self.weak_exit_insurance_reserve.insert(
                            scalp_key(window_number, second_side),
                            reserve_shares.min((second_shares - sell_amount).max(0.0)),
                        );
                    }

                    signals.push(OrderSignal {
                        side: second_side.to_string(),
                        is_buy: false,
                        amount: sell_amount,
                        price: second_bid,
                        reason: format!(
                            "{}_sell_{:.4}_reserve_{:.4}_insurance_{:.2}",
                            reason_str, sell_amount, reserve_shares, reserve_fraction
                        ),
                    });
                }
            }
        }

        signals
    }

    fn get_strategy_state(&self, window_number: usize) -> Option<StrategyState> {
        self.states.get(&window_number).cloned()
    }
}
