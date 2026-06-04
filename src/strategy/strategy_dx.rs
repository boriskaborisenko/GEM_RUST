use crate::client::{get_now_ms, MarketWindow, PricesState};
use crate::config::Config;
use crate::strategy::{EntrySignal, OrderSignal, SpotSignalSnapshot, StrategyState, TradeStrategy};
use crate::trader::WindowState;
use std::collections::{HashMap, VecDeque};

const DX_MIN_VALID_ATR: f64 = 1.0;
const DX_ENTRY_BUDGET_MULTIPLIER: f64 = 0.30;
const DX_ENTRY_VOLATILE_MULTIPLIER: f64 = 0.22;
const DX_ENTRY_STORM_MULTIPLIER: f64 = 0.14;
const DX_MIN_TRADE_USD: f64 = 1.0;
const DX_MAX_ENTRY_ASK: f64 = 0.68;
const DX_RICH_ENTRY_ASK: f64 = 0.58;
const DX_MAX_ENTRY_SPREAD: f64 = 0.08;
const DX_LATE_MAX_ENTRY_SPREAD: f64 = 0.06;
const DX_MAX_TIME_PCT: f64 = 85.0;
const DX_MAIN_MAX_TIME_PCT: f64 = 70.0;
const DX_MIN_ENTRY_EDGE: f64 = 0.03;
const DX_MID_MIN_ENTRY_EDGE: f64 = 0.04;
const DX_LATE_MIN_ENTRY_EDGE: f64 = 0.07;
const DX_MIN_FAIR_PROB: f64 = 0.55;
const DX_MID_MIN_FAIR_PROB: f64 = 0.58;
const DX_LATE_MIN_FAIR_PROB: f64 = 0.64;
const DX_MIN_ABS_GAP_Z: f64 = 0.45;
const DX_RICH_MIN_ABS_GAP_Z: f64 = 0.70;
const DX_LATE_MIN_ABS_GAP_Z: f64 = 0.85;
const DX_VELOCITY_DRIFT_FRACTION: f64 = 0.35;
const DX_SPOT_CONFIRM_USD_PER_SEC: f64 = 0.10;
const DX_SPOT_ACCEL_CONFIRM_USD_PER_SEC2: f64 = 0.01;
const DX_PROB_VELOCITY_30S_CONFIRM: f64 = 0.010;
const DX_PROB_VELOCITY_60S_CONFIRM: f64 = 0.015;
const DX_PROB_VELOCITY_30S_BLOCK: f64 = 0.018;
const DX_PROB_VELOCITY_60S_BLOCK: f64 = 0.025;
const DX_ENTRY_NO_MICRO_EXTRA_EDGE: f64 = 0.02;
const DX_HEDGE_MAX_FRACTION_OF_SPENT: f64 = 0.35;
const DX_HEDGE_STEP_FRACTION_OF_SPENT: f64 = 0.12;
const DX_HEDGE_MIN_EDGE: f64 = 0.035;
const DX_HEDGE_CHEAP_ASK: f64 = 0.40;
const DX_HEDGE_MAX_PAIR_COST: f64 = 1.05;
const DX_HEDGE_SEVERE_MAX_PAIR_COST: f64 = 1.10;
const DX_HEDGE_BASE_TARGET_RATIO: f64 = 0.18;
const DX_HEDGE_ADVERSE_TARGET_RATIO: f64 = 0.35;
const DX_HEDGE_SEVERE_TARGET_RATIO: f64 = 0.55;
const DX_HEDGE_PTB_TARGET: f64 = 0.055;
const DX_PRIMARY_PTB_STEP1: f64 = 0.060;
const DX_PRIMARY_PTB_STEP2: f64 = 0.125;
const DX_PRIMARY_PTB_STEP3: f64 = 0.220;
const DX_PRIMARY_SELL_EDGE: f64 = 0.015;
const DX_PRIMARY_HOLD_REDEEM_SECONDS: i64 = 55;
const DX_FINAL_TIME_PCT: f64 = 90.0;
const DX_FINAL_MIN_BID: f64 = 0.05;
const DX_FINAL_SELL_EDGE: f64 = 0.02;
const DX_SELL_MIN_SHARES: f64 = 0.000001;

