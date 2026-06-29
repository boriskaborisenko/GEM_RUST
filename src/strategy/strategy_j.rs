use crate::client::{MarketWindow, PricesState};
use crate::config::Config;
use crate::j_fees::{leg_fee_usd, DEFAULT_CRYPTO_FEE_RATE_BPS};
use crate::mid_cross_tracker::LeadSide;
use crate::orderbook::{apply_fill_to_asks, ask_depth_usd, simulate_taker_buy_fill, SideBook};
use crate::redeem_hold::expected_move_usd;
use crate::strategy::{
    CexMicroSnapshot, MidCrossSnapshot, OrderOperation, OrderSignal, OrderType, SpotSignalSnapshot,
    StrategyState, TradeStrategy,
};
use crate::trade_tape::{TradeTapeSnapshot, TradeTapeTracker};
use crate::trader::WindowState;
use std::collections::HashMap;
use std::time::Instant;

const J_MIN_TRADEABLE_WINDOW: usize = 1;
const J_TAIL_ADD_DANGER_ATR_MULT: f64 = 1.5;
const J_TAIL_SELL_NEAR_ATR_MULT: f64 = 0.20;
const J_TAIL_SELL_MAX_FRACTION: f64 = 0.50;

#[derive(Debug, Clone, Default)]
pub(crate) struct JWindowState {
    pub(crate) impulse_spent_usd: f64,
    pub(crate) cheap_spent_usd: f64,
    pub(crate) late_spent_usd: f64,
    pub(crate) hedge_spent_usd: f64,
    pub(crate) insurance_spent_usd: f64,
    /// USD deployed on the winner during the composite endgame this window.
    /// Used as the "already deployed" term for target-exposure throttling.
    pub(crate) rescue_spent_usd: f64,
    pub(crate) discount_reload_spent_usd: f64,
    pub(crate) cheap_clips: u16,
    pub(crate) late_clips: u16,
    pub(crate) hedge_clips: u16,
    pub(crate) insurance_clips: u16,
    pub(crate) discount_reload_clips: u16,
    pub(crate) clips_filled: u16,
    pub(crate) primary_side: Option<String>,
    pub(crate) insurance_side: Option<String>,
    pub(crate) winner_side: Option<String>,
    pub(crate) last_endgame_buy_at: Option<Instant>,
    pub(crate) sell_rescue_done: bool,
    /// Cross counts frozen at first endgame tick — chop gate uses this snapshot,
    /// not the live cumulative counter (which grows every tick and blocked ETH).
    pub(crate) entry_cross_snapshot: Option<(u32, u32)>,
    pub(crate) directional_blocked_chop: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndgameTier {
    Insurance,
    Impulse,
    Cheap,
    Late,
    FlipHedge,
    Rescue,
    DiscountReload,
    FinalSeal,
}

#[derive(Debug, Clone)]
pub struct TierPlan {
    pub tier: EndgameTier,
    pub max_pay: f64,
    pub need_tape: bool,
    pub budget_left: f64,
    pub sweep_clips: u8,
    /// When set, buy this side (flip hedge / insurance / rescue).
    pub side: Option<String>,
    /// Per-action clip size; 0 = use config default.
    pub clip_usd: f64,
}

pub struct JEndgameStrategy {
    windows: HashMap<usize, JWindowState>,
    available_cash: f64,
}

impl JEndgameStrategy {
    pub fn new() -> Self {
        Self {
            windows: HashMap::new(),
            available_cash: 0.0,
        }
    }

    pub(crate) fn mark_sell_rescue_executed(&mut self, window_number: usize, signal: &OrderSignal) {
        if let Some(state) = self.windows.get_mut(&window_number) {
            state.sell_rescue_done = true;
            let recovered_usd = (signal.amount * signal.price).max(0.0);
            state.release_primary_exposure_usd(recovered_usd);
        }
    }

    fn mark_buy_executed(&mut self, window_number: usize, signal: &OrderSignal) {
        let Some(tier) = tier_from_signal_reason(&signal.reason) else {
            return;
        };
        let state = self.windows.entry(window_number).or_default();
        let usd = signal.amount.max(0.0);
        match tier {
            EndgameTier::Insurance => {
                state.insurance_spent_usd += usd;
                state.insurance_clips += 1;
                if state.insurance_side.is_none() {
                    state.insurance_side = Some(signal.side.clone());
                }
            }
            EndgameTier::Rescue | EndgameTier::FinalSeal => {
                state.rescue_spent_usd += usd;
                state.last_endgame_buy_at = Some(Instant::now());
                if state.primary_side.is_none() {
                    state.primary_side = Some(signal.side.clone());
                }
            }
            EndgameTier::DiscountReload => {
                state.rescue_spent_usd += usd;
                state.discount_reload_spent_usd += usd;
                state.discount_reload_clips += 1;
                state.last_endgame_buy_at = Some(Instant::now());
                if state.primary_side.is_none() {
                    state.primary_side = Some(signal.side.clone());
                }
            }
            EndgameTier::Impulse => {
                state.impulse_spent_usd += usd;
                if state.primary_side.is_none() {
                    state.primary_side = Some(signal.side.clone());
                }
            }
            EndgameTier::Cheap => {
                state.cheap_spent_usd += usd;
                state.cheap_clips += 1;
                if state.primary_side.is_none() {
                    state.primary_side = Some(signal.side.clone());
                }
            }
            EndgameTier::Late => {
                state.late_spent_usd += usd;
                state.late_clips += 1;
                if state.primary_side.is_none() {
                    state.primary_side = Some(signal.side.clone());
                }
            }
            EndgameTier::FlipHedge => {
                state.hedge_spent_usd += usd;
                state.hedge_clips += 1;
            }
        }
        state.clips_filled += 1;
        state.winner_side = Some(signal.side.clone());
    }
}

fn tier_from_signal_reason(reason: &str) -> Option<EndgameTier> {
    if reason.starts_with("j_insurance_") {
        Some(EndgameTier::Insurance)
    } else if reason.starts_with("j_impulse_") {
        Some(EndgameTier::Impulse)
    } else if reason.starts_with("j_value_") {
        Some(EndgameTier::Cheap)
    } else if reason.starts_with("j_late_") {
        Some(EndgameTier::Late)
    } else if reason.starts_with("j_flip_hedge_") {
        Some(EndgameTier::FlipHedge)
    } else if reason.starts_with("j_rescue_") {
        Some(EndgameTier::Rescue)
    } else if reason.starts_with("j_discount_reload_") {
        Some(EndgameTier::DiscountReload)
    } else if reason.starts_with("j_final_seal_") {
        Some(EndgameTier::FinalSeal)
    } else {
        None
    }
}

fn side_book_mut<'a>(side: &str, prices: &'a mut PricesState) -> &'a mut SideBook {
    if side == "UP" {
        &mut prices.up.book
    } else {
        &mut prices.down.book
    }
}

fn side_book<'a>(side: &str, prices: &'a PricesState) -> &'a SideBook {
    if side == "UP" {
        &prices.up.book
    } else {
        &prices.down.book
    }
}

pub(crate) fn side_ask(side: &str, prices: &PricesState) -> f64 {
    if side == "UP" {
        prices.up.ask
    } else {
        prices.down.ask
    }
}

pub(crate) fn side_bid(side: &str, prices: &PricesState) -> f64 {
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

fn j_buy_order_type(_cfg: &crate::config::JEndgameConfig) -> OrderType {
    // Endgame entries: market FOK; live executor prices from fresh CLOB book (no stale cap).
    OrderType::Market
}

fn sell_rescue_order_type(_cfg: &crate::config::JEndgameConfig, _secs_to_end: i64) -> OrderType {
    // Rescue exits must execute immediately; live maps to market FOK.
    OrderType::Market
}

pub(crate) fn winner_side(spot: f64, ptb: f64) -> Option<&'static str> {
    if spot > ptb {
        Some("UP")
    } else if spot < ptb {
        Some("DOWN")
    } else {
        None
    }
}

pub(crate) fn gap_z(spot: f64, ptb: f64, current_atr: f64, secs_to_end: i64) -> f64 {
    let expected = expected_move_usd(current_atr, secs_to_end);
    if expected > 0.0 {
        (spot - ptb) / expected
    } else {
        f64::NAN
    }
}

pub(crate) fn endgame_gate_allows(
    secs_to_end: i64,
    winner_ask: f64,
    abs_gap_z: f64,
    cfg: &crate::config::JEndgameConfig,
) -> bool {
    secs_to_end > 0
        && secs_to_end <= cfg.endgame_secs
        && winner_ask >= cfg.min_winner_ask
        && abs_gap_z >= cfg.min_abs_gap_z.min(cfg.late_min_gap_z)
}

pub(crate) fn window_duration_secs(market: &MarketWindow) -> i64 {
    match (
        chrono::DateTime::parse_from_rfc3339(&market.start_time),
        chrono::DateTime::parse_from_rfc3339(&market.end_time),
    ) {
        (Ok(start), Ok(end)) => ((end.timestamp_millis() - start.timestamp_millis()) / 1000).max(1),
        _ => 300,
    }
}

pub(crate) fn window_elapsed_pct(market: &MarketWindow, secs_to_end: i64) -> f64 {
    let total = window_duration_secs(market);
    ((total - secs_to_end.max(0)) as f64 / total as f64) * 100.0
}

pub(crate) fn ptb_dist_pct(spot: f64, ptb: f64) -> f64 {
    if ptb.abs() > 1e-12 {
        ((spot - ptb) / ptb).abs() * 100.0
    } else {
        f64::NAN
    }
}

pub(crate) fn effective_max_clips(cfg: &crate::config::JEndgameConfig) -> u16 {
    if cfg.max_clips_per_window == 0 {
        u16::MAX
    } else {
        cfg.max_clips_per_window
    }
}

