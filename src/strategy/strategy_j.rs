use crate::client::{MarketWindow, PricesState};
use crate::config::Config;
use crate::j_fees::DEFAULT_CRYPTO_FEE_RATE_BPS;
use crate::mid_cross_tracker::LeadSide;
use crate::orderbook::{apply_fill_to_asks, ask_depth_usd, simulate_taker_buy_fill, SideBook};
use crate::redeem_hold::expected_move_usd;
use crate::strategy::{
    CexMicroSnapshot, MidCrossSnapshot, OrderSignal, OrderType, SpotSignalSnapshot, StrategyState,
    TradeStrategy,
};
use crate::trade_tape::{TradeTapeSnapshot, TradeTapeTracker};
use crate::trader::WindowState;
use std::collections::HashMap;
use std::time::Instant;

const J_MIN_TRADEABLE_WINDOW: usize = 1;

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

    pub(crate) fn mark_sell_rescue_executed(&mut self, window_number: usize) {
        if let Some(state) = self.windows.get_mut(&window_number) {
            state.sell_rescue_done = true;
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

fn j_buy_order_type(cfg: &crate::config::JEndgameConfig) -> OrderType {
    if cfg.taker_mode {
        OrderType::Market
    } else {
        OrderType::Limit
    }
}

fn sell_rescue_order_type(cfg: &crate::config::JEndgameConfig, secs_to_end: i64) -> OrderType {
    if cfg.sell_rescue_use_market || secs_to_end <= cfg.sell_rescue_market_secs {
        OrderType::Market
    } else {
        OrderType::Limit
    }
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
    Some(OrderSignal {
        side: primary.to_string(),
        is_buy: false,
        order_type: sell_rescue_order_type(cfg, secs_to_end),
        amount: sell_shares,
        price: bid,
        reason: format!(
            "j_sell_rescue_{}_bid_{:.2}_shares_{:.4}_value_{:.2}_gap_z_against_{:+.2}_hold_pnl_{:+.2}_after_sell_{:+.2}",
            primary.to_lowercase(),
            bid,
            sell_shares,
            sell_value,
            gz_against_primary,
            projected_hold_pnl,
            projected_after_sell,
        ),
    })
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

pub(crate) fn taker_max_pay(winner_ask: f64, cfg: &crate::config::JEndgameConfig) -> f64 {
    if cfg.taker_mode {
        winner_ask.min(cfg.taker_max_ask)
    } else {
        (winner_ask - cfg.limit_ask_offset).clamp(cfg.min_winner_ask, cfg.max_winner_ask)
    }
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

        // SELL rescue must run even when the window BUY budget is exhausted.
        let projected_hold_pnl =
            crate::j_controller::projected_redeem_pnl(win_state, current_winner, fee_bps);
        if let Some(sell) = plan_sell_rescue_signal(
            jcfg,
            state,
            win_state,
            prices,
            current_winner,
            gz,
            secs_to_end,
            projected_hold_pnl,
        ) {
            signals.push(sell);
            return signals;
        }

        let window_cap = if config.session.max_window_budget > 0.0 {
            jcfg.effective_max_usd_per_window(&config.session)
                .min(config.session.max_window_budget)
        } else {
            jcfg.effective_max_usd_per_window(&config.session)
        };
        if win_state.spent >= window_cap - 1e-9 || state.clips_filled >= effective_max_clips(jcfg) {
            // allow final seal / rescue even at window budget cap
            if secs_to_end > jcfg.final_seal_secs {
                return signals;
            }
        }

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

        if plan.need_tape && !tape_hot(tape, side, jcfg) {
            return signals;
        }

        let max_pay = if jcfg.taker_mode {
            plan.max_pay
        } else {
            (winner_ask - jcfg.limit_ask_offset).clamp(jcfg.min_winner_ask, plan.max_pay)
        };

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
            EndgameTier::Rescue | EndgameTier::DiscountReload | EndgameTier::FinalSeal
        ) {
            plan.budget_left
        } else {
            plan.budget_left
                .min((window_cap - win_state.spent).max(0.0))
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
            effective_max_clips(jcfg),
        );

        let (tape_usd, tape_count) = TradeTapeTracker::winner_stats(tape, side);
        let projected_pnl = crate::j_controller::projected_redeem_pnl(win_state, side, fee_bps);
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
        let mode = if jcfg.taker_mode { "taker" } else { "limit" };

        for (fill_price, usd) in fills {
            signals.push(OrderSignal {
                side: side.to_string(),
                is_buy: true,
                order_type: j_buy_order_type(jcfg),
                amount: usd,
                price: fill_price,
                reason: format!(
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
            });
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
        if signal.is_buy {
            self.mark_buy_executed(window_number, signal);
        } else if signal.reason.starts_with("j_sell_rescue") {
            self.mark_sell_rescue_executed(window_number);
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
        assert_eq!(signals[0].order_type.as_str(), "limit");
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
        assert_eq!(signals.len(), 1);
        assert!(!signals[0].is_buy);
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
}
