use crate::cex_micro::cex_velocity_against_side;
use crate::client::{get_now_ms, MarketWindow, PricesState};
use crate::config::Config;
use crate::mid_cross_tracker::{LeadSide, MidCrossSnapshot, MID_CROSS_ARM_TIME_PCT};
use crate::redeem_hold::{evaluate_redeem_hold, side_is_itm, RedeemHoldInput};
use crate::strategy::{
    CexMicroSnapshot, OrderSignal, SpotSignalSnapshot, StrategyState, TradeStrategy,
};
use crate::trader::WindowState;
use std::collections::HashMap;

/// Bootstrap window at process start — observe only; first tradeable window is #1.
const E_MIN_TRADEABLE_WINDOW: usize = 1;

const E_MIN_TRADE_USD: f64 = 1.0;
const E_LIVE_BUDGET_MULT: f64 = 0.90;
/// Block entry when the market ping-pongs (any mid flips, not only "significant" ones).
const E_MAX_CHOP_CROSSES: u32 = 5;
const E_CHOP_SKIP_TIME_PCT: f64 = 40.0;
/// Leader gap after a mid cross — cross-momentum entry window.
const E_MIN_LEAD_GAP_FOR_ENTRY: f64 = 0.14;
const E_CROSS_ENTRY_WINDOW_PCT: f64 = 10.0;
/// Buy new leader shortly after cross before ask runs away (e.g. 0.51 → 0.79).
const E_CROSS_MAX_ASK: f64 = 0.58;
const E_MIN_CONVICTION: f64 = 0.14;
const E_CONVICTION_GAP: f64 = 0.08;
const E_MAX_CONTRARIAN_GAP_Z: f64 = 0.10;
const E_MAX_ABS_GAP_Z: f64 = 0.60;
/// Value path: cheap underdog, spot not too detached from PTB.
const E_MAX_CHEAP_ASK: f64 = 0.50;
const E_MIN_VALUE_EDGE: f64 = 0.02;
const E_MIN_ENTRY_EDGE: f64 = 0.04;
const E_ENTRY_CUTOFF_TIME_PCT: f64 = 62.0;
const E_TRANCHE_COOLDOWN_MS: i64 = 45_000;
const E_TRANCHE_TIME_PCTS: [f64; 3] = [12.0, 30.0, 50.0];
const E_TRANCHE_FRACTIONS: [f64; 3] = [0.40, 0.35, 0.25];
const E_GRID_DELTAS: [f64; 3] = [0.08, 0.14, 0.22];
const E_GRID_FRACTIONS: [f64; 3] = [0.30, 0.30, 0.25];
/// After base 3 steps, keep selling on new highs (no hard cap).
const E_GRID_EXTEND_DELTA: f64 = 0.06;
const E_GRID_EXTEND_FRACTION: f64 = 0.20;
const E_RUNNER_FRACTION: f64 = 0.40;
const E_REENTRY_AFTER_TRIM_MS: i64 = 120_000;
const E_DIRECTIONAL_GAP_Z: f64 = 0.05;
const E_HEDGE_PAIR_COST_MAX: f64 = 0.90;
const E_HEDGE_MIN_USD: f64 = 3.0;
const E_HEDGE_MAX_USD: f64 = 8.0;
const E_FINAL_TIME_PCT: f64 = 88.0;
const E_FINAL_SECS: i64 = 25;
const E_LATE_TIME_PCT: f64 = 75.0;
const E_MAX_PTB_TRIMS: u32 = 1;
const E_PTB_TRIM_FRACTION: f64 = 0.30;
const E_TRIM_COOLDOWN_MS: i64 = 60_000;
const E_SELL_MIN_SHARES: f64 = 0.000001;

#[derive(Debug, Clone)]
struct EWindowState {
    conviction_side: Option<String>,
    buy_tranches_done: u8,
    sell_steps_done: usize,
    last_spot_itm: Option<bool>,
    last_buy_ms: i64,
    ptb_trim_count: u32,
    last_trim_ms: i64,
    emergency_sold: bool,
    ptb_baseline: Option<String>,
    ptb_crossed: bool,
    last_grid_target: f64,
}

fn relation_for_spot(spot: f64, ptb: f64) -> Option<&'static str> {
    if spot > ptb {
        Some("ABOVE")
    } else if spot < ptb {
        Some("BELOW")
    } else {
        None
    }
}