pub(crate) fn capture_endgame_chop_snapshot(
    state: &mut JWindowState,
    cfg: &crate::config::JEndgameConfig,
    mid_cross: &MidCrossSnapshot,
    in_endgame: bool,
) {
    if !in_endgame || state.entry_cross_snapshot.is_some() {
        return;
    }
    state.entry_cross_snapshot = Some((mid_cross.cross_count, mid_cross.significant_cross_count));
    if (cfg.max_crosses_directional > 0 && mid_cross.cross_count >= cfg.max_crosses_directional)
        || (cfg.max_sig_crosses_directional > 0
            && mid_cross.significant_cross_count >= cfg.max_sig_crosses_directional)
    {
        state.directional_blocked_chop = true;
    }
}

pub(crate) fn live_chop_blocks_directional(
    cfg: &crate::config::JEndgameConfig,
    state: &JWindowState,
    mid_cross: &MidCrossSnapshot,
) -> bool {
    if state.directional_blocked_chop {
        return true;
    }
    let (base_cross, base_sig) = state.entry_cross_snapshot.unwrap_or((0, 0));
    let raw_growth = mid_cross.cross_count.saturating_sub(base_cross);
    let sig_growth = mid_cross.significant_cross_count.saturating_sub(base_sig);

    (cfg.max_crosses_directional > 0
        && (mid_cross.cross_count >= cfg.max_crosses_directional
            || raw_growth >= cfg.max_crosses_directional))
        || (cfg.max_sig_crosses_directional > 0
            && (mid_cross.significant_cross_count >= cfg.max_sig_crosses_directional
                || sig_growth >= cfg.max_sig_crosses_directional))
}

pub(crate) fn fresh_cross_freeze_blocks_directional(
    cfg: &crate::config::JEndgameConfig,
    mid_cross: &MidCrossSnapshot,
    elapsed_pct: f64,
    window_secs: i64,
) -> bool {
    if cfg.fresh_cross_freeze_secs <= 0 || !elapsed_pct.is_finite() {
        return false;
    }
    let Some(cross_pct) = mid_cross.last_cross_time_pct else {
        return false;
    };
    if !cross_pct.is_finite() || elapsed_pct < cross_pct {
        return false;
    }
    let freeze_pct = cfg.fresh_cross_freeze_secs as f64 / window_secs.max(1) as f64 * 100.0;
    elapsed_pct <= cross_pct + freeze_pct + 1e-9
}

pub(crate) fn directional_entry_allowed(
    cfg: &crate::config::JEndgameConfig,
    state: &JWindowState,
    min_atr: f64,
    current_atr: f64,
    spot: f64,
    ptb: f64,
) -> bool {
    if state.directional_blocked_chop {
        return false;
    }
    if min_atr > 0.0 && current_atr < min_atr {
        return false;
    }
    let dist = ptb_dist_pct(spot, ptb);
    if cfg.min_ptb_dist_pct > 0.0 && dist.is_finite() && dist < cfg.min_ptb_dist_pct {
        return false;
    }
    true
}

/// Dashboard / external callers: pass chop-block flag from live JWindowState.
pub fn directional_entry_allowed_external(
    cfg: &crate::config::JEndgameConfig,
    chop_blocked: bool,
    min_atr: f64,
    current_atr: f64,
    spot: f64,
    ptb: f64,
) -> bool {
    let state = JWindowState {
        directional_blocked_chop: chop_blocked,
        ..Default::default()
    };
    directional_entry_allowed(cfg, &state, min_atr, current_atr, spot, ptb)
}

pub(crate) fn flip_hedge_triggered(
    cfg: &crate::config::JEndgameConfig,
    state: &JWindowState,
    primary_side: &str,
    current_winner: &str,
    spot: f64,
    ptb: f64,
    gz: f64,
    mid_cross: &MidCrossSnapshot,
) -> bool {
    if !cfg.flip_hedge_enabled || !state.has_primary_exposure() {
        return false;
    }
    let spot_against_primary =
        (primary_side == "UP" && spot < ptb) || (primary_side == "DOWN" && spot > ptb);
    if cfg.flip_require_spot_cross && !spot_against_primary {
        return false;
    }
    let mid_against_primary = mid_cross
        .current_side
        .filter(|s| *s != LeadSide::Tie)
        .map(|s| s.as_str() != primary_side)
        .unwrap_or(false);

    if primary_side == current_winner && !mid_against_primary {
        return false;
    }
    if !spot_against_primary && !mid_against_primary {
        return false;
    }

    let sharp = mid_cross.significant_cross_count >= cfg.flip_min_sig_crosses
        || mid_cross.cross_count >= cfg.flip_min_crosses
        || mid_cross.last_cross_is_significant;

    // Sign-aware gap: gap_z>0 means UP leads, <0 means DOWN leads. The hedge
    // buys the side OPPOSITE `primary_side`, so it is only justified when the
    // time/vol-normalized gap actually leans against our side.
    let gz_against_primary = if primary_side == "UP" { -gz } else { gz };

    if spot_against_primary {
        return gz_against_primary >= cfg.flip_min_gap_z;
    }
    // Legacy mode only: mid lead flipped before spot crossed PTB — require chaos evidence.
    sharp
}

fn plan_sell_rescue_signal(
    cfg: &crate::config::JEndgameConfig,
    state: &JWindowState,
    win_state: &WindowState,
    prices: &PricesState,
    current_winner: &str,
    gz: f64,
    secs_to_end: i64,
    projected_hold_pnl: f64,
) -> Option<OrderSignal> {
    if !cfg.sell_rescue_enabled || state.sell_rescue_done || !state.has_primary_exposure() {
        return None;
    }
    let primary = state.primary_side.as_deref()?;
    if primary == current_winner {
        return None;
    }
    let gz_against_primary = if primary == "UP" { -gz } else { gz };
    if gz_against_primary < cfg.sell_rescue_min_gap_z {
        return None;
    }
    let bid = side_bid(primary, prices);
    if bid < cfg.sell_rescue_min_bid {
        return None;
    }
    let shares = shares_for_side(primary, win_state);
    if shares <= 1e-9 {
        return None;
    }
    let sell_shares = shares * cfg.sell_rescue_fraction.clamp(0.0, 1.0);
    let sell_value = sell_shares * bid;
    if sell_value < cfg.sell_rescue_min_value_usd {
        return None;
    }
    let projected_after_sell = projected_hold_pnl + sell_value;
    let improvement = projected_after_sell - projected_hold_pnl;
    if improvement + 1e-9 < cfg.sell_rescue_min_improvement_usd {
        return None;
    }
    Some(OrderSignal::sell(
        primary,
        sell_rescue_order_type(cfg, secs_to_end),
        sell_shares,
        bid,
        format!(
            "j_sell_rescue_{}_bid_{:.2}_shares_{:.4}_value_{:.2}_gap_z_against_{:+.2}_hold_pnl_{:+.2}_after_sell_{:+.2}",
            primary.to_lowercase(),
            bid,
            sell_shares,
            sell_value,
            gz_against_primary,
            projected_hold_pnl,
            projected_after_sell,
        ),
    ))
}

fn side_direction(side: &str) -> f64 {
    if side == "UP" {
        1.0
    } else {
        -1.0
    }
}

fn adverse_velocity_usd_per_sec(side: &str, spot_signal: SpotSignalSnapshot) -> f64 {
    let velocity = spot_signal
        .smoothed_velocity_usd_per_sec
        .or(spot_signal.raw_velocity_usd_per_sec)
        .unwrap_or(0.0);
    (-velocity * side_direction(side)).max(0.0)
}

fn aligned_velocity_usd_per_sec(side: &str, spot_signal: SpotSignalSnapshot) -> f64 {
    let velocity = spot_signal
        .smoothed_velocity_usd_per_sec
        .or(spot_signal.raw_velocity_usd_per_sec)
        .unwrap_or(0.0);
    velocity * side_direction(side)
}

fn choppy_primary_add_velocity_blocks(
    cfg: &crate::config::JEndgameConfig,
    state: &JWindowState,
    mid_cross: &MidCrossSnapshot,
    tier: EndgameTier,
    side: &str,
    spot_signal: SpotSignalSnapshot,
) -> bool {
    if !matches!(
        tier,
        EndgameTier::Impulse
            | EndgameTier::Cheap
            | EndgameTier::Late
            | EndgameTier::Rescue
            | EndgameTier::DiscountReload
            | EndgameTier::FinalSeal
    ) {
        return false;
    }
    if !crate::j_controller::mid_cross_soft_chop(cfg, mid_cross) {
        return false;
    }
    if !state.has_primary_exposure() || state.primary_side.as_deref() != Some(side) {
        return false;
    }
    let min_aligned_velocity = (cfg.mom_full_vel_usd_per_sec * 0.10).max(0.20);
    aligned_velocity_usd_per_sec(side, spot_signal) + 1e-9 < min_aligned_velocity
}

fn discount_reload_velocity_blocks(
    cfg: &crate::config::JEndgameConfig,
    tier: EndgameTier,
    side: &str,
    gz: f64,
    spot_signal: SpotSignalSnapshot,
) -> bool {
    if tier != EndgameTier::DiscountReload {
        return false;
    }
    let side_gap_z = gz * side_direction(side);
    let shallow_gap = side_gap_z < cfg.full_size_gap_z.max(cfg.discount_reload_min_gap_z + 0.4);
    let strong_adverse_velocity = aligned_velocity_usd_per_sec(side, spot_signal)
        < -((cfg.mom_full_vel_usd_per_sec * 0.45).max(0.75));
    shallow_gap && strong_adverse_velocity
}

fn post_target_primary_add_blocks(
    cfg: &crate::config::JEndgameConfig,
    state: &JWindowState,
    tier: EndgameTier,
    side: &str,
    ask: f64,
    gz: f64,
    spot_signal: SpotSignalSnapshot,
    projected_pnl: f64,
) -> bool {
    if !matches!(tier, EndgameTier::Rescue | EndgameTier::FinalSeal) {
        return false;
    }
    if projected_pnl + 1e-9 < cfg.target_profit_usd {
        return false;
    }
    if !state.has_primary_exposure() || state.primary_side.as_deref() != Some(side) {
        return false;
    }
    let non_cheap_ask = cfg.discount_reload_max_ask.max(0.80);
    if ask + 1e-9 < non_cheap_ask {
        return false;
    }
    let side_gap_z = gz * side_direction(side);
    let min_confirm_velocity = (cfg.mom_full_vel_usd_per_sec * 0.05).max(0.10);
    side_gap_z < cfg.full_size_gap_z
        && aligned_velocity_usd_per_sec(side, spot_signal) < min_confirm_velocity
}

