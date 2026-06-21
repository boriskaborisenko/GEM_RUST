//! J endgame window timeline controller — one objective: close at +target via phased buys.
//!
//! Timeline (5m window example):
//!   50–120s ACCUMULATE — ramped clips on winner when gap_z + confidence clear
//!   ≤25s   LATE       — tape-driven winner sweep
//!   ≤20s   RESCUE     — solve USD to hit target_profit; flip hedge if thesis broke
//!
//! Profit source: hold winner leg to $1 redeem — no sells.

use crate::client::PricesState;
use crate::config::{Config, JEndgameConfig};
use crate::j_fees::{leg_fee_usd, DEFAULT_CRYPTO_FEE_RATE_BPS};
use crate::mid_cross_tracker::{LeadSide, MidCrossSnapshot, MID_CROSS_ARM_TIME_PCT};
use crate::strategy::strategy_j::{
    flip_hedge_triggered, gap_z, ptb_dist_pct, side_ask, winner_side, EndgameTier, JWindowState,
    TierPlan,
};
use crate::strategy::{CexMicroSnapshot, SpotSignalSnapshot};
use crate::trade_tape::TradeTapeSnapshot;
use crate::trader::WindowState;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JWindowPhase {
    Warmup,
    Insurance,
    MidWindow,
    Accumulate,
    Late,
    Rescue,
    FinalSeal,
}

impl JWindowPhase {
    pub fn label(self) -> &'static str {
        match self {
            Self::Warmup => "warmup",
            Self::Insurance => "insurance",
            Self::MidWindow => "mid",
            Self::Accumulate => "accumulate",
            Self::Late => "late",
            Self::Rescue => "rescue",
            Self::FinalSeal => "final_seal",
        }
    }
}

pub fn detect_phase(
    elapsed_pct: f64,
    secs_to_end: i64,
    cfg: &JEndgameConfig,
    mid_cross: &MidCrossSnapshot,
) -> JWindowPhase {
    if secs_to_end <= 0 {
        return JWindowPhase::FinalSeal;
    }
    if secs_to_end <= cfg.final_seal_secs {
        return JWindowPhase::FinalSeal;
    }
    if elapsed_pct < MID_CROSS_ARM_TIME_PCT && !mid_cross.armed {
        return JWindowPhase::Warmup;
    }
    if cfg.insurance_enabled
        && elapsed_pct <= cfg.insurance_max_elapsed_pct
        && secs_to_end > cfg.endgame_secs
    {
        return JWindowPhase::Insurance;
    }
    if secs_to_end <= cfg.rescue_zone_secs {
        return JWindowPhase::Rescue;
    }
    if secs_to_end <= cfg.late_max_secs {
        return JWindowPhase::Late;
    }
    if secs_to_end <= cfg.endgame_secs && elapsed_pct >= cfg.cheap_min_elapsed_pct {
        return JWindowPhase::Accumulate;
    }
    JWindowPhase::MidWindow
}

pub fn redeem_pnl_if_wins(
    up_shares: f64,
    down_shares: f64,
    spent: f64,
    winner: &str,
    fee_bps: f64,
) -> f64 {
    let shares = match winner {
        "UP" => up_shares,
        "DOWN" => down_shares,
        _ => 0.0,
    };
    let redeem_fee = leg_fee_usd(1.0, shares, fee_bps);
    shares - redeem_fee - spent
}

fn pick_underdog(prices: &PricesState, max_ask: f64) -> Option<(&'static str, f64)> {
    let up = prices.up.ask;
    let down = prices.down.ask;
    if up <= down && up > 0.0 && up <= max_ask {
        Some(("UP", up))
    } else if down > 0.0 && down <= max_ask {
        Some(("DOWN", down))
    } else {
        None
    }
}

pub fn plan_insurance(
    cfg: &JEndgameConfig,
    state: &JWindowState,
    elapsed_pct: f64,
    spot: f64,
    ptb: f64,
    prices: &PricesState,
    mid_cross: &MidCrossSnapshot,
    min_atr: f64,
    current_atr: f64,
) -> Option<TierPlan> {
    if !cfg.insurance_enabled || elapsed_pct > cfg.insurance_max_elapsed_pct {
        return None;
    }
    if state.insurance_clips >= cfg.insurance_max_clips {
        return None;
    }
    if state.insurance_spent_usd + 1e-9 >= cfg.insurance_tier_usd {
        return None;
    }
    if min_atr > 0.0 && current_atr < min_atr {
        return None;
    }
    let dist = ptb_dist_pct(spot, ptb);
    if !dist.is_finite() || dist > cfg.insurance_max_ptb_dist_pct {
        return None;
    }
    if cfg.insurance_max_lead_gap > 0.0 && mid_cross.peak_lead_gap >= cfg.insurance_max_lead_gap {
        return None;
    }
    let (side, ask) = pick_underdog(prices, cfg.insurance_max_ask)?;
    let clip = cfg.insurance_clip_usd.max(cfg.probe_clip_usd);
    Some(TierPlan {
        tier: EndgameTier::Insurance,
        max_pay: ask,
        need_tape: false,
        budget_left: (cfg.insurance_tier_usd - state.insurance_spent_usd).min(clip),
        sweep_clips: 1,
        side: Some(side.to_string()),
        clip_usd: clip,
    })
}