#[derive(Debug, Clone, Copy)]
struct DxProbTick {
    timestamp_ms: i64,
    yes_mid: f64,
}

#[derive(Debug, Clone, Copy, Default)]
struct DxProbabilityVelocity {
    velocity_30s: Option<f64>,
    velocity_60s: Option<f64>,
}

#[derive(Debug, Clone, Copy)]
enum DxPhase {
    Observe,
    Entry,
    Manage,
    Late,
    Final,
}

impl DxPhase {
    fn from_time_pct(time_pct: f64) -> Self {
        if time_pct < 10.0 {
            Self::Observe
        } else if time_pct < 47.0 {
            Self::Entry
        } else if time_pct < 76.0 {
            Self::Manage
        } else if time_pct < DX_FINAL_TIME_PCT {
            Self::Late
        } else {
            Self::Final
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Observe => "observe",
            Self::Entry => "entry",
            Self::Manage => "manage",
            Self::Late => "late",
            Self::Final => "final",
        }
    }

    fn allows_new_entry(self) -> bool {
        matches!(self, Self::Entry | Self::Manage | Self::Late)
    }
}

#[derive(Debug, Clone, Copy)]
enum DxAtrRegime {
    Calm,
    Normal,
    Volatile,
    Storm,
}

impl DxAtrRegime {
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

pub struct DynamicGridDxStrategy {
    pub states: HashMap<usize, StrategyState>,
    pub primary_side: HashMap<usize, String>,
    pub primary_entry_price: HashMap<usize, f64>,
    pub sell_steps: HashMap<(usize, String), usize>,
    probability_ticks: HashMap<usize, VecDeque<DxProbTick>>,
}

impl DynamicGridDxStrategy {
    pub fn new() -> Self {
        Self {
            states: HashMap::new(),
            primary_side: HashMap::new(),
            primary_entry_price: HashMap::new(),
            sell_steps: HashMap::new(),
            probability_ticks: HashMap::new(),
        }
    }