fn tail_add_danger_secs(cfg: &crate::config::JEndgameConfig) -> i64 {
    (cfg.rescue_zone_secs / 2)
        .max(cfg.final_seal_secs)
        .max(cfg.sell_rescue_market_secs)
        .max(1)
}

fn tail_sell_safety_secs(cfg: &crate::config::JEndgameConfig) -> i64 {
    cfg.final_seal_secs.max(cfg.sell_rescue_market_secs).max(1)
}

fn close_to_ptb_for_tail_add(
    cfg: &crate::config::JEndgameConfig,
    spot: f64,
    ptb: f64,
    current_atr: f64,
) -> bool {
    let raw_dist = (spot - ptb).abs();
    let atr_close = current_atr.is_finite()
        && current_atr > 0.0
        && raw_dist <= current_atr * J_TAIL_ADD_DANGER_ATR_MULT;
    let pct = ptb_dist_pct(spot, ptb);
    let pct_close =
        cfg.min_ptb_dist_pct > 0.0 && pct.is_finite() && pct <= cfg.min_ptb_dist_pct * 2.0;
    atr_close || pct_close
}

fn close_to_ptb_for_tail_sell(
    cfg: &crate::config::JEndgameConfig,
    spot: f64,
    ptb: f64,
    current_atr: f64,
) -> bool {
    let raw_dist = (spot - ptb).abs();
    let atr_close = current_atr.is_finite()
        && current_atr > 0.0
        && raw_dist <= current_atr * J_TAIL_SELL_NEAR_ATR_MULT;
    let pct = ptb_dist_pct(spot, ptb);
    let pct_close = cfg.min_ptb_dist_pct > 0.0 && pct.is_finite() && pct <= cfg.min_ptb_dist_pct;
    atr_close || pct_close
}

fn tail_safety_blocks_primary_buy(
    cfg: &crate::config::JEndgameConfig,
    state: &JWindowState,
    tier: EndgameTier,
    side: &str,
    spot: f64,
    ptb: f64,
    current_atr: f64,
    secs_to_end: i64,
) -> bool {
    if !matches!(
        tier,
        EndgameTier::Impulse
            | EndgameTier::Cheap
            | EndgameTier::Late
            | EndgameTier::Rescue
            | EndgameTier::DiscountReload
            | EndgameTier::FinalSeal
    ) {
        return false;
    }
    if secs_to_end <= 0 || secs_to_end > tail_add_danger_secs(cfg) {
        return false;
    }
    if !state.has_primary_exposure() || state.primary_side.as_deref() != Some(side) {
        return false;
    }
    if winner_side(spot, ptb) != Some(side) {
        return false;
    }
    close_to_ptb_for_tail_add(cfg, spot, ptb, current_atr)
}

fn plan_tail_safety_sell_signal(
    cfg: &crate::config::JEndgameConfig,
    state: &JWindowState,
    win_state: &WindowState,
    prices: &PricesState,
    current_winner: &str,
    spot: f64,
    ptb: f64,
    current_atr: f64,
    secs_to_end: i64,
    spot_signal: SpotSignalSnapshot,
    projected_hold_pnl: f64,
    fee_bps: f64,
) -> Option<OrderSignal> {
    if !cfg.sell_rescue_enabled || state.sell_rescue_done || !state.has_primary_exposure() {
        return None;
    }
    let primary = state.primary_side.as_deref()?;
    if primary != current_winner {
        return None;
    }
    if secs_to_end <= 0 || secs_to_end > tail_sell_safety_secs(cfg) {
        return None;
    }
    if !close_to_ptb_for_tail_sell(cfg, spot, ptb, current_atr) {
        return None;
    }
    let adverse_velocity = adverse_velocity_usd_per_sec(primary, spot_signal);
    let min_adverse_velocity = (cfg.mom_full_vel_usd_per_sec * 0.75).max(1.0);
    if adverse_velocity + 1e-9 < min_adverse_velocity {
        return None;
    }
    let bid = side_bid(primary, prices);
    if bid < cfg.sell_rescue_min_bid {
        return None;
    }
    let shares = shares_for_side(primary, win_state);
    if shares <= 1e-9 {
        return None;
    }
    let sell_fraction = cfg
        .sell_rescue_fraction
        .clamp(0.0, J_TAIL_SELL_MAX_FRACTION);
    let sell_shares = shares * sell_fraction;
    let sell_value = sell_shares * bid;
    if sell_value < cfg.sell_rescue_min_value_usd {
        return None;
    }
    let projected_after_sell =
        projected_winner_pnl_after_partial_sell(win_state, primary, sell_shares, bid, fee_bps);
    let cost_vs_hold = (projected_hold_pnl - projected_after_sell).max(0.0);
    Some(OrderSignal::sell(
        primary,
        sell_rescue_order_type(cfg, secs_to_end),
        sell_shares,
        bid,
        format!(
            "j_sell_rescue_tail_{}_bid_{:.2}_shares_{:.4}_value_{:.2}_adverse_v_{:.2}_hold_pnl_{:+.2}_after_hold_{:+.2}_cost_{:.2}",
            primary.to_lowercase(),
            bid,
            sell_shares,
            sell_value,
            adverse_velocity,
            projected_hold_pnl,
            projected_after_sell,
            cost_vs_hold,
        ),
    ))
}

fn projected_winner_pnl_after_partial_sell(
    win_state: &WindowState,
    winner: &str,
    sell_shares: f64,
    bid: f64,
    fee_bps: f64,
) -> f64 {
    let shares = shares_for_side(winner, win_state);
    let sold = sell_shares.clamp(0.0, shares.max(0.0));
    let remaining = (shares - sold).max(0.0);
    let redeem_fee = leg_fee_usd(1.0, remaining, fee_bps);
    win_state.cash_returned + sold * bid + remaining - redeem_fee - win_state.spent
}

impl JWindowState {
    /// Directional thesis is live once any primary-tier USD is deployed.
    /// Composite (FinalSeal/Rescue) writes `rescue_spent_usd`, not `cheap_clips`.
    pub(crate) fn has_primary_exposure(&self) -> bool {
        self.primary_side.is_some()
            && (self.rescue_spent_usd > 1e-9
                || self.cheap_spent_usd > 1e-9
                || self.late_spent_usd > 1e-9
                || self.impulse_spent_usd > 1e-9
                || self.cheap_clips > 0
                || self.late_clips > 0)
    }

    pub(crate) fn primary_exposure_usd(&self) -> f64 {
        self.rescue_spent_usd + self.cheap_spent_usd + self.late_spent_usd + self.impulse_spent_usd
    }

    fn release_primary_exposure_usd(&mut self, recovered_usd: f64) {
        let mut remaining = recovered_usd.max(0.0);
        reduce_usd(&mut self.rescue_spent_usd, &mut remaining);
        reduce_usd(&mut self.cheap_spent_usd, &mut remaining);
        reduce_usd(&mut self.late_spent_usd, &mut remaining);
        reduce_usd(&mut self.impulse_spent_usd, &mut remaining);

        let mut reload_recovery = recovered_usd.max(0.0);
        reduce_usd(&mut self.discount_reload_spent_usd, &mut reload_recovery);
    }
}

fn reduce_usd(bucket: &mut f64, remaining: &mut f64) {
    if *remaining <= 0.0 || *bucket <= 0.0 {
        return;
    }
    let take = (*bucket).min(*remaining);
    *bucket -= take;
    *remaining -= take;
}

pub(crate) fn flip_hedge_armed_display(
    cfg: &crate::config::JEndgameConfig,
    primary_side: Option<&str>,
    current_winner: &str,
    spot: f64,
    ptb: f64,
    gz: f64,
    mid_cross: &MidCrossSnapshot,
) -> bool {
    let Some(primary) = primary_side else {
        return false;
    };
    let state = JWindowState {
        rescue_spent_usd: 1.0,
        primary_side: Some(primary.to_string()),
        ..Default::default()
    };
    flip_hedge_triggered(
        cfg,
        &state,
        primary,
        current_winner,
        spot,
        ptb,
        gz,
        mid_cross,
    )
}

pub(crate) fn tape_hot(
    tape: &TradeTapeSnapshot,
    winner_side: &str,
    cfg: &crate::config::JEndgameConfig,
) -> bool {
    let (usd, count) = TradeTapeTracker::winner_stats(tape, winner_side);
    usd >= cfg.min_tape_usd && count >= cfg.min_tape_buys
}

/// Worst price for an aggressive endgame buy: ask + slippage, capped by taker_max_ask.
pub(crate) fn aggressive_buy_limit_price(
    winner_ask: f64,
    cfg: &crate::config::JEndgameConfig,
) -> f64 {
    (winner_ask + cfg.limit_ask_offset)
        .min(cfg.taker_max_ask)
        .max(cfg.min_winner_ask)
}

pub(crate) fn taker_max_pay(winner_ask: f64, cfg: &crate::config::JEndgameConfig) -> f64 {
    aggressive_buy_limit_price(winner_ask, cfg)
}

pub(crate) fn sweep_endgame_clips(
    side: &str,
    prices: &mut PricesState,
    winner_ask: f64,
    max_pay: f64,
    clip_usd: f64,
    max_clips: u8,
    remaining_budget: f64,
    already_filled: u16,
    max_clips_window: u16,
) -> Vec<(f64, f64)> {
    let mut fills = vec![];
    let mut budget = remaining_budget;
    let mut filled = already_filled;

    for _ in 0..max_clips {
        if budget + 1e-9 < clip_usd || filled >= max_clips_window {
            break;
        }
        let book = side_book_mut(side, prices);
        let fill = if book.asks.is_empty() {
            simulate_taker_buy_fill(&[], winner_ask, max_pay, clip_usd)
        } else {
            apply_fill_to_asks(&mut book.asks, max_pay, clip_usd)
        };
        let Some((shares, avg)) = fill else {
            break;
        };
        let usd = shares * avg;
        if usd < clip_usd * 0.95 {
            break;
        }
        fills.push((avg, usd.min(clip_usd)));
        budget -= clip_usd.min(usd);
        filled += 1;
    }
    fills
}