fn rescue_budget(cfg: &Config, state: &JWindowState, win_spent: f64, available_cash: f64) -> f64 {
    let j = &cfg.j_endgame;
    let max_window = j.effective_max_usd_per_window(&cfg.session);
    let max_rescue = j.effective_max_rescue_usd(&cfg.session);
    let window_left = (max_window - win_spent).max(0.0);
    let window_left = if cfg.session.max_window_budget > 0.0 {
        window_left.min(cfg.session.max_window_budget)
    } else {
        window_left
    };
    window_left
        .max(0.0)
        .min(max_rescue - state.rescue_spent_usd)
        .min(available_cash.max(0.0))
}

fn ramp(x: f64, lo: f64, hi: f64) -> f64 {
    if !x.is_finite() {
        return 0.0;
    }
    if hi <= lo {
        return if x >= hi { 1.0 } else { 0.0 };
    }
    ((x - lo) / (hi - lo)).clamp(0.0, 1.0)
}

/// Composite confidence C ∈ [0,1] that the current `winner` side will hold to
/// settlement. Built from a weighted blend of the live signals the bot actually
/// receives each tick:
///   - gap_z:     winner distance from PTB ÷ expected move (time/vol-normalized)
///   - momentum:  Binance/Bybit smoothed spot velocity, toward the winner
///   - book:      mid-cross lead on the winner, discounted by chop (sig crosses)
///   - flow:      Polymarket tape imbalance + CEX buy/sell imbalance, toward winner
///
/// Hard vetoes (return 0): gap below the risk floor (coin flip), or the book
/// clearly leading the OPPOSITE side. The number of endgame buys is an emergent
/// consequence of how C evolves — it is never a fixed schedule or count.
pub fn endgame_confidence(
    cfg: &JEndgameConfig,
    winner: &str,
    gz: f64,
    spot_sig: &SpotSignalSnapshot,
    mid_cross: &MidCrossSnapshot,
    cex: &CexMicroSnapshot,
    tape: &TradeTapeSnapshot,
) -> f64 {
    let dir = if winner == "UP" { 1.0 } else { -1.0 };

    // gap_z: below the floor → treat as coin flip, no edge.
    let floor = cfg.final_seal_min_gap_z;
    let full = cfg.full_size_gap_z.max(floor + 1e-6);
    let c_gap = ramp(gz.abs(), floor, full);
    if c_gap <= 0.0 {
        return 0.0;
    }

    // Book contradiction veto: book firmly leading the other side.
    let book_winner = match mid_cross.current_side {
        Some(LeadSide::Up) => Some("UP"),
        Some(LeadSide::Down) => Some("DOWN"),
        Some(LeadSide::Tie) | None => None,
    };
    if let Some(bw) = book_winner {
        if bw != winner && mid_cross.lead_gap >= cfg.book_contradict_gap {
            return 0.0;
        }
    }

    // Book agreement (chop-penalized).
    let book_aligned = book_winner == Some(winner);
    let chop = 1.0
        - (mid_cross.significant_cross_count as f64 / (cfg.book_max_sig_cross.max(1) as f64))
            .clamp(0.0, 1.0);
    let c_book = if book_aligned {
        ramp(mid_cross.lead_gap, 0.0, cfg.book_full_lead_gap) * chop
    } else {
        0.0
    };

    // Momentum: spot moving deeper ITM for the winner.
    let v = spot_sig.smoothed_velocity_usd_per_sec.unwrap_or(0.0);
    let c_mom = ramp(v * dir, 0.0, cfg.mom_full_vel_usd_per_sec.max(1e-6));

    // Order flow: Polymarket tape + CEX imbalance, toward the winner.
    let (win_buy, lose_buy) = if winner == "UP" {
        (tape.up_buy_usd, tape.down_buy_usd)
    } else {
        (tape.down_buy_usd, tape.up_buy_usd)
    };
    let imb_tape = if win_buy + lose_buy > 0.0 {
        (win_buy - lose_buy) / (win_buy + lose_buy)
    } else {
        0.0
    };
    let cex_for_winner = cex.buy_sell_imbalance_3s * dir;
    let c_flow = (0.5 * imb_tape + 0.5 * cex_for_winner).clamp(0.0, 1.0);

    let wsum = cfg.conf_w_gap + cfg.conf_w_mom + cfg.conf_w_book + cfg.conf_w_flow;
    if wsum <= 0.0 {
        return 0.0;
    }
    let mut c = (cfg.conf_w_gap * c_gap
        + cfg.conf_w_mom * c_mom
        + cfg.conf_w_book * c_book
        + cfg.conf_w_flow * c_flow)
        / wsum;

    // Strong gap_z = time/vol-normalized safety. When spot is clearly ITM with
    // low expected remaining move (small ATR → gap_z reads higher), trust the
    // gap even if book/flow haven't fully caught up — this is how we buy @0.83
    // instead of waiting until the book reprices to 0.99.
    if c_gap >= 0.45 {
        c = c.max(c_gap * 0.72 + c * 0.28);
    }
    let safe_lo = full * 0.75;
    let safe_hi = full * 1.25;
    if gz.abs() >= safe_lo {
        let t = ramp(gz.abs(), safe_lo, safe_hi);
        c = c.max(0.58 + 0.38 * t);
    }
    c
}