    fn update_probability_velocity(
        &mut self,
        window_number: usize,
        prices: &PricesState,
    ) -> DxProbabilityVelocity {
        let yes_mid = midpoint(prices.up.bid, prices.up.ask);
        if yes_mid <= 0.0 {
            return DxProbabilityVelocity::default();
        }

        let now = get_now_ms();
        let ticks = self
            .probability_ticks
            .entry(window_number)
            .or_insert_with(VecDeque::new);
        ticks.push_back(DxProbTick {
            timestamp_ms: now,
            yes_mid,
        });
        while ticks
            .front()
            .map(|tick| now - tick.timestamp_ms > 95_000)
            .unwrap_or(false)
        {
            ticks.pop_front();
        }

        DxProbabilityVelocity {
            velocity_30s: probability_velocity_near(ticks, now, 30_000, yes_mid),
            velocity_60s: probability_velocity_near(ticks, now, 60_000, yes_mid),
        }
    }
}

fn midpoint(bid: f64, ask: f64) -> f64 {
    if bid > 0.0 && ask > 0.0 {
        (bid + ask) / 2.0
    } else {
        0.0
    }
}

fn probability_velocity_near(
    ticks: &VecDeque<DxProbTick>,
    now_ms: i64,
    lookback_ms: i64,
    current_mid: f64,
) -> Option<f64> {
    let target = now_ms - lookback_ms;
    let mut best: Option<DxProbTick> = None;
    let mut best_diff = i64::MAX;

    for tick in ticks {
        let diff = (tick.timestamp_ms - target).abs();
        if diff < best_diff {
            best_diff = diff;
            best = Some(*tick);
        }
    }

    let prior = best?;
    if best_diff > 15_000 || prior.yes_mid <= 0.0 {
        return None;
    }
    Some((current_mid - prior.yes_mid) / prior.yes_mid)
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
    current_atr.max(DX_MIN_VALID_ATR) * ((secs_left as f64).max(1.0) / 60.0).sqrt()
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
    let velocity_drift =
        spot_velocity(spot_signal).unwrap_or(0.0) * secs_left_f * DX_VELOCITY_DRIFT_FRACTION;
    normal_cdf(((spot - ptb) + velocity_drift) / expected_move_usd(current_atr, secs_left))
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

fn duration_elapsed_and_time_pct(market: &MarketWindow, secs_to_end: i64) -> (f64, f64, f64) {
    let duration_sec = market_duration_sec(market);
    let elapsed_sec = (duration_sec - secs_to_end as f64).clamp(0.0, duration_sec);
    (
        duration_sec,
        elapsed_sec,
        (elapsed_sec / duration_sec) * 100.0,
    )
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

fn side_spread(side: &str, prices: &PricesState) -> f64 {
    (side_ask(side, prices) - side_bid(side, prices)).max(0.0)
}

fn side_shares(side: &str, win_state: &WindowState) -> f64 {
    if side == "UP" {
        win_state.up_shares
    } else {
        win_state.down_shares
    }
}

fn side_fair_probability(side: &str, fair_up: f64) -> f64 {
    if side == "UP" {
        fair_up
    } else {
        1.0 - fair_up
    }
}

fn opposite_side(side: &str) -> &'static str {
    if side == "UP" {
        "DOWN"
    } else {
        "UP"
    }
}

fn side_buy_cost_shares(side: &str, win_state: &WindowState) -> (f64, f64) {
    win_state
        .trades
        .iter()
        .filter(|trade| trade.trade_type == "BUY" && trade.side == side)
        .fold((0.0, 0.0), |(cost, shares), trade| {
            (cost + trade.usd_value, shares + trade.shares)
        })
}

fn side_avg_entry(side: &str, win_state: &WindowState) -> Option<f64> {
    let (cost, shares) = side_buy_cost_shares(side, win_state);
    if shares > 0.0 {
        Some(cost / shares)
    } else {
        None
    }
}

fn infer_primary_leg(win_state: &WindowState) -> Option<(String, f64)> {
    let first_buy = win_state
        .trades
        .iter()
        .find(|trade| trade.trade_type == "BUY")?;
    Some((first_buy.side.clone(), first_buy.price))
}

fn relation_for_spot(market: &MarketWindow, spot_price: Option<f64>) -> Option<&'static str> {
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
    market: &MarketWindow,
    spot_price: Option<f64>,
) -> bool {
    let Some(relation) = relation_for_spot(market, spot_price) else {
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

fn spot_entry_confirmations(side: &str, spot_signal: SpotSignalSnapshot) -> usize {
    let side_sign = if side == "UP" { 1.0 } else { -1.0 };
    let mut count = 0;
    if spot_signal
        .raw_velocity_usd_per_sec
        .map(|velocity| velocity * side_sign >= DX_SPOT_CONFIRM_USD_PER_SEC)
        .unwrap_or(false)
    {
        count += 1;
    }
    if spot_signal
        .smoothed_velocity_usd_per_sec
        .map(|velocity| velocity * side_sign >= DX_SPOT_CONFIRM_USD_PER_SEC)
        .unwrap_or(false)
    {
        count += 1;
    }
    if spot_signal
        .acceleration_usd_per_sec2
        .map(|accel| accel * side_sign >= DX_SPOT_ACCEL_CONFIRM_USD_PER_SEC2)
        .unwrap_or(false)
    {
        count += 1;
    }
    count
}

fn probability_velocity_confirms(side: &str, velocity: DxProbabilityVelocity) -> bool {
    let side_sign = if side == "UP" { 1.0 } else { -1.0 };
    velocity
        .velocity_30s
        .map(|v| v * side_sign >= DX_PROB_VELOCITY_30S_CONFIRM)
        .unwrap_or(false)
        || velocity
            .velocity_60s
            .map(|v| v * side_sign >= DX_PROB_VELOCITY_60S_CONFIRM)
            .unwrap_or(false)
}

fn probability_velocity_blocks(side: &str, velocity: DxProbabilityVelocity) -> bool {
    let side_sign = if side == "UP" { 1.0 } else { -1.0 };
    velocity
        .velocity_30s
        .map(|v| v * side_sign <= -DX_PROB_VELOCITY_30S_BLOCK)
        .unwrap_or(false)
        || velocity
            .velocity_60s
            .map(|v| v * side_sign <= -DX_PROB_VELOCITY_60S_BLOCK)
            .unwrap_or(false)
}

fn has_counter_spot_velocity(side: &str, spot_signal: SpotSignalSnapshot) -> bool {
    let side_sign = if side == "UP" { 1.0 } else { -1.0 };
    spot_signal
        .smoothed_velocity_usd_per_sec
        .or(spot_signal.raw_velocity_usd_per_sec)
        .map(|velocity| velocity * side_sign < -DX_SPOT_CONFIRM_USD_PER_SEC)
        .unwrap_or(false)
}

fn entry_thresholds(phase: DxPhase, time_pct: f64) -> (f64, f64, f64) {
    if matches!(phase, DxPhase::Late) || time_pct > DX_MAIN_MAX_TIME_PCT {
        (
            DX_LATE_MIN_ENTRY_EDGE,
            DX_LATE_MIN_FAIR_PROB,
            DX_LATE_MIN_ABS_GAP_Z,
        )
    } else if matches!(phase, DxPhase::Manage) {
        (
            DX_MID_MIN_ENTRY_EDGE,
            DX_MID_MIN_FAIR_PROB,
            DX_MIN_ABS_GAP_Z,
        )
    } else {
        (DX_MIN_ENTRY_EDGE, DX_MIN_FAIR_PROB, DX_MIN_ABS_GAP_Z)
    }
}

fn entry_budget(config: &Config, atr_regime: DxAtrRegime) -> f64 {
    let multiplier = match atr_regime {
        DxAtrRegime::Calm | DxAtrRegime::Normal => DX_ENTRY_BUDGET_MULTIPLIER,
        DxAtrRegime::Volatile => DX_ENTRY_VOLATILE_MULTIPLIER,
        DxAtrRegime::Storm => DX_ENTRY_STORM_MULTIPLIER,
    };
    (config.session.min_window_budget * multiplier)
        .clamp(DX_MIN_TRADE_USD, config.session.max_window_budget)
}

fn primary_ptb_target(step: usize, phase: DxPhase) -> f64 {
    let base = match step {
        0 => DX_PRIMARY_PTB_STEP1,
        1 => DX_PRIMARY_PTB_STEP2,
        _ => DX_PRIMARY_PTB_STEP3,
    };
    if matches!(phase, DxPhase::Late | DxPhase::Final) {
        (base * 0.80).max(0.04)
    } else {
        base
    }
}

impl TradeStrategy for DynamicGridDxStrategy {
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
        // Dx trades the active/current window only. NEXT/future windows are
        // observed and prepared by the runtime, but never bought pre-start.
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
    ) -> Vec<OrderSignal> {
        let mut signals = Vec::new();
        let window_number = win_state.window_number;
        let (_, _, time_pct) = duration_elapsed_and_time_pct(market, secs_to_end);
        let phase = DxPhase::from_time_pct(time_pct);
        let atr_regime = DxAtrRegime::from_atr(current_atr);
        let prob_velocity = self.update_probability_velocity(window_number, prices);
        let fair_up =
            fair_probability_up(market, spot_price, current_atr, secs_to_end, spot_signal);
        let fair_down = 1.0 - fair_up;

        let state = self.states.entry(window_number).or_insert(StrategyState {
            up_sold: false,
            down_sold: false,
            first_sold_side: None,
            ptb_crossed: false,
            ptb_baseline: None,
        });
        let cross_happened_now = update_ptb_cross_state(state, market, spot_price);
        let ptb_crossed = state.ptb_crossed;
        let ptb_baseline = state
            .ptb_baseline
            .clone()
            .unwrap_or_else(|| "NA".to_string());
        let relation = relation_for_spot(market, spot_price).unwrap_or("NA");

        if current_atr < config.min_btc_atr || current_atr < DX_MIN_VALID_ATR {
            return signals;
        }

        // ENTRY: current active window only.
        if win_state.spent <= 0.0 && win_state.status == "SKIPPED" {
            if !phase.allows_new_entry() || time_pct > DX_MAX_TIME_PCT {
                return signals;
            }

            let (Some(spot), Some(ptb)) = (spot_price, market.price_to_beat) else {
                return signals;
            };
            let expected_move = expected_move_usd(current_atr, secs_to_end);
            let gap_z = (spot - ptb) / expected_move;
            if !gap_z.is_finite() {
                return signals;
            }

            let d1_side = if gap_z >= 0.0 { "UP" } else { "DOWN" };
            let up_edge = fair_up - prices.up.ask;
            let down_edge = fair_down - prices.down.ask;
            let fair_side = if up_edge >= down_edge { "UP" } else { "DOWN" };
            if d1_side != fair_side {
                return signals;
            }

            let ask = side_ask(d1_side, prices);
            let bid = side_bid(d1_side, prices);
            let spread = side_spread(d1_side, prices);
            let fair_side_prob = side_fair_probability(d1_side, fair_up);
            let entry_edge = fair_side_prob - ask;
            let (mut min_edge, min_prob, min_abs_gap_z) = entry_thresholds(phase, time_pct);
            let spot_confirmations = spot_entry_confirmations(d1_side, spot_signal);
            let prob_confirms = probability_velocity_confirms(d1_side, prob_velocity);
            let has_micro = spot_confirmations > 0 || prob_confirms;
            if !has_micro {
                min_edge += DX_ENTRY_NO_MICRO_EXTRA_EDGE;
            }

            if ask <= 0.0
                || bid <= 0.0
                || ask > DX_MAX_ENTRY_ASK
                || spread
                    > if matches!(phase, DxPhase::Late) {
                        DX_LATE_MAX_ENTRY_SPREAD
                    } else {
                        DX_MAX_ENTRY_SPREAD
                    }
                || entry_edge < min_edge
                || fair_side_prob < min_prob
                || gap_z.abs() < min_abs_gap_z
                || has_counter_spot_velocity(d1_side, spot_signal)
                || probability_velocity_blocks(d1_side, prob_velocity)
            {
                return signals;
            }

            if ask > DX_RICH_ENTRY_ASK && (gap_z.abs() < DX_RICH_MIN_ABS_GAP_Z || !has_micro) {
                return signals;
            }

            let budget = entry_budget(config, atr_regime);
            if budget < DX_MIN_TRADE_USD {
                return signals;
            }

            self.primary_side.insert(window_number, d1_side.to_string());
            self.primary_entry_price.insert(window_number, ask);

            signals.push(OrderSignal {
                side: d1_side.to_string(),
                is_buy: true,
                amount: budget,
                price: ask,
                reason: format!(
                    "dx_live_entry_{}_phase_{}_atr_{}_ask_{:.2}_bid_{:.2}_spread_{:.3}_fair_{:.3}_edge_{:+.3}_gap_z_{:+.2}_time_{:.1}_spot_conf_{}_prob30_{}_prob60_{}_budget_{:.2}",
                    d1_side.to_lowercase(),
                    phase.as_str(),
                    atr_regime.as_str(),
                    ask,
                    bid,
                    spread,
                    fair_side_prob,
                    entry_edge,
                    gap_z,
                    time_pct,
                    spot_confirmations,
                    fmt_opt(prob_velocity.velocity_30s),
                    fmt_opt(prob_velocity.velocity_60s),
                    budget
                ),
            });
            return signals;
        }

        let (inferred_primary, inferred_price) = match infer_primary_leg(win_state) {
            Some(primary) => primary,
            None => return signals,
        };
        let primary_side = self
            .primary_side
            .entry(window_number)
            .or_insert(inferred_primary)
            .clone();
        let primary_entry_price = *self
            .primary_entry_price
            .entry(window_number)
            .or_insert(inferred_price);
        let hedge_side = opposite_side(&primary_side);
        let primary_shares = side_shares(&primary_side, win_state);
        let hedge_shares = side_shares(hedge_side, win_state);
        let primary_prob = side_fair_probability(&primary_side, fair_up);
        let hedge_prob = side_fair_probability(hedge_side, fair_up);
        let primary_bid = side_bid(&primary_side, prices);
        let hedge_bid = side_bid(hedge_side, prices);
        let hedge_ask = side_ask(hedge_side, prices);

        // HEDGE: buy only when the opposite side is useful and not rich.
        if primary_shares > DX_SELL_MIN_SHARES
            && hedge_ask > 0.0
            && hedge_ask <= DX_HEDGE_CHEAP_ASK
            && time_pct < DX_FINAL_TIME_PCT
        {
            let current_hedge_ratio = (hedge_shares / primary_shares).clamp(0.0, 1.0);
            let hedge_edge = hedge_prob - hedge_ask;
            let pair_cost = primary_entry_price + hedge_ask;
            let primary_adverse_after_cross =
                ptb_crossed && !relation_is_favorable_for_side(&primary_side, relation);
            let severe_primary_wrong = primary_prob <= 0.38
                && primary_adverse_after_cross
                && pair_cost <= DX_HEDGE_SEVERE_MAX_PAIR_COST;
            let regular_hedge = hedge_edge >= DX_HEDGE_MIN_EDGE
                && pair_cost <= DX_HEDGE_MAX_PAIR_COST
                && (primary_adverse_after_cross
                    || has_counter_spot_velocity(&primary_side, spot_signal)
                    || probability_velocity_blocks(&primary_side, prob_velocity));

            let (target_ratio, label) = if severe_primary_wrong {
                (DX_HEDGE_SEVERE_TARGET_RATIO, "severe_adverse")
            } else if regular_hedge {
                let target = if primary_adverse_after_cross {
                    DX_HEDGE_ADVERSE_TARGET_RATIO
                } else {
                    DX_HEDGE_BASE_TARGET_RATIO
                };
                (target, "edge_hedge")
            } else {
                (0.0, "none")
            };

            let (hedge_cost, _) = side_buy_cost_shares(hedge_side, win_state);
            let max_hedge_cost = win_state.spent * DX_HEDGE_MAX_FRACTION_OF_SPENT;
            let remaining_hedge_usd = (max_hedge_cost - hedge_cost).max(0.0);
            let hedge_gap = (target_ratio - current_hedge_ratio).max(0.0);
            let desired_usd = primary_shares * hedge_gap * hedge_ask;
            let step_cap = win_state.spent * DX_HEDGE_STEP_FRACTION_OF_SPENT;
            let buy_usd = desired_usd.min(step_cap).min(remaining_hedge_usd);

            if target_ratio > 0.0 && buy_usd >= DX_MIN_TRADE_USD {
                signals.push(OrderSignal {
                    side: hedge_side.to_string(),
                    is_buy: true,
                    amount: buy_usd,
                    price: hedge_ask,
                    reason: format!(
                        "dx_hedge_buy_{}_label_{}_phase_{}_atr_{}_ask_{:.2}_fair_{:.3}_edge_{:+.3}_primary_p_{:.3}_cross_now_{}_baseline_{}_relation_{}_pair_cost_{:.2}_hedge_now_{:.2}_target_{:.2}_gap_{:.2}_usd_{:.2}",
                        hedge_side.to_lowercase(),
                        label,
                        phase.as_str(),
                        atr_regime.as_str(),
                        hedge_ask,
                        hedge_prob,
                        hedge_edge,
                        primary_prob,
                        cross_happened_now,
                        ptb_baseline,
                        relation,
                        pair_cost,
                        current_hedge_ratio,
                        target_ratio,
                        hedge_gap,
                        buy_usd
                    ),
                });
                return signals;
            }
        }

        // HEDGE PTB: unload hedge when it pays us for the protection.
        if hedge_shares > DX_SELL_MIN_SHARES {
            if let Some(hedge_avg) = side_avg_entry(hedge_side, win_state) {
                if hedge_bid >= hedge_avg + DX_HEDGE_PTB_TARGET
                    && hedge_bid >= hedge_prob + DX_PRIMARY_SELL_EDGE
                {
                    signals.push(OrderSignal {
                        side: hedge_side.to_string(),
                        is_buy: false,
                        amount: hedge_shares,
                        price: hedge_bid,
                        reason: format!(
                            "dx_hedge_ptb_sell_{}_phase_{}_bid_{:.2}_avg_{:.2}_fair_{:.3}_edge_{:+.3}_shares_{:.4}",
                            hedge_side.to_lowercase(),
                            phase.as_str(),
                            hedge_bid,
                            hedge_avg,
                            hedge_prob,
                            hedge_bid - hedge_prob,
                            hedge_shares
                        ),
                    });
                    return signals;
                }
            }
        }

        // PRIMARY PTB: lock profit, but keep clear close-window winners.
        if primary_shares > DX_SELL_MIN_SHARES {
            let primary_avg =
                side_avg_entry(&primary_side, win_state).unwrap_or(primary_entry_price);
            let step_key = (window_number, primary_side.clone());
            let current_step = *self.sell_steps.get(&step_key).unwrap_or(&0);
            let target = primary_ptb_target(current_step, phase);
            let close_redeem_hold = secs_to_end <= DX_PRIMARY_HOLD_REDEEM_SECONDS
                && primary_prob >= 0.72
                && relation_is_favorable_for_side(&primary_side, relation);

            if !close_redeem_hold
                && primary_bid >= primary_avg + target
                && primary_bid >= primary_prob + DX_PRIMARY_SELL_EDGE
            {
                let paired_core = win_state.up_shares.min(win_state.down_shares);
                let surplus = (primary_shares - paired_core).max(0.0);
                let sell_fraction = match current_step {
                    0 => 0.35,
                    1 => 0.35,
                    _ => 1.0,
                };
                let mut sell_amount = (primary_shares * sell_fraction).min(primary_shares);
                if surplus > DX_SELL_MIN_SHARES {
                    sell_amount = sell_amount.min(surplus);
                } else if primary_bid < 0.80 && !matches!(phase, DxPhase::Final) {
                    sell_amount = 0.0;
                }

                if sell_amount > DX_SELL_MIN_SHARES {
                    self.sell_steps.insert(step_key, current_step + 1);
                    signals.push(OrderSignal {
                        side: primary_side.to_string(),
                        is_buy: false,
                        amount: sell_amount,
                        price: primary_bid,
                        reason: format!(
                            "dx_primary_ptb_sell_{}_step_{}_phase_{}_bid_{:.2}_avg_{:.2}_target_{:.3}_fair_{:.3}_edge_{:+.3}_sell_{:.4}_paired_{:.4}_time_{:.1}",
                            primary_side.to_lowercase(),
                            current_step + 1,
                            phase.as_str(),
                            primary_bid,
                            primary_avg,
                            target,
                            primary_prob,
                            primary_bid - primary_prob,
                            sell_amount,
                            paired_core,
                            time_pct
                        ),
                    });
                    return signals;
                }
            }
        }

        // FINAL SALVAGE: only sell OTM surplus if market overpays it.
        if matches!(phase, DxPhase::Final) {
            let paired_core = win_state.up_shares.min(win_state.down_shares);
            for side in ["UP", "DOWN"] {
                let shares = side_shares(side, win_state);
                let surplus = (shares - paired_core).max(0.0);
                if surplus <= DX_SELL_MIN_SHARES {
                    continue;
                }
                let bid = side_bid(side, prices);
                let prob = side_fair_probability(side, fair_up);
                let side_is_favorable = relation_is_favorable_for_side(side, relation);
                if !side_is_favorable && bid >= DX_FINAL_MIN_BID && bid >= prob + DX_FINAL_SELL_EDGE
                {
                    signals.push(OrderSignal {
                        side: side.to_string(),
                        is_buy: false,
                        amount: surplus,
                        price: bid,
                        reason: format!(
                            "dx_final_salvage_sell_{}_bid_{:.2}_fair_{:.3}_edge_{:+.3}_sell_{:.4}_paired_{:.4}_relation_{}",
                            side.to_lowercase(),
                            bid,
                            prob,
                            bid - prob,
                            surplus,
                            paired_core,
                            relation
                        ),
                    });
                    return signals;
                }
            }
        }

        signals
    }

    fn get_strategy_state(&self, window_number: usize) -> Option<StrategyState> {
        self.states.get(&window_number).cloned()
    }
}

fn fmt_opt(value: Option<f64>) -> String {
    value
        .map(|v| format!("{:+.4}", v))
        .unwrap_or_else(|| "NA".to_string())
}