fn update_e_ptb_cross(state: &mut EWindowState, spot: f64, ptb: f64) -> bool {
    let Some(relation) = relation_for_spot(spot, ptb) else {
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

pub struct ConvictionRouterStrategy {
    windows: HashMap<usize, EWindowState>,
}

impl ConvictionRouterStrategy {
    pub fn new() -> Self {
        Self {
            windows: HashMap::new(),
        }
    }
}

fn side_bid(side: &str, prices: &PricesState) -> f64 {
    if side == "UP" {
        prices.up.bid
    } else {
        prices.down.bid
    }
}

fn side_ask(side: &str, prices: &PricesState) -> f64 {
    if side == "UP" {
        prices.up.ask
    } else {
        prices.down.ask
    }
}

fn shares_for_side(side: &str, win: &WindowState) -> f64 {
    if side == "UP" {
        win.up_shares
    } else {
        win.down_shares
    }
}

fn side_entry_avg_price(side: &str, win: &WindowState, fallback_ask: f64) -> f64 {
    let mut spent = 0.0;
    let mut shares = 0.0;
    for trade in &win.trades {
        if trade.trade_type == "BUY" && trade.side == side {
            spent += trade.usd_value;
            shares += trade.shares;
        }
    }
    if shares > E_SELL_MIN_SHARES {
        spent / shares
    } else {
        fallback_ask
    }
}

fn primary_position_side(win: &WindowState, preferred: Option<&str>) -> Option<String> {
    if let Some(side) = preferred {
        if shares_for_side(side, win) > E_SELL_MIN_SHARES {
            return Some(side.to_string());
        }
    }

    let has_up = win.up_shares > E_SELL_MIN_SHARES;
    let has_down = win.down_shares > E_SELL_MIN_SHARES;
    match (has_up, has_down) {
        (true, false) => Some("UP".to_string()),
        (false, true) => Some("DOWN".to_string()),
        (true, true) => {
            if win.up_shares >= win.down_shares {
                Some("UP".to_string())
            } else {
                Some("DOWN".to_string())
            }
        }
        _ => None,
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
    let duration = market_duration_sec(market);
    let elapsed = (duration - secs_to_end as f64).clamp(0.0, duration);
    (elapsed / duration) * 100.0
}

fn expected_move_usd(atr: f64, secs_left: i64) -> f64 {
    atr.max(1.0) * ((secs_left as f64).max(1.0) / 60.0).sqrt()
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

fn fair_probability_up(
    market: &MarketWindow,
    spot: Option<f64>,
    atr: f64,
    secs_left: i64,
    spot_signal: SpotSignalSnapshot,
) -> f64 {
    let (Some(spot), Some(ptb)) = (spot, market.price_to_beat) else {
        return 0.50;
    };
    let vel = spot_signal
        .smoothed_velocity_usd_per_sec
        .or(spot_signal.raw_velocity_usd_per_sec)
        .unwrap_or(0.0);
    let expected = expected_move_usd(atr, secs_left);
    let drift = (vel * secs_left as f64 * 0.15).clamp(-expected * 0.35, expected * 0.35);
    normal_cdf(((spot - ptb) + drift) / expected)
}

fn side_fair(side: &str, fair_up: f64) -> f64 {
    if side == "UP" {
        fair_up
    } else {
        1.0 - fair_up
    }
}

fn spot_aligned(side: &str, gap_z: f64) -> bool {
    match side {
        "UP" => gap_z >= -E_MAX_CONTRARIAN_GAP_Z,
        "DOWN" => gap_z <= E_MAX_CONTRARIAN_GAP_Z,
        _ => false,
    }
}

fn conviction_score(
    side: &str,
    spot: f64,
    ptb: f64,
    atr: f64,
    secs_to_end: i64,
    mid_cross: &MidCrossSnapshot,
    cex_micro: &CexMicroSnapshot,
    fair_side: f64,
) -> f64 {
    let expected = expected_move_usd(atr, secs_to_end);
    let raw_gap_z = if expected > 0.0 {
        (spot - ptb) / expected
    } else {
        0.0
    };
    let gap_component = if side == "UP" { raw_gap_z } else { -raw_gap_z };

    let mid_component = match mid_cross.current_side {
        Some(LeadSide::Up) if side == "UP" => mid_cross.lead_gap * 0.6,
        Some(LeadSide::Down) if side == "DOWN" => mid_cross.lead_gap * 0.6,
        Some(LeadSide::Up) if side == "DOWN" => -mid_cross.lead_gap * 0.35,
        Some(LeadSide::Down) if side == "UP" => -mid_cross.lead_gap * 0.35,
        _ => 0.0,
    };

    let cex_sign = if side == "UP" { 1.0 } else { -1.0 };
    let cex_component = cex_micro
        .trade_velocity_3s
        .map(|v| (v * cex_sign / 100.0).clamp(-0.25, 0.25))
        .unwrap_or(0.0);

    let fair_component = (fair_side - 0.50) * 0.8;
    gap_component * 0.40 + mid_component * 0.25 + fair_component * 0.25 + cex_component * 0.10
}

fn pick_conviction_side(
    spot: f64,
    ptb: f64,
    atr: f64,
    secs_to_end: i64,
    mid_cross: &MidCrossSnapshot,
    cex_micro: &CexMicroSnapshot,
    fair_up: f64,
) -> Option<&'static str> {
    let up = conviction_score(
        "UP",
        spot,
        ptb,
        atr,
        secs_to_end,
        mid_cross,
        cex_micro,
        fair_up,
    );
    let down = conviction_score(
        "DOWN",
        spot,
        ptb,
        atr,
        secs_to_end,
        mid_cross,
        cex_micro,
        1.0 - fair_up,
    );
    if up >= E_MIN_CONVICTION && up - down >= E_CONVICTION_GAP {
        Some("UP")
    } else if down >= E_MIN_CONVICTION && down - up >= E_CONVICTION_GAP {
        Some("DOWN")
    } else {
        None
    }
}

fn chop_too_high(mid_cross: &MidCrossSnapshot, time_pct: f64) -> bool {
    mid_cross.cross_count >= E_MAX_CHOP_CROSSES && time_pct < E_CHOP_SKIP_TIME_PCT
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryKind {
    Value,
    Cross,
}

fn cheap_entry_side(prices: &PricesState) -> Option<&'static str> {
    let up = prices.up.ask;
    let down = prices.down.ask;
    let up_ok = up > 0.0 && up <= E_MAX_CHEAP_ASK;
    let down_ok = down > 0.0 && down <= E_MAX_CHEAP_ASK;
    match (up_ok, down_ok) {
        (true, true) => {
            if up <= down {
                Some("UP")
            } else {
                Some("DOWN")
            }
        }
        (true, false) => Some("UP"),
        (false, true) => Some("DOWN"),
        _ => None,
    }
}

fn cross_lead_side(mid_cross: &MidCrossSnapshot) -> Option<&'static str> {
    match mid_cross.current_side {
        Some(LeadSide::Up) => Some("UP"),
        Some(LeadSide::Down) => Some("DOWN"),
        _ => None,
    }
}

fn cross_entry_active(mid_cross: &MidCrossSnapshot, time_pct: f64) -> bool {
    let Some(cross_pct) = mid_cross.last_cross_time_pct else {
        return false;
    };
    mid_cross.lead_gap >= E_MIN_LEAD_GAP_FOR_ENTRY
        && time_pct >= cross_pct
        && time_pct <= cross_pct + E_CROSS_ENTRY_WINDOW_PCT
}

fn directional_entry_side(gap_z: f64, prices: &PricesState) -> Option<&'static str> {
    if gap_z >= E_DIRECTIONAL_GAP_Z {
        let ask = prices.up.ask;
        if ask > 0.0 && ask <= E_CROSS_MAX_ASK {
            return Some("UP");
        }
    } else if gap_z <= -E_DIRECTIONAL_GAP_Z {
        let ask = prices.down.ask;
        if ask > 0.0 && ask <= E_CROSS_MAX_ASK {
            return Some("DOWN");
        }
    }
    None
}

fn pick_entry_side(
    prices: &PricesState,
    mid_cross: &MidCrossSnapshot,
    time_pct: f64,
    gap_z: f64,
) -> Option<(&'static str, EntryKind)> {
    if cross_entry_active(mid_cross, time_pct) {
        if let Some(side) = cross_lead_side(mid_cross) {
            let ask = side_ask(side, prices);
            if ask > 0.0 && ask <= E_CROSS_MAX_ASK {
                return Some((side, EntryKind::Cross));
            }
        }
    }
    if let Some(side) = directional_entry_side(gap_z, prices) {
        return Some((side, EntryKind::Cross));
    }
    cheap_entry_side(prices).map(|side| (side, EntryKind::Value))
}

fn trim_blocks_reentry(state: &EWindowState, side: &str, now_ms: i64) -> bool {
    state.ptb_trim_count > 0
        && state.last_trim_ms > 0
        && now_ms - state.last_trim_ms < E_REENTRY_AFTER_TRIM_MS
        && state.conviction_side.as_deref() == Some(side)
}

fn capped_grid_sell(conv_shares: f64, fraction: f64, time_pct: f64) -> f64 {
    let mut sell = (conv_shares * fraction).max(E_SELL_MIN_SHARES);
    if time_pct < E_LATE_TIME_PCT {
        let runner_floor = conv_shares * E_RUNNER_FRACTION;
        sell = sell.min((conv_shares - runner_floor).max(0.0));
    }
    sell
}

fn redeem_hold_blocks(
    side: &str,
    spot: f64,
    ptb: f64,
    secs_to_end: i64,
    time_pct: f64,
    atr: f64,
    bid: f64,
    fair_prob: f64,
    ptb_crossed: bool,
    spot_signal: SpotSignalSnapshot,
    cex_micro: &CexMicroSnapshot,
) -> bool {
    let side_sign = if side == "UP" { 1.0 } else { -1.0 };
    let counter = spot_signal
        .smoothed_velocity_usd_per_sec
        .or(spot_signal.raw_velocity_usd_per_sec)
        .map(|v| v * side_sign < -0.10)
        .unwrap_or(false);
    evaluate_redeem_hold(&RedeemHoldInput {
        side,
        spot,
        ptb,
        secs_to_end,
        time_pct,
        current_atr: atr,
        bid,
        fair_prob,
        ptb_crossed,
        counter_velocity_against: counter,
        cex_velocity_against: cex_velocity_against_side(side, cex_micro),
    })
    .should_hold
}

impl TradeStrategy for ConvictionRouterStrategy {
    fn check_pre_start_entry(
        &mut self,
        _config: &Config,
        _prices: &PricesState,
        _market: &MarketWindow,
        _spot_price: Option<f64>,
        _window_number: usize,
        _secs_to_start: i64,
        _current_atr: f64,
        _spot_signal: SpotSignalSnapshot,
        _llm_forecast: Option<crate::strategy::LlmForecast>,
        _cex_micro: &CexMicroSnapshot,
    ) -> Option<crate::strategy::EntrySignal> {
        // E trades the active/current window only via process_live_tick.
        // NEXT is observed by the runtime but never bought pre-start.
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
        tape: &crate::trade_tape::TradeTapeSnapshot,
    ) -> Vec<OrderSignal> {
        let mut signals = Vec::new();
        let window_number = win_state.window_number;
        if window_number < E_MIN_TRADEABLE_WINDOW {
            return signals;
        }
        let time_pct = time_pct_for(market, secs_to_end);
        let state = self.windows.entry(window_number).or_insert(EWindowState {
            conviction_side: None,
            buy_tranches_done: 0,
            sell_steps_done: 0,
            last_spot_itm: None,
            last_buy_ms: 0,
            ptb_trim_count: 0,
            last_trim_ms: 0,
            emergency_sold: false,
            ptb_baseline: None,
            ptb_crossed: false,
            last_grid_target: 0.0,
        });

        let (Some(spot), Some(ptb)) = (spot_price, market.price_to_beat) else {
            return signals;
        };
        update_e_ptb_cross(state, spot, ptb);
        let ptb_crossed = state.ptb_crossed;
        let fair_up =
            fair_probability_up(market, Some(spot), current_atr, secs_to_end, spot_signal);

        if time_pct >= E_FINAL_TIME_PCT || secs_to_end <= E_FINAL_SECS {
            if !state.emergency_sold {
                for side in ["UP", "DOWN"] {
                    let shares = shares_for_side(side, win_state);
                    let bid = side_bid(side, prices);
                    if shares <= E_SELL_MIN_SHARES || bid < 0.05 {
                        continue;
                    }
                    let fair = side_fair(side, fair_up);
                    if redeem_hold_blocks(
                        side,
                        spot,
                        ptb,
                        secs_to_end,
                        time_pct,
                        current_atr,
                        bid,
                        fair,
                        ptb_crossed,
                        spot_signal,
                        cex_micro,
                    ) {
                        continue;
                    }
                    let itm = side_is_itm(side, spot, ptb);
                    if itm && fair >= 0.35 {
                        continue;
                    }
                    signals.push(OrderSignal {
                        side: side.to_string(),
                        is_buy: false,
                        amount: shares,
                        price: bid,
                        reason: if itm {
                            "e_emergency_itm".to_string()
                        } else {
                            "e_emergency_otm".to_string()
                        },
                    });
                }
                if !signals.is_empty() {
                    state.emergency_sold = true;
                }
            }
            return signals;
        }

        let held_side = primary_position_side(win_state, state.conviction_side.as_deref());
        if state.conviction_side.is_none() {
            state.conviction_side = held_side.clone();
        }

        let fresh_conviction_side = pick_conviction_side(
            spot,
            ptb,
            current_atr,
            secs_to_end,
            mid_cross,
            cex_micro,
            fair_up,
        );
        if let Some(side) = fresh_conviction_side {
            if held_side
                .as_deref()
                .map(|held| held == side)
                .unwrap_or(true)
            {
                state.conviction_side = Some(side.to_string());
            }
        }

        let budget_total = (config.session.min_window_budget * E_LIVE_BUDGET_MULT)
            .clamp(E_MIN_TRADE_USD, config.session.max_window_budget);
        let remaining_budget = (budget_total - win_state.spent).max(0.0);

        if mid_cross.armed
            && time_pct >= MID_CROSS_ARM_TIME_PCT
            && time_pct <= E_ENTRY_CUTOFF_TIME_PCT
            && !chop_too_high(mid_cross, time_pct)
            && state.buy_tranches_done < E_TRANCHE_TIME_PCTS.len() as u8
            && remaining_budget >= E_MIN_TRADE_USD
        {
            let tranche_idx = state.buy_tranches_done as usize;
            let now_ms = get_now_ms();
            let cooldown_ready =
                state.last_buy_ms <= 0 || now_ms - state.last_buy_ms >= E_TRANCHE_COOLDOWN_MS;
            if time_pct >= E_TRANCHE_TIME_PCTS[tranche_idx] && cooldown_ready {
                let expected = expected_move_usd(current_atr, secs_to_end);
                let gap_z = (spot - ptb) / expected;
                let entry_pick = if tranche_idx > 0 {
                    cheap_entry_side(prices).map(|side| (side, EntryKind::Value))
                } else {
                    pick_entry_side(prices, mid_cross, time_pct, gap_z)
                };
                if let Some((side, entry_kind)) = entry_pick {
                    let can_buy_side = held_side
                        .as_deref()
                        .map(|held| held == side)
                        .unwrap_or(true);
                    if can_buy_side
                        && gap_z.is_finite()
                        && gap_z.abs() <= E_MAX_ABS_GAP_Z
                        && spot_aligned(side, gap_z)
                        && !trim_blocks_reentry(state, side, now_ms)
                    {
                        let ask = side_ask(side, prices);
                        let fair = side_fair(side, fair_up);
                        let min_edge = match entry_kind {
                            EntryKind::Value => E_MIN_VALUE_EDGE,
                            EntryKind::Cross => E_MIN_ENTRY_EDGE,
                        };
                        let max_ask = match entry_kind {
                            EntryKind::Value => E_MAX_CHEAP_ASK,
                            EntryKind::Cross => E_CROSS_MAX_ASK,
                        };
                        let kind_tag = match entry_kind {
                            EntryKind::Value => "value",
                            EntryKind::Cross => "cross",
                        };
                        if ask > 0.0
                            && ask <= max_ask
                            && fair - ask >= min_edge
                            && !cex_velocity_against_side(side, cex_micro)
                        {
                            let conviction = conviction_score(
                                side,
                                spot,
                                ptb,
                                current_atr,
                                secs_to_end,
                                mid_cross,
                                cex_micro,
                                fair,
                            );
                            let tranche_usd = (budget_total * E_TRANCHE_FRACTIONS[tranche_idx])
                                .clamp(E_MIN_TRADE_USD, remaining_budget);
                            signals.push(OrderSignal {
                                side: side.to_string(),
                                is_buy: true,
                                amount: tranche_usd,
                                price: ask,
                                reason: format!(
                                    "e_tranche{}_{}_{}_ask_{:.2}_conv_{:.3}_gap_z_{:+.2}_lead_{:.2}",
                                    tranche_idx + 1,
                                    kind_tag,
                                    side.to_lowercase(),
                                    ask,
                                    conviction,
                                    gap_z,
                                    mid_cross.lead_gap
                                ),
                            });
                            state.conviction_side = Some(side.to_string());
                            state.buy_tranches_done += 1;
                            state.last_buy_ms = now_ms;
                            return signals;
                        }
                    }
                }
            }
        }

        let conviction = match &state.conviction_side {
            Some(s) => s.clone(),
            None => return signals,
        };
        let conv_shares = shares_for_side(&conviction, win_state);
        if conv_shares <= E_SELL_MIN_SHARES {
            return signals;
        }

        let bid = side_bid(&conviction, prices);
        let spot_itm = side_is_itm(&conviction, spot, ptb);
        let now_ms = get_now_ms();
        if matches!(state.last_spot_itm, Some(true))
            && !spot_itm
            && state.ptb_trim_count < E_MAX_PTB_TRIMS
            && (state.last_trim_ms <= 0 || now_ms - state.last_trim_ms >= E_TRIM_COOLDOWN_MS)
            && bid >= 0.10
        {
            let sell = (conv_shares * E_PTB_TRIM_FRACTION).max(E_SELL_MIN_SHARES);
            if sell <= conv_shares {
                signals.push(OrderSignal {
                    side: conviction.clone(),
                    is_buy: false,
                    amount: sell,
                    price: bid,
                    reason: format!("e_ptb_cross_trim_bid_{:.2}", bid),
                });
                state.ptb_trim_count += 1;
                state.last_trim_ms = now_ms;
                state.last_spot_itm = Some(spot_itm);
                return signals;
            }
        }

        let fair_conv = side_fair(&conviction, fair_up);
        if redeem_hold_blocks(
            &conviction,
            spot,
            ptb,
            secs_to_end,
            time_pct,
            current_atr,
            bid,
            fair_conv,
            ptb_crossed,
            spot_signal,
            cex_micro,
        ) {
            state.last_spot_itm = Some(spot_itm);
            return signals;
        }

        let opposite = if conviction == "UP" { "DOWN" } else { "UP" };
        let opp_ask = side_ask(opposite, prices);
        let pair_cost = side_ask(&conviction, prices) + opp_ask;
        if pair_cost <= E_HEDGE_PAIR_COST_MAX
            && opp_ask <= 0.38
            && !side_is_itm(&conviction, spot, ptb)
            && win_state.spent + E_HEDGE_MIN_USD <= budget_total
        {
            let opp_shares = shares_for_side(opposite, win_state);
            if opp_shares <= E_SELL_MIN_SHARES && remaining_budget >= E_HEDGE_MIN_USD {
                let hedge_usd = E_HEDGE_MIN_USD.min(remaining_budget).min(E_HEDGE_MAX_USD);
                signals.push(OrderSignal {
                    side: opposite.to_string(),
                    is_buy: true,
                    amount: hedge_usd,
                    price: opp_ask,
                    reason: format!("e_hedge_pair_{:.2}_spot_otm", pair_cost),
                });
                return signals;
            }
        }

        let entry_ask =
            side_entry_avg_price(&conviction, win_state, side_ask(&conviction, prices));
        let edge = bid - fair_conv;
        if edge >= 0.04 {
            let step = state.sell_steps_done;
            if step < E_GRID_DELTAS.len() {
                let target = (entry_ask + E_GRID_DELTAS[step]).clamp(0.05, 0.99);
                if bid >= target {
                    let sell = capped_grid_sell(conv_shares, E_GRID_FRACTIONS[step], time_pct);
                    if sell >= E_SELL_MIN_SHARES && sell <= conv_shares {
                        signals.push(OrderSignal {
                            side: conviction.clone(),
                            is_buy: false,
                            amount: sell,
                            price: bid,
                            reason: format!(
                                "e_grid_step{}_bid_{:.2}_target_{:.2}",
                                step + 1,
                                bid,
                                target
                            ),
                        });
                        state.sell_steps_done += 1;
                        state.last_grid_target = target;
                    }
                }
            } else if state.last_grid_target > 0.0 {
                let target = (state.last_grid_target + E_GRID_EXTEND_DELTA).clamp(0.05, 0.98);
                if bid >= target {
                    let sell =
                        capped_grid_sell(conv_shares, E_GRID_EXTEND_FRACTION, time_pct);
                    if sell >= E_SELL_MIN_SHARES && sell <= conv_shares {
                        signals.push(OrderSignal {
                            side: conviction.clone(),
                            is_buy: false,
                            amount: sell,
                            price: bid,
                            reason: format!(
                                "e_grid_ext{}_bid_{:.2}_target_{:.2}",
                                step - E_GRID_DELTAS.len() + 1,
                                bid,
                                target
                            ),
                        });
                        state.sell_steps_done += 1;
                        state.last_grid_target = target;
                    }
                }
            }
        }

        state.last_spot_itm = Some(spot_itm);
        signals
    }

    fn get_strategy_state(&self, window_number: usize) -> Option<StrategyState> {
        self.windows.get(&window_number).map(|s| StrategyState {
            up_sold: s.sell_steps_done > 0 && s.conviction_side.as_deref() == Some("UP"),
            down_sold: s.sell_steps_done > 0 && s.conviction_side.as_deref() == Some("DOWN"),
            first_sold_side: None,
            ptb_crossed: s.ptb_crossed,
            ptb_baseline: s.ptb_baseline.clone(),
            e_conviction_side: s.conviction_side.clone(),
            e_tranches_done: s.buy_tranches_done,
            e_grid_steps_done: s.sell_steps_done as u8,
            h_entry_side: None,
            h_entry_done: false,
            h_salvage_done: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mid_cross_tracker::{LeadSide, MidCrossSnapshot};

    fn empty_mid() -> MidCrossSnapshot {
        MidCrossSnapshot {
            armed: true,
            current_side: Some(LeadSide::Up),
            lead_gap: 0.20,
            up_mid: 0.55,
            down_mid: 0.45,
            cross_count: 1,
            significant_cross_count: 1,
            peak_lead_gap: 0.20,
            last_cross_from: None,
            last_cross_to: Some(LeadSide::Up),
            last_cross_time_pct: Some(15.0),
            last_cross_is_significant: true,
            last_cross_atr: 50.0,
        }
    }

    #[test]
    fn picks_up_when_spot_and_mid_align() {
        let mid = empty_mid();
        let cex = CexMicroSnapshot::default();
        let side = pick_conviction_side(61_250.0, 61_200.0, 50.0, 600, &mid, &cex, 0.62);
        assert_eq!(side, Some("UP"));
    }

    #[test]
    fn spot_aligned_blocks_contrarian_down() {
        assert!(!spot_aligned("DOWN", 0.15));
        assert!(spot_aligned("DOWN", 0.05));
    }

    #[test]
    fn chop_blocks_on_any_flip_count() {
        let mid = MidCrossSnapshot {
            cross_count: 5,
            significant_cross_count: 0,
            ..Default::default()
        };
        assert!(chop_too_high(&mid, 25.0));
        assert!(!chop_too_high(&mid, 45.0));
    }

    #[test]
    fn cheap_entry_picks_cheaper_ask_under_cap() {
        let prices = PricesState {
            up: crate::client::ContractPrices::top(0.20, 0.22),
            down: crate::client::ContractPrices::top(0.77, 0.79),
        };
        assert_eq!(cheap_entry_side(&prices), Some("UP"));
    }

    #[test]
    fn cross_entry_prioritized_inside_window() {
        let prices = PricesState {
            up: crate::client::ContractPrices::top(0.40, 0.42),
            down: crate::client::ContractPrices::top(0.55, 0.56),
        };
        let mid = MidCrossSnapshot {
            armed: true,
            current_side: Some(LeadSide::Down),
            lead_gap: 0.16,
            last_cross_time_pct: Some(58.0),
            last_cross_to: Some(LeadSide::Down),
            ..Default::default()
        };
        let picked = pick_entry_side(&prices, &mid, 60.0, 0.0);
        assert_eq!(picked, Some(("DOWN", EntryKind::Cross)));
    }
}
