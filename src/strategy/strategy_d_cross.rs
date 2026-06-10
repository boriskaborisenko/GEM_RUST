use crate::cex_micro::cex_vetoes_cheap_entry;
use crate::client::{MarketWindow, PricesState};
use crate::config::Config;
use crate::mid_cross_tracker::{LeadSide, MidCrossSnapshot, MID_CROSS_ARM_TIME_PCT};
use crate::redeem_hold::{evaluate_redeem_hold, side_is_itm, RedeemHoldInput};
use crate::strategy::{
    CexMicroSnapshot, EntrySignal, OrderSignal, SpotSignalSnapshot, StrategyState, TradeStrategy,
};
use crate::trader::WindowState;
use std::collections::HashMap;

const DCROSS_MIN_VALID_ATR: f64 = 1.0;
const DCROSS_MIN_TRADE_USD: f64 = 1.0;
const DCROSS_ENTRY_BUDGET_MULTIPLIER: f64 = 0.85;
const DCROSS_MAX_ENTRY_TIME_PCT: f64 = 65.0;
const DCROSS_MIN_SECS_TO_END: i64 = 120;
const DCROSS_MAX_CHEAP_ASK: f64 = 0.48;
const DCROSS_MAX_EXPENSIVE_MID: f64 = 0.62;
const DCROSS_MAX_ABS_GAP_Z: f64 = 0.55;
const DCROSS_MIN_ENTRY_EDGE: f64 = 0.04;
const DCROSS_VELOCITY_DRIFT_FRACTION: f64 = 0.20;
const DCROSS_MIN_VELOCITY_CONFIRM_USD_PER_SEC: f64 = 0.10;
const DCROSS_FINAL_TIME_PCT: f64 = 88.0;
const DCROSS_FINAL_SECS_TO_END: i64 = 25;
const DCROSS_THESIS_LOCK_MIN_GAP: f64 = 0.15;
const DCROSS_LATE_TIME_PCT: f64 = 75.0;
const DCROSS_RUNNER_FRACTION: f64 = 0.35;
const DCROSS_EDGE_STEP1: f64 = 0.03;
const DCROSS_EDGE_STEP2: f64 = 0.05;
const DCROSS_EMERGENCY_MIN_BID: f64 = 0.08;
const DCROSS_EMERGENCY_MIN_FAIR: f64 = 0.08;
const DCROSS_SELL_MIN_SHARES: f64 = 0.000001;
const DCROSS_MAX_CHAOTIC_CROSSES: u32 = 2;

#[derive(Debug, Clone)]
struct DCrossWindowState {
    entry_side: Option<String>,
    entry_ask: f64,
    pending_entry_ask: f64,
    initial_entry_shares: f64,
    sell_steps_done: usize,
    thesis_locked: bool,
    emergency_sold: bool,
}

pub struct DynamicGridDCrossStrategy {
    windows: HashMap<usize, DCrossWindowState>,
}