/// Lower entry bar when the ask is cheap (value) or gap_z shows clear safety.
fn effective_conf_enter(cfg: &JEndgameConfig, ask: f64, gz: f64) -> f64 {
    let mut enter = cfg.conf_enter;
    if ask > 0.0 && ask <= cfg.cheap_max_ask {
        let cheap = ((cfg.cheap_max_ask - ask) / cfg.cheap_max_ask.max(0.01)).clamp(0.0, 1.0);
        enter -= 0.14 * cheap;
    }
    let full = cfg.full_size_gap_z;
    if gz.abs() >= full * 0.75 {
        enter -= 0.10 * ramp(gz.abs(), full * 0.75, full * 1.25);
    }
    enter.clamp(0.40, cfg.conf_enter)
}

/// USD to deploy on `winner` at `ask` so redeem PnL reaches `target_profit`.
/// At ask=0.99 the edge is ~1% so recovering a -$2 hole needs ~$300 — this
/// makes the planner size accordingly (then budget caps apply).
fn usd_to_close_profit_gap(
    win_state: &WindowState,
    winner: &str,
    ask: f64,
    target_profit: f64,
    fee_bps: f64,
) -> f64 {
    let current = redeem_pnl_if_wins(
        win_state.up_shares,
        win_state.down_shares,
        win_state.spent,
        winner,
        fee_bps,
    );
    if current + 1e-9 >= target_profit {
        return 0.0;
    }
    let edge = (1.0 / ask - 1.0).max(1e-9);
    (target_profit - current) / edge
}

fn has_deployed_exposure(win_state: &WindowState) -> bool {
    win_state.spent > 1e-9 || win_state.up_shares > 1e-9 || win_state.down_shares > 1e-9
}

fn shares_for_side(side: &str, win_state: &WindowState) -> f64 {
    if side == "UP" {
        win_state.up_shares
    } else {
        win_state.down_shares
    }
}

fn primary_avg_entry_ask(state: &JWindowState, win_state: &WindowState) -> Option<f64> {
    let primary = state.primary_side.as_deref()?;
    let shares = shares_for_side(primary, win_state);
    let spent = state.primary_exposure_usd();
    if shares > 1e-9 && spent > 1e-9 {
        Some(spent / shares)
    } else {
        None
    }
}

/// Hard cap for primary winner exposure by current ask. Old J worked best when
/// it stayed simple and avoided huge late-cost entries; this keeps that shape
/// while making the expensive tail explicit and testable.
pub fn tail_cut_exposure_cap_usd(config: &Config, ask: f64) -> f64 {
    if !ask.is_finite() || ask <= 0.0 {
        return 0.0;
    }
    let cfg = &config.j_endgame;
    let session = &config.session;
    let cap = if ask <= 0.70 {
        cfg.effective_tail_cap_ask70_usd(session)
    } else if ask <= 0.88 {
        cfg.effective_tail_cap_ask88_usd(session)
    } else if ask <= 0.94 {
        cfg.effective_tail_cap_ask94_usd(session)
    } else if ask <= 0.97 {
        cfg.effective_tail_cap_ask97_usd(session)
    } else {
        0.0
    };
    cap.max(0.0)
}

/// Per-tick clip cap: probe on first buy, then ramp with gap_z, time, and ask cheapness.
fn effective_max_clip_usd(
    config: &Config,
    rescue_spent_usd: f64,
    gz: f64,
    ask: f64,
    elapsed_pct: f64,
) -> f64 {
    let cfg = &config.j_endgame;
    let session = &config.session;
    let floor = cfg.effective_probe_clip_usd(session).max(1e-9);
    if rescue_spent_usd < 1e-9 {
        return cfg.effective_first_clip_usd(session).max(floor);
    }
    let full = cfg.full_size_gap_z;
    let gz_ramp = ramp(gz.abs(), full * 0.75, full);
    let time_ramp = ramp(elapsed_pct, 60.0, 78.0);
    let cheap_ramp = if ask <= cfg.cheap_max_ask {
        1.0
    } else {
        ramp(cfg.expensive_ask_threshold - ask, 0.0, 0.04)
    };
    let scale = (gz_ramp * time_ramp.max(0.35 * cheap_ramp) * cheap_ramp).clamp(0.0, 1.0);
    let max_clip = cfg.effective_max_clip_usd(session).max(floor);
    (floor + scale * (max_clip - floor)).max(cfg.effective_first_clip_usd(session).max(floor))
}