impl TradeStrategy for JEndgameStrategy {
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
        _spot_signal: SpotSignalSnapshot,
        mid_cross: &MidCrossSnapshot,
        _cex_micro: &CexMicroSnapshot,
        tape: &TradeTapeSnapshot,
    ) -> Vec<OrderSignal> {
        let mut signals = Vec::new();
        let window_number = win_state.window_number;
        if window_number < J_MIN_TRADEABLE_WINDOW {
            return signals;
        }

        let jcfg = &config.j_endgame;
        let fee_bps = jcfg.fee_rate_bps.unwrap_or(DEFAULT_CRYPTO_FEE_RATE_BPS);
        let default_clip = if jcfg.bank_sizing_enabled {
            jcfg.effective_probe_clip_usd(&config.session)
        } else {
            jcfg.clip_usd
                .max(jcfg.effective_probe_clip_usd(&config.session))
        };
        let state = self.windows.entry(window_number).or_insert(JWindowState {
            impulse_spent_usd: 0.0,
            cheap_spent_usd: 0.0,
            late_spent_usd: 0.0,
            hedge_spent_usd: 0.0,
            insurance_spent_usd: 0.0,
            rescue_spent_usd: 0.0,
            discount_reload_spent_usd: 0.0,
            cheap_clips: 0,
            late_clips: 0,
            hedge_clips: 0,
            insurance_clips: 0,
            discount_reload_clips: 0,
            clips_filled: 0,
            primary_side: None,
            insurance_side: None,
            winner_side: None,
            last_endgame_buy_at: None,
            sell_rescue_done: false,
            entry_cross_snapshot: None,
            directional_blocked_chop: false,
        });

        let (Some(spot), Some(ptb)) = (spot_price, market.price_to_beat) else {
            return signals;
        };

        let min_atr = config.min_atr_for(&market.asset);
        let elapsed_pct = window_elapsed_pct(market, secs_to_end);
        let phase = crate::j_controller::detect_phase(elapsed_pct, secs_to_end, jcfg, mid_cross);
        let in_endgame = !matches!(
            phase,
            crate::j_controller::JWindowPhase::Warmup
                | crate::j_controller::JWindowPhase::MidWindow
                | crate::j_controller::JWindowPhase::Insurance
        );
        capture_endgame_chop_snapshot(state, jcfg, mid_cross, in_endgame);

        let Some(current_winner) = winner_side(spot, ptb) else {
            return signals;
        };

        let gz = gap_z(spot, ptb, current_atr, secs_to_end);
        if !gz.is_finite() {
            return signals;
        }

        let window_cap = if config.session.max_window_budget > 0.0 {
            jcfg.effective_max_usd_per_window(&config.session)
                .min(config.session.max_window_budget)
        } else {
            jcfg.effective_max_usd_per_window(&config.session)
        };

        if live_chop_blocks_directional(jcfg, state, mid_cross) {
            state.directional_blocked_chop = true;
        }
        let fresh_cross_freeze = fresh_cross_freeze_blocks_directional(
            jcfg,
            mid_cross,
            elapsed_pct,
            window_duration_secs(market),
        );
        let allow_directional =
            directional_entry_allowed(jcfg, state, min_atr, current_atr, spot, ptb)
                && !fresh_cross_freeze;
        let confidence = crate::j_controller::endgame_confidence(
            jcfg,
            current_winner,
            gz,
            &_spot_signal,
            mid_cross,
            _cex_micro,
            tape,
        );
        let projected_hold_pnl =
            crate::j_controller::projected_redeem_pnl(win_state, current_winner, fee_bps);
        let sell_rescue = plan_sell_rescue_signal(
            jcfg,
            state,
            win_state,
            prices,
            current_winner,
            gz,
            secs_to_end,
            projected_hold_pnl,
        )
        .or_else(|| {
            plan_tail_safety_sell_signal(
                jcfg,
                state,
                win_state,
                prices,
                current_winner,
                spot,
                ptb,
                current_atr,
                secs_to_end,
                _spot_signal,
                projected_hold_pnl,
                fee_bps,
            )
        });

        let plan = crate::j_controller::plan_j_window(
            config,
            state,
            win_state,
            prices,
            spot,
            ptb,
            secs_to_end,
            elapsed_pct,
            current_atr,
            min_atr,
            mid_cross,
            allow_directional,
            confidence,
            self.available_cash,
        );

        let Some(plan) = plan else {
            if let Some(sell) = sell_rescue {
                signals.push(sell);
            }
            return signals;
        };

        if matches!(
            plan.tier,
            EndgameTier::Rescue | EndgameTier::DiscountReload | EndgameTier::FinalSeal
        ) {
            if let Some(last) = state.last_endgame_buy_at {
                if last.elapsed() < std::time::Duration::from_millis(jcfg.min_buy_interval_ms) {
                    return signals;
                }
            }
        }

        let side = plan.side.as_deref().unwrap_or(current_winner);
        let winner_ask = side_ask(side, prices);
        let clip_usd = if plan.clip_usd > 0.0 {
            plan.clip_usd
        } else {
            default_clip
        };
        let projected_pnl = crate::j_controller::projected_redeem_pnl(win_state, side, fee_bps);
        let sell_blocks_primary_buy = sell_rescue
            .as_ref()
            .map(|s| s.side == side && s.reason.starts_with("j_sell_rescue_tail_"))
            .unwrap_or(false);
        if sell_blocks_primary_buy
            || discount_reload_velocity_blocks(jcfg, plan.tier, side, gz, _spot_signal)
            || post_target_primary_add_blocks(
                jcfg,
                state,
                plan.tier,
                side,
                winner_ask,
                gz,
                _spot_signal,
                projected_pnl,
            )
            || choppy_primary_add_velocity_blocks(
                jcfg,
                state,
                mid_cross,
                plan.tier,
                side,
                _spot_signal,
            )
            || tail_safety_blocks_primary_buy(
                jcfg,
                state,
                plan.tier,
                side,
                spot,
                ptb,
                current_atr,
                secs_to_end,
            )
        {
            if let Some(sell) = sell_rescue {
                signals.push(sell);
            }
            return signals;
        }

        if plan.need_tape && !tape_hot(tape, side, jcfg) {
            return signals;
        }

        let max_pay = aggressive_buy_limit_price(winner_ask, jcfg);

        let cheap_tier = matches!(
            plan.tier,
            EndgameTier::Insurance
                | EndgameTier::FlipHedge
                | EndgameTier::Rescue
                | EndgameTier::DiscountReload
                | EndgameTier::FinalSeal
        );
        if max_pay < jcfg.min_winner_ask && !cheap_tier {
            return signals;
        }

        let book = side_book(side, prices);
        if book.asks.is_empty() && winner_ask > max_pay {
            return signals;
        }
        if !book.asks.is_empty() && ask_depth_usd(&book.asks, max_pay) < clip_usd * 0.5 {
            return signals;
        }

        let remaining = if matches!(
            plan.tier,
            EndgameTier::Insurance
                | EndgameTier::FlipHedge
                | EndgameTier::Rescue
                | EndgameTier::DiscountReload
                | EndgameTier::FinalSeal
        ) {
            plan.budget_left
        } else {
            let net_risk = (win_state.spent - win_state.cash_returned).max(0.0);
            plan.budget_left.min((window_cap - net_risk).max(0.0))
        };
        let max_clips_window = if matches!(
            plan.tier,
            EndgameTier::FlipHedge
                | EndgameTier::Rescue
                | EndgameTier::DiscountReload
                | EndgameTier::FinalSeal
        ) {
            u16::MAX
        } else {
            effective_max_clips(jcfg)
        };
        let mut prices_mut = prices.clone();
        let fills = sweep_endgame_clips(
            side,
            &mut prices_mut,
            winner_ask,
            max_pay,
            clip_usd,
            plan.sweep_clips,
            remaining,
            state.clips_filled,
            max_clips_window,
        );

        let (tape_usd, tape_count) = TradeTapeTracker::winner_stats(tape, side);
        let tier_label = match plan.tier {
            EndgameTier::Insurance => "insurance",
            EndgameTier::Impulse => "impulse",
            EndgameTier::Cheap => "value",
            EndgameTier::Late => "late",
            EndgameTier::FlipHedge => "flip_hedge",
            EndgameTier::Rescue => "rescue",
            EndgameTier::DiscountReload => "discount_reload",
            EndgameTier::FinalSeal => "final_seal",
        };
        let mode = "limit";

        if let Some(sell) = sell_rescue {
            signals.push(sell);
        }
        let min_buy_usd = jcfg.effective_probe_clip_usd(&config.session).max(1.0);
        for (fill_price, usd) in fills {
            if usd + 1e-9 < min_buy_usd {
                continue;
            }
            signals.push(OrderSignal::buy(
                side,
                j_buy_order_type(jcfg),
                usd,
                max_pay,
                format!(
                    "j_{}_{}_{}_fill_{:.2}_ask_{:.2}_gap_z_{:+.2}_phase_{}_pnl_proj_{:+.2}_tape_${:.0}/{}_xc{}",
                    tier_label,
                    mode,
                    side.to_lowercase(),
                    fill_price,
                    winner_ask,
                    gz,
                    phase.label(),
                    projected_pnl,
                    tape_usd,
                    tape_count,
                    mid_cross.cross_count,
                ),
            ));
        }

        signals
    }

    fn get_strategy_state(&self, window_number: usize) -> Option<StrategyState> {
        self.windows.get(&window_number).map(|s| StrategyState {
            up_sold: false,
            down_sold: false,
            first_sold_side: None,
            ptb_crossed: false,
            ptb_baseline: None,
            e_conviction_side: s.primary_side.clone(),
            e_tranches_done: s.clips_filled.min(255) as u8,
            e_grid_steps_done: 0,
            h_entry_side: s.primary_side.clone(),
            h_entry_done: s.clips_filled > 0,
            h_salvage_done: false,
        })
    }

    fn set_runtime_cash(&mut self, cash: f64) {
        self.available_cash = cash.max(0.0);
    }

    fn j_directional_blocked(&self, window_number: usize) -> bool {
        self.windows
            .get(&window_number)
            .map(|s| s.directional_blocked_chop)
            .unwrap_or(false)
    }

    fn notify_order_executed(&mut self, window_number: usize, signal: &OrderSignal) {
        match signal.operation() {
            OrderOperation::Buy => self.mark_buy_executed(window_number, signal),
            OrderOperation::Sell => {
                if signal.reason.starts_with("j_sell_rescue") {
                    self.mark_sell_rescue_executed(window_number, signal);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::ContractPrices;
    use crate::config::{
        Config, JEndgameConfig, PreStartConfig, SellStrategyConfig, SessionConfig,
    };
    use crate::orderbook::BookLevel;

    fn test_config() -> Config {
        Config {
            strategy: "j_endgame".to_string(),
            llm: Default::default(),
            min_btc_atr: 0.0,
            min_eth_atr: 0.0,
            session: SessionConfig {
                starting_bank: 500.0,
                min_window_budget: 30.0,
                max_window_budget: 500.0,
                window_budget_pct: 10.0,
            },
            pre_start_entry: PreStartConfig {
                enabled: false,
                min_seconds_before_start: 5,
                max_seconds_before_start: 120,
                min_side_ask: 0.42,
                max_side_ask: 0.58,
            },
            sell_strategy: SellStrategyConfig { exit_bid: 0.65 },
            asymmetric_ladder: None,
            dynamic_breakeven: None,
            exit_before_end_seconds: 25,
            force_close_at_end: false,
            execution: Default::default(),
            j_endgame: JEndgameConfig::default(),
        }
    }

    fn strat_with_cash() -> JEndgameStrategy {
        let mut s = JEndgameStrategy::new();
        s.set_runtime_cash(500.0);
        s
    }

    fn sample_market() -> MarketWindow {
        let end = chrono::Utc::now() + chrono::Duration::seconds(60);
        let start = end - chrono::Duration::seconds(300);
        MarketWindow {
            id: "t".to_string(),
            slug: "btc-updown-5m-test".to_string(),
            question: "t".to_string(),
            asset: "BTC".to_string(),
            interval: "5m".to_string(),
            start_time: start.to_rfc3339(),
            end_time: end.to_rfc3339(),
            price_to_beat: Some(60_000.0),
            tokens: crate::client::TokensMap {
                up: crate::client::TokenInfo {
                    token_id: "u".to_string(),
                    outcome_name: "Up".to_string(),
                },
                down: crate::client::TokenInfo {
                    token_id: "d".to_string(),
                    outcome_name: "Down".to_string(),
                },
            },
        }
    }

    fn hot_tape() -> TradeTapeSnapshot {
        TradeTapeSnapshot {
            up_buy_usd: 12.0,
            up_buy_count: 4,
            window_ms: 5000,
            ..Default::default()
        }
    }

    fn hot_down_tape() -> TradeTapeSnapshot {
        TradeTapeSnapshot {
            down_buy_usd: 900.0,
            down_buy_count: 40,
            window_ms: 5000,
            ..Default::default()
        }
    }

    #[test]
    fn composite_fires_on_strong_consensus() {
        let mut strat = strat_with_cash();
        let prices = PricesState {
            up: ContractPrices {
                bid: 0.87,
                ask: 0.88,
                book: SideBook {
                    asks: vec![
                        BookLevel {
                            price: 0.88,
                            size: 50.0,
                        },
                        BookLevel {
                            price: 0.88,
                            size: 50.0,
                        },
                    ],
                    ..Default::default()
                },
            },
            down: ContractPrices::top(0.12, 0.13),
        };
        let win = WindowState {
            window_number: 1,
            role: "CURRENT".to_string(),
            status: "LIVE".to_string(),
            market: sample_market(),
            spent: 0.0,
            cash_returned: 0.0,
            up_shares: 0.0,
            down_shares: 0.0,
            initial_up_shares: 0.0,
            initial_down_shares: 0.0,
            trades: vec![],
            prices: prices.clone(),
        };
        // All signals agree the winner (UP) holds: book leads UP, spot momentum
        // up, tape + CEX flow up => composite confidence high => buys.
        let mid = MidCrossSnapshot {
            armed: true,
            current_side: Some(LeadSide::Up),
            lead_gap: 0.20,
            ..Default::default()
        };
        let spot_sig = SpotSignalSnapshot {
            smoothed_velocity_usd_per_sec: Some(3.0),
            ..Default::default()
        };
        let cex = CexMicroSnapshot {
            buy_sell_imbalance_3s: 1.0,
            ..Default::default()
        };
        let signals = strat.process_live_tick(
            &test_config(),
            &prices,
            Some(60_100.0),
            &win.market,
            &win,
            60,
            40.0,
            spot_sig,
            &mid,
            &cex,
            &hot_tape(),
        );
        assert!(!signals.is_empty());
        assert!(
            signals[0].reason.starts_with("j_final_seal"),
            "reason={}",
            signals[0].reason
        );
    }

    #[test]
    fn j_state_updates_only_after_confirmed_execution() {
        let mut strat = strat_with_cash();
        let prices = PricesState {
            up: ContractPrices {
                bid: 0.87,
                ask: 0.88,
                book: SideBook {
                    asks: vec![BookLevel {
                        price: 0.88,
                        size: 50.0,
                    }],
                    ..Default::default()
                },
            },
            down: ContractPrices::top(0.12, 0.13),
        };
        let win = WindowState {
            window_number: 1,
            role: "CURRENT".to_string(),
            status: "LIVE".to_string(),
            market: sample_market(),
            spent: 0.0,
            cash_returned: 0.0,
            up_shares: 0.0,
            down_shares: 0.0,
            initial_up_shares: 0.0,
            initial_down_shares: 0.0,
            trades: vec![],
            prices: prices.clone(),
        };
        let mid = MidCrossSnapshot {
            armed: true,
            current_side: Some(LeadSide::Up),
            lead_gap: 0.20,
            ..Default::default()
        };
        let signals = strat.process_live_tick(
            &test_config(),
            &prices,
            Some(60_100.0),
            &win.market,
            &win,
            60,
            40.0,
            SpotSignalSnapshot {
                smoothed_velocity_usd_per_sec: Some(3.0),
                ..Default::default()
            },
            &mid,
            &CexMicroSnapshot {
                buy_sell_imbalance_3s: 1.0,
                ..Default::default()
            },
            &hot_tape(),
        );
        assert!(!signals.is_empty());

        let before = strat.windows.get(&1).expect("state exists");
        assert_eq!(before.clips_filled, 0);
        assert!(before.primary_side.is_none());
        assert_eq!(before.rescue_spent_usd, 0.0);

        strat.notify_order_executed(1, &signals[0]);
        let after = strat.windows.get(&1).expect("state exists");
        assert_eq!(after.clips_filled, 1);
        assert_eq!(after.primary_side.as_deref(), Some("UP"));
        assert!(after.rescue_spent_usd > 0.0);
    }

    #[test]
    fn late_no_entry_without_tape() {
        let mut strat = strat_with_cash();
        let prices = PricesState {
            up: ContractPrices {
                bid: 0.95,
                ask: 0.96,
                book: SideBook {
                    asks: vec![BookLevel {
                        price: 0.96,
                        size: 20.0,
                    }],
                    ..Default::default()
                },
            },
            down: ContractPrices::top(0.03, 0.04),
        };
        let win = WindowState {
            window_number: 1,
            role: "CURRENT".to_string(),
            status: "LIVE".to_string(),
            market: sample_market(),
            spent: 0.0,
            cash_returned: 0.0,
            up_shares: 0.0,
            down_shares: 0.0,
            initial_up_shares: 0.0,
            initial_down_shares: 0.0,
            trades: vec![],
            prices: prices.clone(),
        };
        // Expensive ask + weak gap_z near PTB => no fresh composite entry.
        let signals = strat.process_live_tick(
            &test_config(),
            &prices,
            Some(60_020.0),
            &win.market,
            &win,
            40,
            40.0,
            SpotSignalSnapshot::default(),
            &MidCrossSnapshot::default(),
            &CexMicroSnapshot::default(),
            &TradeTapeSnapshot::default(),
        );
        assert!(signals.is_empty());
    }

    #[test]
    fn composite_skips_without_consensus() {
        let mut strat = strat_with_cash();
        let prices = PricesState {
            up: ContractPrices {
                bid: 0.95,
                ask: 0.96,
                book: SideBook {
                    asks: vec![BookLevel {
                        price: 0.96,
                        size: 20.0,
                    }],
                    ..Default::default()
                },
            },
            down: ContractPrices::top(0.03, 0.04),
        };
        let win = WindowState {
            window_number: 1,
            role: "CURRENT".to_string(),
            status: "LIVE".to_string(),
            market: sample_market(),
            spent: 0.0,
            cash_returned: 0.0,
            up_shares: 0.0,
            down_shares: 0.0,
            initial_up_shares: 0.0,
            initial_down_shares: 0.0,
            trades: vec![],
            prices: prices.clone(),
        };
        // gap_z alone near PTB on an expensive ask is blocked (no coin-flip @96¢).
        let gap_only = strat.process_live_tick(
            &test_config(),
            &prices,
            Some(60_030.0),
            &win.market,
            &win,
            22,
            40.0,
            SpotSignalSnapshot::default(),
            &MidCrossSnapshot::default(),
            &CexMicroSnapshot::default(),
            &TradeTapeSnapshot::default(),
        );
        assert!(gap_only.is_empty());
        // Hot tape alone does not bypass the expensive-ask gap floor either.
        let tape_only = strat.process_live_tick(
            &test_config(),
            &prices,
            Some(60_030.0),
            &win.market,
            &win,
            22,
            40.0,
            SpotSignalSnapshot::default(),
            &MidCrossSnapshot::default(),
            &CexMicroSnapshot::default(),
            &hot_tape(),
        );
        assert!(tape_only.is_empty());
    }

    #[test]
    fn impulse_blocked_before_half_window() {
        let mut strat = strat_with_cash();
        let mut cfg = test_config();
        cfg.j_endgame.impulse_enabled = true;
        cfg.j_endgame.impulse_tier_usd = 9.0;
        let prices = PricesState {
            up: ContractPrices {
                bid: 0.89,
                ask: 0.91,
                book: SideBook {
                    asks: vec![BookLevel {
                        price: 0.91,
                        size: 30.0,
                    }],
                    ..Default::default()
                },
            },
            down: ContractPrices::top(0.08, 0.09),
        };
        let win = WindowState {
            window_number: 1,
            role: "CURRENT".to_string(),
            status: "LIVE".to_string(),
            market: sample_market(),
            spent: 0.0,
            cash_returned: 0.0,
            up_shares: 0.0,
            down_shares: 0.0,
            initial_up_shares: 0.0,
            initial_down_shares: 0.0,
            trades: vec![],
            prices: prices.clone(),
        };
        // 145s left = ~51% elapsed — impulse should NOT fire (needs 2nd half + endgame rules).
        let signals = strat.process_live_tick(
            &cfg,
            &prices,
            Some(60_030.0),
            &win.market,
            &win,
            145,
            40.0,
            SpotSignalSnapshot::default(),
            &MidCrossSnapshot::default(),
            &CexMicroSnapshot::default(),
            &hot_tape(),
        );
        assert!(signals.is_empty());
    }

    #[test]
    fn flip_hedge_buys_opposite_on_sharp_reversal() {
        let mut strat = strat_with_cash();
        let cfg = test_config();
        let prices = PricesState {
            up: ContractPrices {
                bid: 0.44,
                ask: 0.46,
                book: SideBook {
                    asks: vec![BookLevel {
                        price: 0.46,
                        size: 50.0,
                    }],
                    ..Default::default()
                },
            },
            down: ContractPrices {
                bid: 0.52,
                ask: 0.54,
                book: SideBook {
                    asks: vec![BookLevel {
                        price: 0.54,
                        size: 50.0,
                    }],
                    ..Default::default()
                },
            },
        };
        let win = WindowState {
            window_number: 1,
            role: "CURRENT".to_string(),
            status: "LIVE".to_string(),
            market: sample_market(),
            spent: 0.0,
            cash_returned: 0.0,
            up_shares: 0.0,
            down_shares: 0.0,
            initial_up_shares: 0.0,
            initial_down_shares: 0.0,
            trades: vec![],
            prices: prices.clone(),
        };
        // Seed primary UP position in state via a cheap fill first.
        let mut mid = MidCrossSnapshot::default();
        mid.armed = true;
        mid.cross_count = 8;
        mid.significant_cross_count = 2;
        mid.last_cross_is_significant = true;
        // Spot well below PTB => DOWN winner; genuine reversal vs UP primary.
        strat.windows.insert(
            1,
            JWindowState {
                cheap_spent_usd: 9.0,
                cheap_clips: 3,
                clips_filled: 3,
                primary_side: Some("UP".to_string()),
                ..Default::default()
            },
        );
        let gz = gap_z(58_500.0, 60_000.0, 40.0, 12);
        assert!(
            flip_hedge_triggered(
                &cfg.j_endgame,
                strat.windows.get(&1).unwrap(),
                "UP",
                "DOWN",
                58_500.0,
                60_000.0,
                gz,
                &mid,
            ),
            "flip hedge must trigger on spot reversal with gap against primary"
        );
        let signals = strat.process_live_tick(
            &cfg,
            &prices,
            Some(58_500.0),
            &win.market,
            &win,
            12,
            40.0,
            SpotSignalSnapshot::default(),
            &mid,
            &CexMicroSnapshot::default(),
            &TradeTapeSnapshot::default(),
        );
        assert!(!signals.is_empty());
        assert_eq!(signals[0].side, "DOWN");
        assert!(
            signals[0].reason.starts_with("j_rescue")
                || signals[0].reason.starts_with("j_flip_hedge")
        );
    }

    #[test]
    fn tail_safety_blocks_late_primary_add_near_ptb() {
        let mut strat = strat_with_cash();
        let cfg = test_config();
        let prices = PricesState {
            up: ContractPrices::top(0.21, 0.31),
            down: ContractPrices {
                bid: 0.69,
                ask: 0.79,
                book: SideBook {
                    asks: vec![BookLevel {
                        price: 0.79,
                        size: 50.0,
                    }],
                    ..Default::default()
                },
            },
        };
        let mut win = WindowState {
            window_number: 1,
            role: "CURRENT".to_string(),
            status: "LIVE".to_string(),
            market: sample_market(),
            spent: 4.7321,
            cash_returned: 0.0,
            up_shares: 0.0,
            down_shares: 5.4619,
            initial_up_shares: 0.0,
            initial_down_shares: 0.0,
            trades: vec![],
            prices: prices.clone(),
        };
        win.market.price_to_beat = Some(60_000.0);
        strat.windows.insert(
            1,
            JWindowState {
                rescue_spent_usd: 4.7321,
                clips_filled: 3,
                primary_side: Some("DOWN".to_string()),
                ..Default::default()
            },
        );
        let mid = MidCrossSnapshot {
            armed: true,
            current_side: Some(LeadSide::Down),
            lead_gap: 0.20,
            ..Default::default()
        };
        let signals = strat.process_live_tick(
            &cfg,
            &prices,
            Some(59_950.0),
            &win.market,
            &win,
            7,
            40.0,
            SpotSignalSnapshot {
                smoothed_velocity_usd_per_sec: Some(-1.0),
                ..Default::default()
            },
            &mid,
            &CexMicroSnapshot {
                buy_sell_imbalance_3s: -1.0,
                ..Default::default()
            },
            &hot_down_tape(),
        );

        assert!(
            signals.is_empty(),
            "last-second primary add near PTB must be blocked, signals={signals:?}"
        );
    }

    #[test]
    fn tail_safety_does_not_dump_small_winner_on_weak_reversal() {
        let mut strat = strat_with_cash();
        let cfg = test_config();
        let prices = PricesState {
            up: ContractPrices::top(0.78, 0.79),
            down: ContractPrices::top(0.21, 0.22),
        };
        let mut win = WindowState {
            window_number: 1,
            role: "CURRENT".to_string(),
            status: "LIVE".to_string(),
            market: sample_market(),
            spent: 1.6000,
            cash_returned: 0.0,
            up_shares: 1.64948454,
            down_shares: 0.0,
            initial_up_shares: 0.0,
            initial_down_shares: 0.0,
            trades: vec![],
            prices: prices.clone(),
        };
        win.market.price_to_beat = Some(64_103.1432);
        strat.windows.insert(
            1,
            JWindowState {
                rescue_spent_usd: 1.6000,
                clips_filled: 1,
                primary_side: Some("UP".to_string()),
                ..Default::default()
            },
        );

        let signals = strat.process_live_tick(
            &cfg,
            &prices,
            Some(64_107.1259),
            &win.market,
            &win,
            5,
            24.0786,
            SpotSignalSnapshot {
                smoothed_velocity_usd_per_sec: Some(-0.598539),
                ..Default::default()
            },
            &MidCrossSnapshot::default(),
            &CexMicroSnapshot::default(),
            &TradeTapeSnapshot::default(),
        );

        assert!(
            signals.is_empty(),
            "#27-like tiny winner should not be fully dumped, signals={signals:?}"
        );
    }

    #[test]
    fn tail_safety_sells_primary_before_cross_when_velocity_against() {
        let mut strat = strat_with_cash();
        let cfg = test_config();
        let prices = PricesState {
            up: ContractPrices::top(0.48, 0.50),
            down: ContractPrices::top(0.52, 0.55),
        };
        let mut win = WindowState {
            window_number: 1,
            role: "CURRENT".to_string(),
            status: "LIVE".to_string(),
            market: sample_market(),
            spent: 8.0,
            cash_returned: 0.0,
            up_shares: 0.0,
            down_shares: 10.0,
            initial_up_shares: 0.0,
            initial_down_shares: 0.0,
            trades: vec![],
            prices: prices.clone(),
        };
        win.market.price_to_beat = Some(60_000.0);
        strat.windows.insert(
            1,
            JWindowState {
                rescue_spent_usd: 8.0,
                clips_filled: 3,
                primary_side: Some("DOWN".to_string()),
                ..Default::default()
            },
        );

        let signals = strat.process_live_tick(
            &cfg,
            &prices,
            Some(59_998.5),
            &win.market,
            &win,
            1,
            40.0,
            SpotSignalSnapshot {
                smoothed_velocity_usd_per_sec: Some(2.5),
                ..Default::default()
            },
            &MidCrossSnapshot::default(),
            &CexMicroSnapshot::default(),
            &TradeTapeSnapshot::default(),
        );

        assert_eq!(signals.len(), 1, "signals={signals:?}");
        assert!(!signals[0].is_buy, "tail safety must SELL, got {signals:?}");
        assert_eq!(signals[0].side, "DOWN");
        assert!(
            signals[0].reason.starts_with("j_sell_rescue_tail_down"),
            "reason={}",
            signals[0].reason
        );
    }

    #[test]
    fn partial_winner_sell_projection_counts_lost_redeem() {
        let mut win = WindowState {
            window_number: 1,
            role: "CURRENT".to_string(),
            status: "LIVE".to_string(),
            market: sample_market(),
            spent: 1.6000,
            cash_returned: 0.0,
            up_shares: 1.64948454,
            down_shares: 0.0,
            initial_up_shares: 0.0,
            initial_down_shares: 0.0,
            trades: vec![],
            prices: PricesState {
                up: ContractPrices::top(0.78, 0.79),
                down: ContractPrices::top(0.21, 0.22),
            },
        };
        win.market.price_to_beat = Some(64_103.1432);

        let after_half =
            projected_winner_pnl_after_partial_sell(&win, "UP", win.up_shares * 0.5, 0.78, 0.0);
        let hold = win.up_shares - win.spent;

        assert!((hold - 0.04948454).abs() < 1e-8);
        assert!((after_half - (-0.1319587594)).abs() < 1e-8);
    }

    #[test]
    fn tape_hot_respects_thresholds() {
        let cfg = JEndgameConfig::default();
        let tape = TradeTapeSnapshot {
            up_buy_usd: 6.0,
            up_buy_count: 2,
            ..Default::default()
        };
        assert!(tape_hot(&tape, "UP", &cfg));
    }

    #[test]
    fn blocks_directional_when_ptb_dist_tiny() {
        let cfg = test_config();
        let open = JWindowState::default();
        assert!(!directional_entry_allowed(
            &cfg.j_endgame,
            &open,
            0.0,
            40.0,
            60_020.0,
            60_000.0,
        ));
        assert!(directional_entry_allowed(
            &cfg.j_endgame,
            &open,
            0.0,
            40.0,
            60_100.0,
            60_000.0,
        ));
    }

    #[test]
    fn blocks_directional_when_atr_too_low() {
        let mut cfg = test_config();
        cfg.min_btc_atr = 20.0;
        assert!(!directional_entry_allowed(
            &cfg.j_endgame,
            &JWindowState::default(),
            cfg.min_btc_atr,
            10.0,
            60_100.0,
            60_000.0,
        ));
    }

    #[test]
    fn chop_gate_uses_endgame_snapshot_not_later_crosses() {
        let mut cfg = test_config();
        cfg.j_endgame.max_crosses_directional = 9;
        let mut state = JWindowState::default();
        let calm = MidCrossSnapshot {
            cross_count: 2,
            significant_cross_count: 0,
            ..Default::default()
        };
        capture_endgame_chop_snapshot(&mut state, &cfg.j_endgame, &calm, true);
        assert!(!state.directional_blocked_chop);
        assert!(directional_entry_allowed(
            &cfg.j_endgame,
            &state,
            0.0,
            40.0,
            60_100.0,
            60_000.0
        ));
        // Later chop growth must not block — snapshot was calm at endgame open.
        state.directional_blocked_chop = false;
        assert!(directional_entry_allowed(
            &cfg.j_endgame,
            &state,
            0.0,
            40.0,
            60_100.0,
            60_000.0
        ));
        let mut chop = JWindowState::default();
        let noisy = MidCrossSnapshot {
            cross_count: 10,
            significant_cross_count: 2,
            ..Default::default()
        };
        capture_endgame_chop_snapshot(&mut chop, &cfg.j_endgame, &noisy, true);
        assert!(chop.directional_blocked_chop);
        assert!(!directional_entry_allowed(
            &cfg.j_endgame,
            &chop,
            0.0,
            40.0,
            60_100.0,
            60_000.0
        ));
    }

    #[test]
    fn late_chop_growth_blocks_directional_composite() {
        let mut cfg = test_config();
        cfg.j_endgame.max_crosses_directional = 9;
        cfg.j_endgame.max_sig_crosses_directional = 3;
        let mut state = JWindowState::default();
        let calm = MidCrossSnapshot {
            cross_count: 1,
            significant_cross_count: 0,
            ..Default::default()
        };
        capture_endgame_chop_snapshot(&mut state, &cfg.j_endgame, &calm, true);
        assert!(!state.directional_blocked_chop);
        assert!(!live_chop_blocks_directional(&cfg.j_endgame, &state, &calm));

        let late_chop = MidCrossSnapshot {
            cross_count: 10,
            significant_cross_count: 3,
            last_cross_is_significant: true,
            ..Default::default()
        };
        assert!(live_chop_blocks_directional(
            &cfg.j_endgame,
            &state,
            &late_chop
        ));
    }

    #[test]
    fn choppy_primary_add_requires_velocity_alignment() {
        let mut cfg = test_config();
        cfg.j_endgame.max_crosses_directional = 8;
        cfg.j_endgame.max_sig_crosses_directional = 4;
        cfg.j_endgame.mom_full_vel_usd_per_sec = 2.0;
        let state = JWindowState {
            rescue_spent_usd: 1.6,
            primary_side: Some("UP".to_string()),
            ..Default::default()
        };
        let soft_chop = MidCrossSnapshot {
            cross_count: 5,
            significant_cross_count: 1,
            ..Default::default()
        };
        let weak_velocity = SpotSignalSnapshot {
            smoothed_velocity_usd_per_sec: Some(0.05),
            ..Default::default()
        };
        assert!(choppy_primary_add_velocity_blocks(
            &cfg.j_endgame,
            &state,
            &soft_chop,
            EndgameTier::FinalSeal,
            "UP",
            weak_velocity
        ));

        let aligned_velocity = SpotSignalSnapshot {
            smoothed_velocity_usd_per_sec: Some(0.30),
            ..Default::default()
        };
        assert!(!choppy_primary_add_velocity_blocks(
            &cfg.j_endgame,
            &state,
            &soft_chop,
            EndgameTier::FinalSeal,
            "UP",
            aligned_velocity
        ));
        assert!(!choppy_primary_add_velocity_blocks(
            &cfg.j_endgame,
            &state,
            &MidCrossSnapshot::default(),
            EndgameTier::FinalSeal,
            "UP",
            weak_velocity
        ));
    }

    #[test]
    fn discount_reload_blocks_shallow_gap_when_velocity_sharply_against() {
        let mut cfg = test_config();
        cfg.j_endgame.full_size_gap_z = 1.8;
        cfg.j_endgame.discount_reload_min_gap_z = 1.10;
        cfg.j_endgame.mom_full_vel_usd_per_sec = 2.0;

        assert!(discount_reload_velocity_blocks(
            &cfg.j_endgame,
            EndgameTier::DiscountReload,
            "DOWN",
            -1.21,
            SpotSignalSnapshot {
                smoothed_velocity_usd_per_sec: Some(0.95),
                ..Default::default()
            },
        ));
    }

    #[test]
    fn discount_reload_allows_deep_gap_despite_adverse_velocity() {
        let mut cfg = test_config();
        cfg.j_endgame.full_size_gap_z = 1.8;
        cfg.j_endgame.discount_reload_min_gap_z = 1.10;
        cfg.j_endgame.mom_full_vel_usd_per_sec = 2.0;

        assert!(!discount_reload_velocity_blocks(
            &cfg.j_endgame,
            EndgameTier::DiscountReload,
            "UP",
            5.48,
            SpotSignalSnapshot {
                smoothed_velocity_usd_per_sec: Some(-1.12),
                ..Default::default()
            },
        ));
    }

    #[test]
    fn post_target_primary_add_requires_stronger_confirmation() {
        let mut cfg = test_config();
        cfg.j_endgame.target_profit_usd = 1.0;
        cfg.j_endgame.full_size_gap_z = 1.8;
        cfg.j_endgame.mom_full_vel_usd_per_sec = 2.0;
        cfg.j_endgame.discount_reload_max_ask = 0.74;
        let state = JWindowState {
            rescue_spent_usd: 6.0,
            primary_side: Some("DOWN".to_string()),
            ..Default::default()
        };

        assert!(post_target_primary_add_blocks(
            &cfg.j_endgame,
            &state,
            EndgameTier::FinalSeal,
            "DOWN",
            0.84,
            -1.55,
            SpotSignalSnapshot {
                smoothed_velocity_usd_per_sec: Some(-0.06),
                ..Default::default()
            },
            1.29,
        ));
        assert!(!post_target_primary_add_blocks(
            &cfg.j_endgame,
            &state,
            EndgameTier::FinalSeal,
            "DOWN",
            0.84,
            -1.88,
            SpotSignalSnapshot {
                smoothed_velocity_usd_per_sec: Some(-0.06),
                ..Default::default()
            },
            1.05,
        ));
        assert!(!post_target_primary_add_blocks(
            &cfg.j_endgame,
            &state,
            EndgameTier::FinalSeal,
            "DOWN",
            0.78,
            -1.50,
            SpotSignalSnapshot {
                smoothed_velocity_usd_per_sec: Some(-0.06),
                ..Default::default()
            },
            1.05,
        ));
    }

    #[test]
    fn fresh_cross_freeze_is_temporary_directional_only() {
        let mut cfg = test_config();
        cfg.j_endgame.fresh_cross_freeze_secs = 9;
        let mid = MidCrossSnapshot {
            last_cross_time_pct: Some(70.0),
            ..Default::default()
        };
        assert!(fresh_cross_freeze_blocks_directional(
            &cfg.j_endgame,
            &mid,
            72.0,
            300
        ));
        assert!(!fresh_cross_freeze_blocks_directional(
            &cfg.j_endgame,
            &mid,
            74.0,
            300
        ));

        let state = JWindowState::default();
        assert!(
            directional_entry_allowed(&cfg.j_endgame, &state, 0.0, 40.0, 60_100.0, 60_000.0),
            "freeze should be composed by caller, not persisted as chop state"
        );
    }

    #[test]
    fn flip_hedge_ignores_mid_lead_before_spot_cross() {
        let mut strat = strat_with_cash();
        let mut cfg = test_config();
        cfg.j_endgame.sell_rescue_enabled = false;
        let prices = PricesState {
            up: ContractPrices {
                bid: 0.52,
                ask: 0.54,
                book: SideBook {
                    asks: vec![BookLevel {
                        price: 0.54,
                        size: 50.0,
                    }],
                    ..Default::default()
                },
            },
            down: ContractPrices {
                bid: 0.44,
                ask: 0.46,
                book: SideBook {
                    asks: vec![BookLevel {
                        price: 0.46,
                        size: 50.0,
                    }],
                    ..Default::default()
                },
            },
        };
        let win = WindowState {
            window_number: 1,
            role: "CURRENT".to_string(),
            status: "LIVE".to_string(),
            market: sample_market(),
            spent: 0.0,
            cash_returned: 0.0,
            up_shares: 0.0,
            down_shares: 0.0,
            initial_up_shares: 0.0,
            initial_down_shares: 0.0,
            trades: vec![],
            prices: prices.clone(),
        };
        let mut mid = MidCrossSnapshot::default();
        mid.armed = true;
        mid.current_side = Some(LeadSide::Up);
        mid.cross_count = 8;
        mid.significant_cross_count = 3;
        assert!(
            !flip_hedge_triggered(
                &cfg.j_endgame,
                &JWindowState {
                    cheap_spent_usd: 9.0,
                    cheap_clips: 3,
                    primary_side: Some("DOWN".to_string()),
                    ..Default::default()
                },
                "DOWN",
                "DOWN",
                59_970.0,
                60_000.0,
                -1.0,
                &mid,
            ),
            "mid-only flip should not buy a hedge while spot/PTB still supports primary"
        );
        strat.windows.insert(
            1,
            JWindowState {
                cheap_spent_usd: 9.0,
                cheap_clips: 3,
                clips_filled: 3,
                primary_side: Some("DOWN".to_string()),
                ..Default::default()
            },
        );
        // Spot still below PTB (DOWN winner) but mid lead flipped UP.
        let signals = strat.process_live_tick(
            &cfg,
            &prices,
            Some(59_970.0),
            &win.market,
            &win,
            12,
            40.0,
            SpotSignalSnapshot::default(),
            &mid,
            &CexMicroSnapshot::default(),
            &TradeTapeSnapshot::default(),
        );
        assert!(
            signals
                .iter()
                .all(|s| !(s.side == "UP" && s.reason.starts_with("j_flip_hedge"))),
            "mid-only flip must not emit opposite hedge: {:?}",
            signals
        );
    }

    #[test]
    fn flip_hedge_triggers_after_composite_final_seal() {
        let mut cfg = test_config();
        cfg.j_endgame.sell_rescue_enabled = false;
        let state = JWindowState {
            rescue_spent_usd: 72.0,
            primary_side: Some("DOWN".to_string()),
            clips_filled: 4,
            ..Default::default()
        };
        assert!(
            state.has_primary_exposure(),
            "composite path must count as primary exposure"
        );
        let cheap_only = JWindowState {
            cheap_clips: 0,
            late_clips: 0,
            rescue_spent_usd: 0.0,
            primary_side: Some("DOWN".to_string()),
            ..Default::default()
        };
        assert!(
            !cheap_only.has_primary_exposure(),
            "primary_side alone is not enough without deployed USD"
        );

        let mid = MidCrossSnapshot {
            armed: true,
            cross_count: 8,
            significant_cross_count: 3,
            last_cross_is_significant: true,
            ..Default::default()
        };
        // w14-like: committed DOWN, spot crossed above PTB, sharp chop.
        assert!(flip_hedge_triggered(
            &cfg.j_endgame,
            &state,
            "DOWN",
            "UP",
            62_585.0,
            62_572.0,
            0.65,
            &mid,
        ));

        let mut strat = strat_with_cash();
        strat.windows.insert(1, state);
        let prices = PricesState {
            up: ContractPrices {
                bid: 0.52,
                ask: 0.54,
                book: SideBook {
                    asks: vec![BookLevel {
                        price: 0.54,
                        size: 50.0,
                    }],
                    ..Default::default()
                },
            },
            down: ContractPrices::top(0.44, 0.46),
        };
        let win = WindowState {
            window_number: 1,
            role: "CURRENT".to_string(),
            status: "LIVE".to_string(),
            market: sample_market(),
            spent: 72.0,
            cash_returned: 0.0,
            up_shares: 0.0,
            down_shares: 80.0,
            initial_up_shares: 0.0,
            initial_down_shares: 0.0,
            trades: vec![],
            prices: prices.clone(),
        };
        let signals = strat.process_live_tick(
            &cfg,
            &prices,
            Some(62_585.0),
            &win.market,
            &win,
            35,
            26.8,
            SpotSignalSnapshot::default(),
            &mid,
            &CexMicroSnapshot::default(),
            &TradeTapeSnapshot::default(),
        );
        assert!(
            !signals.is_empty(),
            "flip hedge must fire for composite thesis"
        );
        assert_eq!(signals[0].side, "UP");
        assert!(
            signals[0].reason.starts_with("j_flip_hedge"),
            "reason={}",
            signals[0].reason
        );
        assert!(
            signals[0].amount <= cfg.j_endgame.flip_hedge_clip_usd + 1e-9,
            "hedge clip should be small, amount={}",
            signals[0].amount
        );
    }

    #[test]
    fn sell_rescue_cuts_primary_when_thesis_breaks() {
        let mut cfg = test_config();
        cfg.j_endgame.sell_rescue_min_gap_z = 0.65;
        let mut strat = strat_with_cash();
        strat.windows.insert(
            1,
            JWindowState {
                rescue_spent_usd: 72.0,
                primary_side: Some("DOWN".to_string()),
                clips_filled: 4,
                ..Default::default()
            },
        );
        let prices = PricesState {
            up: ContractPrices::top(0.52, 0.54),
            down: ContractPrices::top(0.44, 0.46),
        };
        let win = WindowState {
            window_number: 1,
            role: "CURRENT".to_string(),
            status: "LIVE".to_string(),
            market: sample_market(),
            spent: 72.0,
            cash_returned: 0.0,
            up_shares: 0.0,
            down_shares: 80.0,
            initial_up_shares: 0.0,
            initial_down_shares: 0.0,
            trades: vec![],
            prices: prices.clone(),
        };
        let mid = MidCrossSnapshot {
            armed: true,
            cross_count: 8,
            significant_cross_count: 3,
            last_cross_is_significant: true,
            ..Default::default()
        };
        let signals = strat.process_live_tick(
            &cfg,
            &prices,
            Some(62_585.0),
            &win.market,
            &win,
            35,
            26.8,
            SpotSignalSnapshot::default(),
            &mid,
            &CexMicroSnapshot::default(),
            &TradeTapeSnapshot::default(),
        );
        assert!(!signals.is_empty());
        assert!(!signals[0].is_buy);
        assert_eq!(signals[0].side, "DOWN");
        assert_eq!(signals[0].order_type.as_str(), "market");
        assert!(signals[0].reason.starts_with("j_sell_rescue"));
    }

    #[test]
    fn sell_rescue_fires_when_window_budget_exhausted() {
        let mut cfg = test_config();
        cfg.j_endgame.sell_rescue_min_gap_z = 0.65;
        let mut strat = strat_with_cash();
        strat.windows.insert(
            1,
            JWindowState {
                rescue_spent_usd: 72.0,
                primary_side: Some("DOWN".to_string()),
                clips_filled: 8,
                ..Default::default()
            },
        );
        let prices = PricesState {
            up: ContractPrices::top(0.52, 0.54),
            down: ContractPrices::top(0.44, 0.46),
        };
        let win = WindowState {
            window_number: 1,
            role: "CURRENT".to_string(),
            status: "LIVE".to_string(),
            market: sample_market(),
            spent: 80.0,
            cash_returned: 0.0,
            up_shares: 0.0,
            down_shares: 80.0,
            initial_up_shares: 0.0,
            initial_down_shares: 0.0,
            trades: vec![],
            prices: prices.clone(),
        };
        let mid = MidCrossSnapshot {
            armed: true,
            cross_count: 8,
            significant_cross_count: 3,
            last_cross_is_significant: true,
            ..Default::default()
        };
        let signals = strat.process_live_tick(
            &cfg,
            &prices,
            Some(62_585.0),
            &win.market,
            &win,
            60,
            26.8,
            SpotSignalSnapshot::default(),
            &mid,
            &CexMicroSnapshot::default(),
            &TradeTapeSnapshot::default(),
        );
        assert!(
            signals
                .iter()
                .any(|s| !s.is_buy && s.reason.starts_with("j_sell_rescue")),
            "sell rescue must still fire at exhausted budget, signals={signals:?}"
        );
        assert!(
            signals.iter().any(|s| s.reason.starts_with("j_flip_hedge")),
            "defensive hedge may run beside sell rescue, signals={signals:?}"
        );
        assert!(
            !strat.windows.get(&1).unwrap().sell_rescue_done,
            "sell_rescue_done must wait for fill"
        );
    }

    #[test]
    fn sell_rescue_done_set_only_after_execution() {
        let mut cfg = test_config();
        cfg.j_endgame.sell_rescue_min_gap_z = 0.65;
        let mut strat = strat_with_cash();
        strat.windows.insert(
            1,
            JWindowState {
                rescue_spent_usd: 72.0,
                primary_side: Some("DOWN".to_string()),
                ..Default::default()
            },
        );
        let prices = PricesState {
            up: ContractPrices::top(0.52, 0.54),
            down: ContractPrices::top(0.44, 0.46),
        };
        let win = WindowState {
            window_number: 1,
            role: "CURRENT".to_string(),
            status: "LIVE".to_string(),
            market: sample_market(),
            spent: 72.0,
            cash_returned: 0.0,
            up_shares: 0.0,
            down_shares: 80.0,
            initial_up_shares: 0.0,
            initial_down_shares: 0.0,
            trades: vec![],
            prices: prices.clone(),
        };
        let mid = MidCrossSnapshot {
            armed: true,
            cross_count: 8,
            significant_cross_count: 3,
            last_cross_is_significant: true,
            ..Default::default()
        };
        let signals = strat.process_live_tick(
            &cfg,
            &prices,
            Some(62_585.0),
            &win.market,
            &win,
            35,
            26.8,
            SpotSignalSnapshot::default(),
            &mid,
            &CexMicroSnapshot::default(),
            &TradeTapeSnapshot::default(),
        );
        assert!(!signals.is_empty());
        assert!(!strat.windows.get(&1).unwrap().sell_rescue_done);
        strat.notify_order_executed(1, &signals[0]);
        assert!(strat.windows.get(&1).unwrap().sell_rescue_done);
    }

    #[test]
    fn sell_rescue_execution_releases_primary_exposure() {
        let mut strat = strat_with_cash();
        strat.windows.insert(
            1,
            JWindowState {
                rescue_spent_usd: 10.0,
                cheap_spent_usd: 2.0,
                discount_reload_spent_usd: 3.0,
                primary_side: Some("UP".to_string()),
                ..Default::default()
            },
        );
        let sig = OrderSignal::sell("UP", OrderType::Limit, 5.0, 0.50, "j_sell_rescue_up_test");

        strat.notify_order_executed(1, &sig);

        let state = strat.windows.get(&1).unwrap();
        assert!(state.sell_rescue_done);
        assert!((state.rescue_spent_usd - 7.5).abs() < 1e-9);
        assert!((state.cheap_spent_usd - 2.0).abs() < 1e-9);
        assert!((state.discount_reload_spent_usd - 0.5).abs() < 1e-9);
    }
}