impl DynamicGridDCrossStrategy {
    pub fn new() -> Self {
        Self {
            windows: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum DCrossAtrRegime {
    Calm,
    Normal,
    Volatile,
    Storm,
}

impl DCrossAtrRegime {
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

    fn grid_deltas(self) -> (f64, f64, f64) {
        match self {
            Self::Calm => (0.06, 0.10, 0.14),
            Self::Normal => (0.08, 0.12, 0.18),
            Self::Volatile => (0.10, 0.15, 0.22),
            Self::Storm => (0.12, 0.18, 0.26),
        }
    }
}

fn midpoint(bid: f64, ask: f64) -> f64 {
    (bid + ask) / 2.0
}

fn side_mid(side: &str, prices: &PricesState) -> f64 {
    if side == "UP" {
        midpoint(prices.up.bid, prices.up.ask)
    } else {
        midpoint(prices.down.bid, prices.down.ask)
    }
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

fn shares_for_side(side: &str, win_state: &WindowState) -> f64 {
    if side == "UP" {
        win_state.up_shares
    } else {
        win_state.down_shares
    }
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

fn time_pct_for(market: &MarketWindow, secs_to_end: i64) -> f64 {
    let duration_sec = market_duration_sec(market);
    let elapsed_sec = (duration_sec - secs_to_end as f64).clamp(0.0, duration_sec);
    (elapsed_sec / duration_sec) * 100.0
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
    current_atr.max(DCROSS_MIN_VALID_ATR) * ((secs_left as f64).max(1.0) / 60.0).sqrt()
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
    let secs_left_f = (secs_left as f64).max(1.0);
    let expected_move = expected_move_usd(current_atr, secs_left);
    let max_velocity_drift = expected_move * 0.35;
    let velocity_drift = (spot_velocity(spot_signal).unwrap_or(0.0)
        * secs_left_f
        * DCROSS_VELOCITY_DRIFT_FRACTION)
        .clamp(-max_velocity_drift, max_velocity_drift);
    normal_cdf(((spot - ptb) + velocity_drift) / expected_move)
}

fn side_fair_probability(side: &str, fair_up: f64) -> f64 {
    if side == "UP" {
        fair_up
    } else {
        1.0 - fair_up
    }
}

fn has_counter_velocity(side: &str, spot_signal: SpotSignalSnapshot) -> bool {
    let side_sign = if side == "UP" { 1.0 } else { -1.0 };
    spot_signal
        .smoothed_velocity_usd_per_sec
        .or(spot_signal.raw_velocity_usd_per_sec)
        .map(|velocity| velocity * side_sign < -DCROSS_MIN_VELOCITY_CONFIRM_USD_PER_SEC)
        .unwrap_or(false)
}

fn cheap_side(prices: &PricesState) -> Option<&'static str> {
    let up_mid = side_mid("UP", prices);
    let down_mid = side_mid("DOWN", prices);
    if up_mid < 0.5 && down_mid >= 0.5 {
        Some("UP")
    } else if down_mid < 0.5 && up_mid >= 0.5 {
        Some("DOWN")
    } else if up_mid < down_mid {
        Some("UP")
    } else if down_mid < up_mid {
        Some("DOWN")
    } else {
        None
    }
}

fn expensive_side(cheap: &str) -> &'static str {
    if cheap == "UP" {
        "DOWN"
    } else {
        "UP"
    }
}

fn mid_cross_context_ok(mid_cross: &MidCrossSnapshot) -> bool {
    mid_cross.cross_count <= DCROSS_MAX_CHAOTIC_CROSSES || mid_cross.last_cross_is_significant
}

fn redeem_hold_blocks_sell(
    side: &str,
    spot_price: Option<f64>,
    market: &MarketWindow,
    secs_to_end: i64,
    time_pct: f64,
    current_atr: f64,
    bid: f64,
    fair_prob: f64,
    spot_signal: SpotSignalSnapshot,
    cex_micro: &CexMicroSnapshot,
) -> bool {
    let (Some(spot), Some(ptb)) = (spot_price, market.price_to_beat) else {
        return false;
    };
    let side_sign = if side == "UP" { 1.0 } else { -1.0 };
    let counter_velocity = spot_signal
        .smoothed_velocity_usd_per_sec
        .or(spot_signal.raw_velocity_usd_per_sec)
        .map(|v| v * side_sign < -DCROSS_MIN_VELOCITY_CONFIRM_USD_PER_SEC)
        .unwrap_or(false);
    evaluate_redeem_hold(&RedeemHoldInput {
        side,
        spot,
        ptb,
        secs_to_end,
        time_pct,
        current_atr,
        bid,
        fair_prob,
        ptb_crossed: false,
        counter_velocity_against: counter_velocity,
        cex_velocity_against: crate::cex_micro::cex_velocity_against_side(side, cex_micro),
    })
    .should_hold
}

fn grid_target(entry_ask: f64, delta: f64, time_pct: f64) -> f64 {
    let late_trim = if time_pct > DCROSS_LATE_TIME_PCT {
        0.02
    } else {
        0.0
    };
    (entry_ask + delta - late_trim).clamp(0.05, 0.99)
}

fn thesis_lock_conditions(
    entry_side: &str,
    spot: Option<f64>,
    ptb: Option<f64>,
    mid_cross: &MidCrossSnapshot,
) -> bool {
    let (Some(spot), Some(ptb)) = (spot, ptb) else {
        return false;
    };
    if !side_is_itm(entry_side, spot, ptb) {
        return false;
    }
    let leader = match mid_cross.current_side {
        Some(LeadSide::Up) => "UP",
        Some(LeadSide::Down) => "DOWN",
        Some(LeadSide::Tie) | None => return false,
    };
    leader == entry_side && mid_cross.lead_gap >= DCROSS_THESIS_LOCK_MIN_GAP
}

fn grid_step_sell_allowed(thesis_locked: bool, step: usize) -> bool {
    if !thesis_locked {
        return true;
    }
    step == 2
}

fn edge_allows_grid_sell(step: usize, bid: f64, fair_side: f64, target: f64) -> bool {
    let edge = bid - fair_side;
    match step {
        0 => edge >= DCROSS_EDGE_STEP1 || bid >= target,
        1 | 2 => edge >= DCROSS_EDGE_STEP2 && bid >= target,
        _ => false,
    }
}

fn cap_sell_with_runner_floor(
    sell_shares: f64,
    total_shares: f64,
    initial_entry_shares: f64,
    time_pct: f64,
    thesis_locked: bool,
) -> f64 {
    if thesis_locked || time_pct >= DCROSS_LATE_TIME_PCT {
        return sell_shares;
    }
    let runner_floor = initial_entry_shares * DCROSS_RUNNER_FRACTION;
    sell_shares.min((total_shares - runner_floor).max(0.0))
}

impl TradeStrategy for DynamicGridDCrossStrategy {
    fn check_pre_start_entry(
        &mut self,
        _config: &Config,
        _prices: &PricesState,
        _market: &MarketWindow,
        _spot_price: Option<f64>,
        _window_number: usize,
        _secs_to_start: i64,
        _current_btc_atr: f64,
        _spot_signal: SpotSignalSnapshot,
        _llm_forecast: Option<crate::strategy::LlmForecast>,
    ) -> Option<EntrySignal> {
        None
    }

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
        mid_cross: &MidCrossSnapshot,
        cex_micro: &CexMicroSnapshot,
    ) -> Vec<OrderSignal> {
        let mut signals = Vec::new();
        let time_pct = time_pct_for(market, secs_to_end);
        let window_number = win_state.window_number;
        let state = self
            .windows
            .entry(window_number)
            .or_insert(DCrossWindowState {
                entry_side: None,
                entry_ask: 0.0,
                pending_entry_ask: 0.0,
                initial_entry_shares: 0.0,
                sell_steps_done: 0,
                thesis_locked: false,
                emergency_sold: false,
            });

        if state.entry_side.is_none() {
            if win_state.up_shares > DCROSS_SELL_MIN_SHARES {
                state.entry_side = Some("UP".to_string());
                if state.entry_ask <= 0.0 {
                    state.entry_ask = if state.pending_entry_ask > 0.0 {
                        state.pending_entry_ask
                    } else {
                        side_ask("UP", prices)
                    };
                }
            } else if win_state.down_shares > DCROSS_SELL_MIN_SHARES {
                state.entry_side = Some("DOWN".to_string());
                if state.entry_ask <= 0.0 {
                    state.entry_ask = if state.pending_entry_ask > 0.0 {
                        state.pending_entry_ask
                    } else {
                        side_ask("DOWN", prices)
                    };
                }
            }
        }

        if time_pct >= DCROSS_FINAL_TIME_PCT || secs_to_end <= DCROSS_FINAL_SECS_TO_END {
            if !state.emergency_sold {
                let fair_up = fair_probability_up(
                    market,
                    spot_price,
                    current_atr,
                    secs_to_end,
                    spot_signal,
                );
                for side in ["UP", "DOWN"] {
                    let shares = shares_for_side(side, win_state);
                    let bid = side_bid(side, prices);
                    if shares <= DCROSS_SELL_MIN_SHARES || bid < 0.05 {
                        continue;
                    }
                    let fair_prob = side_fair_probability(side, fair_up);
                    if redeem_hold_blocks_sell(
                        side,
                        spot_price,
                        market,
                        secs_to_end,
                        time_pct,
                        current_atr,
                        bid,
                        fair_prob,
                        spot_signal,
                        cex_micro,
                    ) {
                        continue;
                    }
                    let (Some(spot), Some(ptb)) = (spot_price, market.price_to_beat) else {
                        signals.push(OrderSignal {
                            side: side.to_string(),
                            is_buy: false,
                            amount: shares,
                            price: bid,
                            reason: "dcross_emergency_time_stop_otm".to_string(),
                        });
                        continue;
                    };
                    let is_itm = if side == "UP" {
                        spot > ptb
                    } else {
                        spot < ptb
                    };
                    if is_itm && fair_prob >= 0.35 {
                        continue;
                    }
                    if !is_itm
                        && bid < DCROSS_EMERGENCY_MIN_BID
                        && fair_prob < DCROSS_EMERGENCY_MIN_FAIR
                    {
                        continue;
                    }
                    let reason = if is_itm {
                        "dcross_emergency_time_stop_itm".to_string()
                    } else if bid >= DCROSS_EMERGENCY_MIN_BID {
                        "dcross_emergency_otm_salvage".to_string()
                    } else {
                        "dcross_emergency_time_stop_otm".to_string()
                    };
                    signals.push(OrderSignal {
                        side: side.to_string(),
                        is_buy: false,
                        amount: shares,
                        price: bid,
                        reason,
                    });
                }
                state.emergency_sold = true;
            }
            return signals;
        }

        let entry_side = match &state.entry_side {
            Some(side) => side.clone(),
            None => {
                if win_state.spent > 0.0 {
                    return signals;
                }
                if !mid_cross.armed || time_pct < MID_CROSS_ARM_TIME_PCT {
                    return signals;
                }
                if time_pct > DCROSS_MAX_ENTRY_TIME_PCT || secs_to_end < DCROSS_MIN_SECS_TO_END {
                    return signals;
                }
                if !mid_cross_context_ok(mid_cross) {
                    return signals;
                }

                let cheap = match cheap_side(prices) {
                    Some(side) => side,
                    None => return signals,
                };
                let expensive = expensive_side(cheap);
                let cheap_ask = side_ask(cheap, prices);
                let expensive_mid = side_mid(expensive, prices);
                if cheap_ask <= 0.0 || cheap_ask > DCROSS_MAX_CHEAP_ASK {
                    return signals;
                }
                if expensive_mid > DCROSS_MAX_EXPENSIVE_MID {
                    return signals;
                }

                let (Some(spot), Some(ptb)) = (spot_price, market.price_to_beat) else {
                    return signals;
                };
                let expected_move = expected_move_usd(current_atr, secs_to_end);
                let gap_z = (spot - ptb) / expected_move;
                if !gap_z.is_finite() || gap_z.abs() > DCROSS_MAX_ABS_GAP_Z {
                    return signals;
                }
                if has_counter_velocity(cheap, spot_signal) {
                    return signals;
                }
                if cex_vetoes_cheap_entry(cheap, cex_micro) {
                    return signals;
                }

                let fair_up = fair_probability_up(
                    market,
                    spot_price,
                    current_atr,
                    secs_to_end,
                    spot_signal,
                );
                let fair_side = side_fair_probability(cheap, fair_up);
                let entry_edge = fair_side - cheap_ask;
                if entry_edge < DCROSS_MIN_ENTRY_EDGE {
                    return signals;
                }

                let budget = (config.session.min_window_budget * DCROSS_ENTRY_BUDGET_MULTIPLIER)
                    .clamp(DCROSS_MIN_TRADE_USD, config.session.max_window_budget);
                if budget < DCROSS_MIN_TRADE_USD {
                    return signals;
                }

                state.pending_entry_ask = cheap_ask;

                signals.push(OrderSignal {
                    side: cheap.to_string(),
                    is_buy: true,
                    amount: budget,
                    price: cheap_ask,
                    reason: format!(
                        "dcross_cheap_entry_{}_ask_{:.2}_fair_{:.3}_edge_{:+.3}_gap_z_{:+.2}_crosses_{}_sig_{}_atr_{:.1}",
                        cheap.to_lowercase(),
                        cheap_ask,
                        fair_side,
                        entry_edge,
                        gap_z,
                        mid_cross.cross_count,
                        mid_cross.significant_cross_count,
                        current_atr
                    ),
                });
                return signals;
            }
        };

        let shares = shares_for_side(&entry_side, win_state);
        if shares <= DCROSS_SELL_MIN_SHARES {
            return signals;
        }

        if state.initial_entry_shares <= DCROSS_SELL_MIN_SHARES {
            state.initial_entry_shares = shares;
        }

        let fair_up = fair_probability_up(
            market,
            spot_price,
            current_atr,
            secs_to_end,
            spot_signal,
        );
        let fair_side = side_fair_probability(&entry_side, fair_up);
        let bid = side_bid(&entry_side, prices);
        if redeem_hold_blocks_sell(
            &entry_side,
            spot_price,
            market,
            secs_to_end,
            time_pct,
            current_atr,
            bid,
            fair_side,
            spot_signal,
            cex_micro,
        ) {
            return signals;
        }

        if state.thesis_locked {
            if let (Some(spot), Some(ptb)) = (spot_price, market.price_to_beat) {
                if !side_is_itm(&entry_side, spot, ptb) {
                    state.thesis_locked = false;
                }
            }
        }
        if !state.thesis_locked
            && thesis_lock_conditions(&entry_side, spot_price, market.price_to_beat, mid_cross)
        {
            state.thesis_locked = true;
        }

        let atr_regime = DCrossAtrRegime::from_atr(current_atr);
        let (d1, d2, d3) = atr_regime.grid_deltas();
        let targets = [
            grid_target(state.entry_ask, d1, time_pct),
            grid_target(state.entry_ask, d2, time_pct),
            grid_target(state.entry_ask, d3, time_pct),
        ];
        let fractions = [0.35, 0.35, 0.30];

        if state.sell_steps_done < targets.len() {
            let step = state.sell_steps_done;
            if bid >= targets[step]
                && grid_step_sell_allowed(state.thesis_locked, step)
                && edge_allows_grid_sell(step, bid, fair_side, targets[step])
            {
                let mut sell_shares = (shares * fractions[step]).max(DCROSS_SELL_MIN_SHARES);
                sell_shares = cap_sell_with_runner_floor(
                    sell_shares,
                    shares,
                    state.initial_entry_shares,
                    time_pct,
                    state.thesis_locked,
                );
                if sell_shares >= DCROSS_SELL_MIN_SHARES && sell_shares <= shares {
                    let lock_tag = if state.thesis_locked {
                        "_thesis_lock"
                    } else {
                        ""
                    };
                    signals.push(OrderSignal {
                        side: entry_side.clone(),
                        is_buy: false,
                        amount: sell_shares,
                        price: bid,
                        reason: format!(
                            "dcross_grid_step{}_target_{:.2}_bid_{:.2}_atr_{}{}",
                            step + 1,
                            targets[step],
                            bid,
                            match atr_regime {
                                DCrossAtrRegime::Calm => "calm",
                                DCrossAtrRegime::Normal => "normal",
                                DCrossAtrRegime::Volatile => "volatile",
                                DCrossAtrRegime::Storm => "storm",
                            },
                            lock_tag
                        ),
                    });
                    state.sell_steps_done += 1;
                }
            }
        }

        signals
    }

    fn get_strategy_state(&self, _window_number: usize) -> Option<StrategyState> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mid_cross_tracker::{LeadSide, MidCrossSnapshot};

    fn sample_mid_cross(side: LeadSide, gap: f64) -> MidCrossSnapshot {
        MidCrossSnapshot {
            armed: true,
            current_side: Some(side),
            lead_gap: gap,
            up_mid: 0.55,
            down_mid: 0.45,
            cross_count: 1,
            significant_cross_count: 1,
            peak_lead_gap: gap,
            last_cross_from: None,
            last_cross_to: Some(side),
            last_cross_time_pct: Some(20.0),
            last_cross_is_significant: true,
            last_cross_atr: 40.0,
        }
    }

    #[test]
    fn thesis_lock_requires_spot_itm_and_mid_gap() {
        let mid = sample_mid_cross(LeadSide::Up, 0.16);
        assert!(thesis_lock_conditions("UP", Some(61_200.0), Some(61_198.0), &mid));
        assert!(!thesis_lock_conditions(
            "UP",
            Some(61_190.0),
            Some(61_198.0),
            &mid
        ));
        assert!(!thesis_lock_conditions("DOWN", Some(61_200.0), Some(61_198.0), &mid));
    }

    #[test]
    fn thesis_lock_blocks_grid_steps_except_step3() {
        assert!(grid_step_sell_allowed(false, 0));
        assert!(grid_step_sell_allowed(false, 1));
        assert!(!grid_step_sell_allowed(true, 0));
        assert!(!grid_step_sell_allowed(true, 1));
        assert!(grid_step_sell_allowed(true, 2));
    }

    #[test]
    fn edge_gate_blocks_thin_flip_bid() {
        assert!(!edge_allows_grid_sell(1, 0.58, 0.60, 0.58));
        assert!(edge_allows_grid_sell(1, 0.65, 0.58, 0.58));
        assert!(edge_allows_grid_sell(0, 0.56, 0.52, 0.56));
    }

    #[test]
    fn runner_floor_keeps_tail_before_late_phase() {
        let capped = cap_sell_with_runner_floor(40.0, 55.0, 55.0, 50.0, false);
        assert!((capped - 35.75).abs() < 0.01);
        let uncapped = cap_sell_with_runner_floor(40.0, 55.0, 55.0, 50.0, true);
        assert!((uncapped - 40.0).abs() < 0.01);
    }
}