/// Target-exposure endgame: given composite confidence, compute how much USD we
/// WANT on the winner and buy only the positive delta vs what's already deployed.
/// Target is the max of (a) confidence-scaled budget and (b) USD needed to reach
/// `target_profit_usd` at the current ask — so buying @0.99 gets sized for the
/// 1% edge instead of deploying a useless $6 clip.
pub fn plan_endgame_composite(
    config: &Config,
    state: &JWindowState,
    win_state: &WindowState,
    winner: &str,
    ask: f64,
    gz: f64,
    confidence: f64,
    elapsed_pct: f64,
    available_cash: f64,
) -> Option<TierPlan> {
    let cfg = &config.j_endgame;
    if ask <= 0.0 || ask > cfg.final_seal_max_ask {
        return None;
    }
    if state.rescue_spent_usd < 1e-9
        && ask > cfg.expensive_ask_threshold
        && gz.abs() + 1e-9 < cfg.expensive_min_gap_z
    {
        return None;
    }
    let enter = effective_conf_enter(cfg, ask, gz);
    if confidence < enter {
        return None;
    }
    let fee_bps = cfg.fee_rate_bps.unwrap_or(DEFAULT_CRYPTO_FEE_RATE_BPS);
    let max_rescue = cfg.effective_max_rescue_usd(&config.session);
    let probe_clip = cfg.effective_probe_clip_usd(&config.session);
    let min_increment = cfg.effective_min_increment_usd(&config.session);
    let exposure_cap = tail_cut_exposure_cap_usd(config, ask).min(max_rescue);
    if exposure_cap <= 1e-9 || state.rescue_spent_usd + 1e-9 >= exposure_cap {
        return None;
    }
    let mut eff = ramp(confidence, enter, 1.0);
    // Safe gap at end → deploy aggressively; we are not on a coin-flip edge.
    let full = cfg.full_size_gap_z;
    if gz.abs() >= full * 0.75 {
        let gz_boost = ramp(gz.abs(), full * 0.75, full * 1.5);
        eff = eff.max(0.55 + 0.45 * gz_boost);
    }
    let conf_target = (eff * max_rescue).min(exposure_cap);
    let remaining = rescue_budget(config, state, win_state.spent, available_cash);
    let profit_increment = if has_deployed_exposure(win_state) {
        usd_to_close_profit_gap(win_state, winner, ask, cfg.target_profit_usd, fee_bps)
    } else {
        0.0
    };
    if profit_increment > 0.0 {
        if ask > cfg.abort_rescue_if_ask_above {
            return None;
        }
        if profit_increment > remaining + 1e-9 {
            return None;
        }
        if state.rescue_spent_usd + profit_increment > exposure_cap + 1e-9 {
            return None;
        }
    }
    let profit_target = (state.rescue_spent_usd + profit_increment).min(exposure_cap);
    let target = conf_target.max(profit_target).min(exposure_cap);
    let increment = (target - state.rescue_spent_usd).clamp(0.0, remaining);
    if increment + 1e-9 < probe_clip {
        return None;
    }
    if state.rescue_spent_usd > 1e-9 && increment + 1e-9 < min_increment {
        return None;
    }
    let max_clip = effective_max_clip_usd(config, state.rescue_spent_usd, gz, ask, elapsed_pct);
    let clip = increment.min(max_clip);
    Some(TierPlan {
        tier: EndgameTier::FinalSeal,
        max_pay: ask.min(cfg.taker_max_ask),
        need_tape: false,
        budget_left: clip,
        sweep_clips: 1,
        side: Some(winner.to_string()),
        clip_usd: clip,
    })
}

pub fn plan_discount_reload(
    config: &Config,
    state: &JWindowState,
    win_state: &WindowState,
    current_winner: &str,
    ask: f64,
    gz: f64,
    available_cash: f64,
) -> Option<TierPlan> {
    let cfg = &config.j_endgame;
    if !cfg.discount_reload_enabled || !state.has_primary_exposure() {
        return None;
    }
    let primary = state.primary_side.as_deref()?;
    if primary != current_winner {
        return None;
    }
    if ask <= 0.0 || ask > cfg.discount_reload_max_ask {
        return None;
    }
    if state.discount_reload_clips >= cfg.discount_reload_max_clips {
        return None;
    }
    let avg = primary_avg_entry_ask(state, win_state)?;
    if avg - ask + 1e-9 < cfg.discount_reload_min_drop {
        return None;
    }
    let gz_toward_primary = if primary == "UP" { gz } else { -gz };
    if gz_toward_primary + 1e-9 < cfg.discount_reload_min_gap_z {
        return None;
    }

    let probe_clip = cfg.effective_probe_clip_usd(&config.session);
    let tail_left = tail_cut_exposure_cap_usd(config, ask) - state.rescue_spent_usd;
    let reload_left =
        cfg.effective_discount_reload_max_usd(&config.session) - state.discount_reload_spent_usd;
    let remaining = rescue_budget(config, state, win_state.spent, available_cash)
        .min(tail_left)
        .min(reload_left)
        .max(0.0);
    if remaining + 1e-9 < probe_clip {
        return None;
    }
    let clip = cfg
        .effective_discount_reload_clip_usd(&config.session)
        .max(probe_clip)
        .min(remaining);
    Some(TierPlan {
        tier: EndgameTier::DiscountReload,
        max_pay: ask.min(cfg.taker_max_ask),
        need_tape: false,
        budget_left: remaining,
        sweep_clips: 1,
        side: Some(primary.to_string()),
        clip_usd: clip,
    })
}

pub fn flip_hedge_budget_cap(config: &Config, state: &JWindowState) -> f64 {
    let cfg = &config.j_endgame;
    let primary = state.primary_exposure_usd();
    cfg.effective_flip_tier_usd(&config.session)
        .max(primary * cfg.flip_hedge_exposure_ratio)
        .min(cfg.effective_flip_tier_max_usd(&config.session))
}

pub fn plan_flip_hedge_rescue(
    config: &Config,
    state: &JWindowState,
    current_winner: &str,
    spot: f64,
    ptb: f64,
    gz: f64,
    prices: &PricesState,
    mid_cross: &MidCrossSnapshot,
) -> Option<TierPlan> {
    let cfg = &config.j_endgame;
    let primary = state.primary_side.as_deref()?;
    if !flip_hedge_triggered(
        cfg,
        state,
        primary,
        current_winner,
        spot,
        ptb,
        gz,
        mid_cross,
    ) {
        return None;
    }
    let hedge_side = if primary == "UP" { "DOWN" } else { "UP" };
    let hedge_ask = side_ask(hedge_side, prices);
    let budget_cap = flip_hedge_budget_cap(config, state);
    if state.hedge_spent_usd + 1e-9 >= budget_cap || hedge_ask > cfg.flip_max_ask {
        return None;
    }
    let probe_clip = cfg.effective_probe_clip_usd(&config.session);
    let hedge_clip_base = cfg
        .effective_flip_hedge_clip_usd(&config.session)
        .max(probe_clip);
    let budget_left = budget_cap - state.hedge_spent_usd;
    let hedge_clip = hedge_clip_base
        .min(budget_left)
        .max(probe_clip.min(budget_left));
    Some(TierPlan {
        tier: EndgameTier::FlipHedge,
        max_pay: hedge_ask.min(cfg.flip_max_ask),
        need_tape: cfg.flip_require_tape,
        budget_left,
        sweep_clips: cfg.flip_sweep_clips_per_tick,
        side: Some(hedge_side.to_string()),
        clip_usd: hedge_clip,
    })
}

/// Unified timeline planner. Three independent engines, no time-of-window rails:
///   - FLIP-HEDGE: buy the opposite side if our committed thesis reverses
///   - COMPOSITE: signal-driven target-exposure on the winner (emergent N buys)
/// `confidence` is the composite signal score from [`endgame_confidence`].
pub fn plan_j_window(
    config: &Config,
    state: &JWindowState,
    win_state: &WindowState,
    prices: &PricesState,
    spot: f64,
    ptb: f64,
    secs_to_end: i64,
    elapsed_pct: f64,
    current_atr: f64,
    min_atr: f64,
    mid_cross: &MidCrossSnapshot,
    allow_directional: bool,
    confidence: f64,
    available_cash: f64,
) -> Option<TierPlan> {
    let cfg = &config.j_endgame;
    let Some(current_winner) = winner_side(spot, ptb) else {
        return None;
    };
    let gz = gap_z(spot, ptb, current_atr, secs_to_end);
    if !gz.is_finite() {
        return None;
    }
    let phase = detect_phase(elapsed_pct, secs_to_end, cfg, mid_cross);

    // Early window: only insurance optionality is allowed.
    if let JWindowPhase::Warmup | JWindowPhase::MidWindow = phase {
        return None;
    }
    if let JWindowPhase::Insurance = phase {
        return plan_insurance(
            cfg,
            state,
            elapsed_pct,
            spot,
            ptb,
            prices,
            mid_cross,
            min_atr,
            current_atr,
        );
    }

    // Endgame zone: flip-hedge first (defends a reversal), then discount reload
    // if our existing thesis is still winner but got cheaper, then composite.
    plan_flip_hedge_rescue(
        config,
        state,
        current_winner,
        spot,
        ptb,
        gz,
        prices,
        mid_cross,
    )
    .or_else(|| {
        if !allow_directional {
            return None;
        }
        let ask = side_ask(current_winner, prices);
        if let Some(plan) = plan_discount_reload(
            config,
            state,
            win_state,
            current_winner,
            ask,
            gz,
            available_cash,
        ) {
            return Some(plan);
        }
        plan_endgame_composite(
            config,
            state,
            win_state,
            current_winner,
            ask,
            gz,
            confidence,
            elapsed_pct,
            available_cash,
        )
    })
}

pub fn projected_redeem_pnl(win_state: &WindowState, winner: &str, fee_bps: f64) -> f64 {
    redeem_pnl_if_wins(
        win_state.up_shares,
        win_state.down_shares,
        win_state.spent,
        winner,
        fee_bps,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::ContractPrices;
    use crate::config::{Config, JEndgameConfig};
    use crate::orderbook::SideBook;
    use crate::strategy::strategy_j::JWindowState;

    fn j_cfg() -> JEndgameConfig {
        let mut c = JEndgameConfig::default();
        c.insurance_enabled = true;
        c.insurance_max_ask = 0.18;
        c.insurance_max_ptb_dist_pct = 0.05;
        c.target_profit_usd = 1.0;
        c
    }

    fn full_cfg(mut j: JEndgameConfig) -> Config {
        let mut c = Config::load("config.json").expect("config load");
        j.fee_rate_bps = Some(0.0);
        c.j_endgame = j;
        c.session.max_window_budget = 500.0;
        c
    }

    #[test]
    fn insurance_picks_cheap_underdog_near_ptb() {
        let prices = PricesState {
            up: ContractPrices {
                ask: 0.74,
                ..ContractPrices::top(0.72, 0.74)
            },
            down: ContractPrices {
                ask: 0.17,
                ..ContractPrices::top(0.15, 0.17)
            },
        };
        let plan = plan_insurance(
            &j_cfg(),
            &JWindowState::default(),
            15.0,
            60_010.0,
            60_000.0,
            &prices,
            &MidCrossSnapshot::default(),
            0.0,
            30.0,
        )
        .expect("insurance");
        assert_eq!(plan.tier, EndgameTier::Insurance);
        assert_eq!(plan.side.as_deref(), Some("DOWN"));
        assert!((plan.clip_usd - 1.0).abs() < 1e-9);
    }

    fn win_state_zero() -> crate::trader::WindowState {
        use crate::client::MarketWindow;
        crate::trader::WindowState {
            window_number: 1,
            role: "CURRENT".into(),
            status: "LIVE".into(),
            market: MarketWindow {
                id: "t".into(),
                slug: "t".into(),
                question: "t".into(),
                asset: "BTC".into(),
                interval: "5m".into(),
                start_time: String::new(),
                end_time: String::new(),
                price_to_beat: Some(60_000.0),
                tokens: crate::client::TokensMap {
                    up: crate::client::TokenInfo {
                        token_id: "u".into(),
                        outcome_name: "Up".into(),
                    },
                    down: crate::client::TokenInfo {
                        token_id: "d".into(),
                        outcome_name: "Down".into(),
                    },
                },
            },
            spent: 0.0,
            cash_returned: 0.0,
            up_shares: 0.0,
            down_shares: 0.0,
            initial_up_shares: 0.0,
            initial_down_shares: 0.0,
            trades: vec![],
            prices: PricesState {
                up: ContractPrices {
                    ask: 0.92,
                    ..ContractPrices::top(0.90, 0.92)
                },
                down: ContractPrices::top(0.07, 0.08),
            },
        }
    }

    #[test]
    fn confidence_zero_below_gap_floor() {
        let mut c = JEndgameConfig::default();
        c.final_seal_min_gap_z = 0.8;
        c.full_size_gap_z = 2.5;
        // gz below floor => coin flip => no edge, regardless of other signals.
        let conf = endgame_confidence(
            &c,
            "UP",
            0.2,
            &SpotSignalSnapshot::default(),
            &MidCrossSnapshot::default(),
            &CexMicroSnapshot::default(),
            &TradeTapeSnapshot::default(),
        );
        assert_eq!(conf, 0.0);
    }

    #[test]
    fn confidence_vetoed_when_book_leads_opposite() {
        let mut c = JEndgameConfig::default();
        c.final_seal_min_gap_z = 0.8;
        c.full_size_gap_z = 2.5;
        c.book_contradict_gap = 0.04;
        let mc = MidCrossSnapshot {
            current_side: Some(LeadSide::Down),
            lead_gap: 0.30,
            ..Default::default()
        };
        // Winner UP but the book firmly leads DOWN => veto.
        let conf = endgame_confidence(
            &c,
            "UP",
            3.0,
            &SpotSignalSnapshot::default(),
            &mc,
            &CexMicroSnapshot::default(),
            &TradeTapeSnapshot::default(),
        );
        assert_eq!(conf, 0.0);
    }

    #[test]
    fn composite_target_exposure_throttles_and_stops() {
        let j = {
            let mut c = JEndgameConfig::default();
            c.conf_enter = 0.5;
            c.max_rescue_usd = 60.0;
            c.max_usd_per_window = 60.0;
            c.probe_clip_usd = 1.0;
            c.first_clip_usd = 8.0;
            c.min_increment_usd = 5.0;
            c.max_clip_usd = 25.0;
            c.tail_cap_ask94_usd = 60.0;
            c.final_seal_max_ask = 0.99;
            c.taker_max_ask = 0.99;
            c
        };
        let cfg = full_cfg(j);
        let win = win_state_zero();

        // Below conf_enter => no buy at all (0 buys for coin-flip windows).
        let mut state = JWindowState::default();
        assert!(
            plan_endgame_composite(&cfg, &state, &win, "UP", 0.92, 0.5, 0.49, 65.0, 500.0)
                .is_none()
        );

        // conf 0.75 => eff 0.5 => target 30; first clip capped at first_clip_usd=8.
        let p1 = plan_endgame_composite(&cfg, &state, &win, "UP", 0.92, 1.0, 0.75, 65.0, 500.0)
            .expect("first add");
        assert!((p1.clip_usd - 8.0).abs() < 1e-9, "clip={}", p1.clip_usd);
        state.rescue_spent_usd += p1.clip_usd;

        // Same confidence again: increment 22, but min_increment=5 and ramped max_clip.
        let p2 = plan_endgame_composite(&cfg, &state, &win, "UP", 0.92, 1.0, 0.75, 70.0, 500.0)
            .expect("second add");
        assert!(p2.clip_usd >= 5.0, "clip={}", p2.clip_usd);
        state.rescue_spent_usd = 30.0;

        // At target, flat confidence => nothing more (no per-tick spam).
        assert!(
            plan_endgame_composite(&cfg, &state, &win, "UP", 0.92, 1.0, 0.75, 70.0, 500.0)
                .is_none()
        );

        // Confidence rises to full => target 60, buys the next increment.
        let p3 = plan_endgame_composite(&cfg, &state, &win, "UP", 0.92, 1.0, 1.0, 75.0, 500.0)
            .expect("third add on stronger signal");
        assert!(p3.clip_usd > 0.0);

        // Ask too expensive => skip even with full confidence.
        assert!(
            plan_endgame_composite(&cfg, &state, &win, "UP", 0.999, 1.0, 1.0, 75.0, 500.0)
                .is_none()
        );
    }

    #[test]
    fn expensive_ask_blocks_weak_gap_fresh_entry() {
        let j = {
            let mut c = JEndgameConfig::default();
            c.expensive_ask_threshold = 0.94;
            c.expensive_min_gap_z = 1.35;
            c.conf_enter = 0.5;
            c.final_seal_max_ask = 0.99;
            c
        };
        let cfg = full_cfg(j);
        let win = win_state_zero();
        let state = JWindowState::default();
        assert!(
            plan_endgame_composite(&cfg, &state, &win, "DOWN", 0.98, -1.14, 0.8, 63.0, 500.0)
                .is_none(),
            "w90-like weak gap @ expensive ask should not enter fresh"
        );
    }

    #[test]
    fn tail_cut_blocks_fresh_entry_above_97c() {
        let j = {
            let mut c = JEndgameConfig::default();
            c.conf_enter = 0.5;
            c.target_profit_usd = 1.0;
            c.max_rescue_usd = 75.0;
            c.first_clip_usd = 8.0;
            c.final_seal_max_ask = 0.99;
            c
        };
        let cfg = full_cfg(j);
        let win = win_state_zero();
        let state = JWindowState::default();
        assert!(
            plan_endgame_composite(&cfg, &state, &win, "UP", 0.99, 2.0, 0.8, 65.0, 500.0).is_none(),
            "fresh @0.99 should not open a costly tail position"
        );
    }

    #[test]
    fn tail_cut_caps_expensive_primary_exposure() {
        let j = {
            let mut c = JEndgameConfig::default();
            c.conf_enter = 0.5;
            c.max_rescue_usd = 75.0;
            c.max_usd_per_window = 80.0;
            c.probe_clip_usd = 1.0;
            c.first_clip_usd = 8.0;
            c.tail_cap_ask97_usd = 14.0;
            c.final_seal_max_ask = 0.99;
            c.taker_max_ask = 0.99;
            c
        };
        let cfg = full_cfg(j);
        let win = win_state_zero();
        let open = JWindowState::default();
        let first = plan_endgame_composite(&cfg, &open, &win, "UP", 0.96, 2.0, 1.0, 80.0, 500.0)
            .expect("first high-ask clip");
        assert!(first.clip_usd <= 8.0 + 1e-9, "clip={}", first.clip_usd);

        let capped = JWindowState {
            rescue_spent_usd: 14.0,
            ..Default::default()
        };
        assert!(
            plan_endgame_composite(&cfg, &capped, &win, "UP", 0.96, 2.0, 1.0, 80.0, 500.0)
                .is_none(),
            "0.96 ask must stop at tailCapAsk97Usd"
        );
    }

    #[test]
    fn discount_reload_adds_primary_when_discounted_and_still_winner() {
        let j = {
            let mut c = JEndgameConfig::default();
            c.max_rescue_usd = 75.0;
            c.max_usd_per_window = 80.0;
            c.probe_clip_usd = 1.0;
            c.discount_reload_enabled = true;
            c.discount_reload_max_ask = 0.74;
            c.discount_reload_min_drop = 0.12;
            c.discount_reload_min_gap_z = 1.10;
            c.discount_reload_clip_usd = 4.0;
            c.discount_reload_max_usd = 12.0;
            c.discount_reload_max_clips = 2;
            c
        };
        let cfg = full_cfg(j);
        let mut win = win_state_zero();
        win.spent = 8.0;
        win.up_shares = 8.0 / 0.98;
        let state = JWindowState {
            rescue_spent_usd: 8.0,
            primary_side: Some("UP".to_string()),
            clips_filled: 1,
            ..Default::default()
        };

        let plan =
            plan_discount_reload(&cfg, &state, &win, "UP", 0.70, 1.35, 500.0).expect("reload");
        assert_eq!(plan.tier, EndgameTier::DiscountReload);
        assert_eq!(plan.side.as_deref(), Some("UP"));
        assert!((plan.clip_usd - 4.0).abs() < 1e-9, "clip={}", plan.clip_usd);
    }

    #[test]
    fn discount_reload_does_not_average_losing_primary() {
        let cfg = full_cfg(JEndgameConfig::default());
        let mut win = win_state_zero();
        win.spent = 8.0;
        win.up_shares = 8.0 / 0.98;
        let state = JWindowState {
            rescue_spent_usd: 8.0,
            primary_side: Some("UP".to_string()),
            clips_filled: 1,
            ..Default::default()
        };

        assert!(
            plan_discount_reload(&cfg, &state, &win, "DOWN", 0.70, -1.35, 500.0).is_none(),
            "reload must not buy more UP after UP is no longer the spot/PTB winner"
        );
    }

    #[test]
    fn discount_reload_respects_reload_budget() {
        let j = {
            let mut c = JEndgameConfig::default();
            c.discount_reload_max_usd = 12.0;
            c.discount_reload_max_clips = 2;
            c
        };
        let cfg = full_cfg(j);
        let mut win = win_state_zero();
        win.spent = 20.0;
        win.up_shares = 20.0 / 0.98;
        let state = JWindowState {
            rescue_spent_usd: 20.0,
            discount_reload_spent_usd: 12.0,
            discount_reload_clips: 2,
            primary_side: Some("UP".to_string()),
            clips_filled: 3,
            ..Default::default()
        };

        assert!(plan_discount_reload(&cfg, &state, &win, "UP", 0.70, 1.35, 500.0).is_none());
    }

    #[test]
    fn safe_gap_fires_value_entry_at_cheap_ask() {
        // Log-like: gap_z ~1.94, UP ask 0.88, minimal book/flow — should enter.
        let j = {
            let mut c = JEndgameConfig::default();
            c.conf_enter = 0.58;
            c.full_size_gap_z = 1.8;
            c.final_seal_min_gap_z = 0.8;
            c.max_rescue_usd = 75.0;
            c.max_usd_per_window = 80.0;
            c.probe_clip_usd = 1.0;
            c.max_clip_usd = 35.0;
            c.cheap_max_ask = 0.88;
            c.final_seal_max_ask = 0.99;
            c.taker_max_ask = 0.99;
            c
        };
        let cfg = full_cfg(j);
        let win = win_state_zero();
        let conf = endgame_confidence(
            &cfg.j_endgame,
            "UP",
            1.94,
            &SpotSignalSnapshot::default(),
            &MidCrossSnapshot::default(),
            &CexMicroSnapshot::default(),
            &TradeTapeSnapshot::default(),
        );
        assert!(
            conf + 1e-9 >= effective_conf_enter(&cfg.j_endgame, 0.88, 1.94),
            "conf={conf} should pass value entry at gz=1.94 ask=0.88"
        );
        let plan = plan_endgame_composite(
            &cfg,
            &JWindowState::default(),
            &win,
            "UP",
            0.88,
            1.94,
            conf,
            65.0,
            500.0,
        )
        .expect("value entry on safe gap");
        assert!(plan.clip_usd <= 8.0 + 1e-9, "first clip={}", plan.clip_usd);
    }

    #[test]
    fn composite_aborts_impossible_profit_gap_at_high_ask() {
        // Reproduce log economics: $2 insurance lost, need target +$1 at ask 0.99.
        let j = {
            let mut c = JEndgameConfig::default();
            c.conf_enter = 0.5;
            c.target_profit_usd = 1.0;
            c.max_rescue_usd = 75.0;
            c.max_usd_per_window = 80.0;
            c.probe_clip_usd = 1.0;
            c.max_clip_usd = 25.0;
            c.final_seal_max_ask = 0.99;
            c.taker_max_ask = 0.99;
            c.fee_rate_bps = Some(0.0);
            c
        };
        let cfg = full_cfg(j);
        let mut win = win_state_zero();
        win.spent = 2.0;
        win.down_shares = 7.14;
        let state = JWindowState {
            insurance_spent_usd: 2.0,
            insurance_clips: 1,
            ..Default::default()
        };
        // Need roughly $300 to go from -2 to +1 at 0.99. With a $75 rescue cap,
        // this is a controlled no-trade instead of chasing an unreachable target.
        assert!(
            plan_endgame_composite(&cfg, &state, &win, "UP", 0.99, 2.0, 0.8, 65.0, 500.0).is_none()
        );
    }

    #[test]
    fn composite_allows_affordable_profit_gap() {
        let j = {
            let mut c = JEndgameConfig::default();
            c.conf_enter = 0.5;
            c.target_profit_usd = 1.0;
            c.max_rescue_usd = 75.0;
            c.max_usd_per_window = 80.0;
            c.probe_clip_usd = 1.0;
            c.max_clip_usd = 25.0;
            c.final_seal_max_ask = 0.90;
            c.taker_max_ask = 0.90;
            c.fee_rate_bps = Some(0.0);
            c
        };
        let cfg = full_cfg(j);
        let mut win = win_state_zero();
        win.spent = 10.0;
        win.up_shares = 10.0;
        let plan = plan_endgame_composite(
            &cfg,
            &JWindowState::default(),
            &win,
            "UP",
            0.90,
            2.0,
            0.8,
            65.0,
            500.0,
        )
        .expect("affordable rescue gap");
        assert!(plan.clip_usd > 0.0);
    }

    #[test]
    fn flip_hedge_budget_scales_with_primary_exposure() {
        let mut j = j_cfg();
        j.flip_tier_usd = 4.0;
        j.flip_hedge_exposure_ratio = 0.25;
        j.flip_tier_max_usd = 8.0;
        let cfg = full_cfg(j);
        let state_small = JWindowState {
            rescue_spent_usd: 20.0,
            primary_side: Some("DOWN".to_string()),
            ..Default::default()
        };
        assert!((flip_hedge_budget_cap(&cfg, &state_small) - 5.0).abs() < 1e-9);
        let state_large = JWindowState {
            rescue_spent_usd: 70.0,
            primary_side: Some("DOWN".to_string()),
            ..Default::default()
        };
        assert!((flip_hedge_budget_cap(&cfg, &state_large) - 8.0).abs() < 1e-9);
    }
}
