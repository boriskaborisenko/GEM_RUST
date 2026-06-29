use serde::{Deserialize, Deserializer};
use std::fs::File;
use std::io::Read;
use std::path::Path;

#[derive(Debug, Deserialize, Clone)]
pub struct SessionConfig {
    #[serde(rename = "startingBank")]
    pub starting_bank: f64,
    #[serde(rename = "minWindowBudget", default)]
    pub min_window_budget: f64,
    #[serde(rename = "maxWindowBudget", default)]
    pub max_window_budget: f64,
    #[serde(rename = "windowBudgetPct", default = "default_window_budget_pct")]
    pub window_budget_pct: f64,
}

#[derive(Debug, Deserialize, Clone)]
pub struct PreStartConfig {
    pub enabled: bool,
    #[serde(rename = "minSecondsBeforeStart")]
    pub min_seconds_before_start: i64,
    #[serde(rename = "maxSecondsBeforeStart")]
    pub max_seconds_before_start: i64,
    #[serde(rename = "minSideAsk")]
    pub min_side_ask: f64,
    #[serde(rename = "maxSideAsk")]
    pub max_side_ask: f64,
}

#[derive(Debug, Deserialize, Clone)]
pub struct SellStrategyConfig {
    #[serde(rename = "exitBid")]
    pub exit_bid: f64,
}

#[derive(Debug, Deserialize, Clone)]
pub struct AsymmetricLadderConfig {
    #[serde(rename = "strongSteps")]
    pub strong_steps: Vec<f64>,
    #[serde(rename = "weakSteps")]
    pub weak_steps: Vec<f64>,
    #[serde(rename = "decayEnabled")]
    pub decay_enabled: bool,
}

#[derive(Debug, Deserialize, Clone)]
pub struct DynamicBreakevenConfig {
    #[serde(rename = "slippageBuffer")]
    pub slippage_buffer: f64,
}

#[derive(Debug, Deserialize, Clone)]
pub struct LlmConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_llm_model")]
    pub model: String,
    #[serde(default = "default_llm_location")]
    pub location: String,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            model: default_llm_model(),
            location: default_llm_location(),
        }
    }
}

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionMode {
    Paper,
    Live,
}

impl Default for ExecutionMode {
    fn default() -> Self {
        Self::Paper
    }
}

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LiveSignatureType {
    Eoa,
    Proxy,
    GnosisSafe,
    Poly1271,
}

impl Default for LiveSignatureType {
    fn default() -> Self {
        Self::Poly1271
    }
}

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LiveMarketOrderType {
    Fok,
    Fak,
}

impl Default for LiveMarketOrderType {
    fn default() -> Self {
        Self::Fok
    }
}

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LiveLimitOrderType {
    Gtc,
    Gtd,
}

impl Default for LiveLimitOrderType {
    fn default() -> Self {
        Self::Gtd
    }
}

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ExecutionConfig {
    #[serde(default)]
    pub mode: ExecutionMode,
    #[serde(default = "execution_default_true")]
    pub dry_run: bool,
    #[serde(default = "execution_default_secrets_file")]
    pub secrets_file: String,
    #[serde(default = "execution_default_clob_host")]
    pub clob_host: String,
    #[serde(default)]
    pub signature_type: LiveSignatureType,
    #[serde(default)]
    pub funder_address: Option<String>,
    #[serde(default)]
    pub market_order_type: LiveMarketOrderType,
    #[serde(default)]
    pub limit_order_type: LiveLimitOrderType,
    #[serde(default)]
    pub limit_post_only: bool,
    #[serde(default = "execution_default_limit_ttl_ms")]
    pub limit_ttl_ms: i64,
    #[serde(default = "execution_default_max_order_age_ms")]
    pub max_order_age_ms: i64,
    #[serde(default = "execution_default_min_order_usd")]
    pub min_order_usd: f64,
}

impl Default for ExecutionConfig {
    fn default() -> Self {
        Self {
            mode: ExecutionMode::Paper,
            dry_run: true,
            secrets_file: execution_default_secrets_file(),
            clob_host: execution_default_clob_host(),
            signature_type: LiveSignatureType::Poly1271,
            funder_address: None,
            market_order_type: LiveMarketOrderType::Fok,
            limit_order_type: LiveLimitOrderType::Gtd,
            limit_post_only: true,
            limit_ttl_ms: execution_default_limit_ttl_ms(),
            max_order_age_ms: execution_default_max_order_age_ms(),
            min_order_usd: execution_default_min_order_usd(),
        }
    }
}

fn execution_default_true() -> bool {
    true
}

fn execution_default_secrets_file() -> String {
    ".env.live".to_string()
}

fn execution_default_clob_host() -> String {
    "https://clob.polymarket.com".to_string()
}

fn execution_default_limit_ttl_ms() -> i64 {
    25_000
}

fn execution_default_max_order_age_ms() -> i64 {
    3_000
}

fn execution_default_min_order_usd() -> f64 {
    1.0
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum LlmConfigWire {
    Bool(bool),
    Object(LlmConfig),
}

fn default_llm_model() -> String {
    "gemini-3.5-flash".to_string()
}

fn default_llm_location() -> String {
    "global".to_string()
}

fn default_zero() -> f64 {
    0.0
}

fn default_window_budget_pct() -> f64 {
    100.0
}

fn deserialize_llm_config<'de, D>(deserializer: D) -> Result<LlmConfig, D::Error>
where
    D: Deserializer<'de>,
{
    let wire = Option::<LlmConfigWire>::deserialize(deserializer)?;
    Ok(match wire {
        Some(LlmConfigWire::Bool(enabled)) => LlmConfig {
            enabled,
            ..LlmConfig::default()
        },
        Some(LlmConfigWire::Object(config)) => config,
        None => LlmConfig::default(),
    })
}

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct JEndgameConfig {
    #[serde(default = "j_default_endgame_secs")]
    pub endgame_secs: i64,
    #[serde(default = "j_default_min_winner_ask")]
    pub min_winner_ask: f64,
    #[serde(default = "j_default_max_winner_ask")]
    pub max_winner_ask: f64,
    #[serde(default = "j_default_taker_max_ask")]
    pub taker_max_ask: f64,
    #[serde(default = "j_default_min_abs_gap_z")]
    pub min_abs_gap_z: f64,
    #[serde(default = "j_default_limit_ask_offset")]
    pub limit_ask_offset: f64,
    #[serde(default = "j_default_clip_usd")]
    pub clip_usd: f64,
    #[serde(default = "j_default_max_usd_per_window")]
    pub max_usd_per_window: f64,
    #[serde(default = "j_default_max_clips")]
    pub max_clips_per_window: u16,
    #[serde(default = "j_default_sweep_clips_per_tick")]
    pub sweep_clips_per_tick: u8,
    #[serde(default = "j_default_tape_window_ms")]
    pub tape_window_ms: i64,
    #[serde(default = "j_default_min_tape_usd")]
    pub min_tape_usd: f64,
    #[serde(default = "j_default_min_tape_buys")]
    pub min_tape_buys: u32,
    #[serde(default = "j_default_true")]
    pub require_tape: bool,
    #[serde(default = "j_default_true")]
    pub taker_mode: bool,
    #[serde(default = "j_default_cheap_max_ask")]
    pub cheap_max_ask: f64,
    #[serde(default = "j_default_cheap_min_gap_z")]
    pub cheap_min_gap_z: f64,
    #[serde(default = "j_default_cheap_tier_usd")]
    pub cheap_tier_usd: f64,
    #[serde(default = "j_default_cheap_sweep_clips")]
    pub cheap_sweep_clips_per_tick: u8,
    #[serde(default)]
    pub cheap_require_tape: bool,
    #[serde(default = "j_default_late_max_secs")]
    pub late_max_secs: i64,
    #[serde(default = "j_default_late_min_gap_z")]
    pub late_min_gap_z: f64,
    #[serde(default = "j_default_late_tier_usd")]
    pub late_tier_usd: f64,
    #[serde(default = "j_default_true")]
    pub late_require_tape: bool,
    #[serde(default = "j_default_true")]
    pub impulse_enabled: bool,
    #[serde(default = "j_default_impulse_max_ask")]
    pub impulse_max_ask: f64,
    #[serde(default = "j_default_impulse_min_gap_z")]
    pub impulse_min_gap_z: f64,
    #[serde(default = "j_default_impulse_tier_usd")]
    pub impulse_tier_usd: f64,
    #[serde(default = "j_default_impulse_sweep_clips")]
    pub impulse_sweep_clips_per_tick: u8,
    #[serde(default = "j_default_true")]
    pub impulse_require_tape: bool,
    #[serde(default = "j_default_cheap_min_elapsed_pct")]
    pub cheap_min_elapsed_pct: f64,
    #[serde(default = "j_default_cheap_max_clips")]
    pub cheap_max_clips: u16,
    #[serde(default = "j_default_late_heavy_secs")]
    pub late_heavy_secs: i64,
    #[serde(default = "j_default_late_heavy_sweep_clips")]
    pub late_heavy_sweep_clips: u8,
    #[serde(default = "j_default_true")]
    pub flip_hedge_enabled: bool,
    #[serde(default = "j_default_flip_min_sig_crosses")]
    pub flip_min_sig_crosses: u32,
    #[serde(default = "j_default_flip_min_crosses")]
    pub flip_min_crosses: u32,
    #[serde(default = "j_default_flip_min_gap_z")]
    pub flip_min_gap_z: f64,
    #[serde(default = "j_default_flip_tier_usd")]
    pub flip_tier_usd: f64,
    /// Hedge budget floor scales with primary exposure: max(flip_tier_usd, primary * ratio).
    #[serde(default = "j_default_flip_hedge_exposure_ratio")]
    pub flip_hedge_exposure_ratio: f64,
    #[serde(default = "j_default_flip_tier_max_usd")]
    pub flip_tier_max_usd: f64,
    #[serde(default = "j_default_flip_hedge_clip_usd")]
    pub flip_hedge_clip_usd: f64,
    #[serde(default = "j_default_flip_sweep_clips")]
    pub flip_sweep_clips_per_tick: u8,
    #[serde(default = "j_default_flip_max_ask")]
    pub flip_max_ask: f64,
    /// Require the underlying spot to cross PTB against primary before buying
    /// the opposite leg. Mid-lead flips alone only freeze/diagnose, not hedge.
    #[serde(default = "j_default_true")]
    pub flip_require_spot_cross: bool,
    #[serde(default)]
    pub flip_require_tape: bool,
    /// Enable selling the primary leg when the thesis breaks and bid salvage beats holding loser to $0.
    #[serde(default = "j_default_true")]
    pub sell_rescue_enabled: bool,
    #[serde(default = "j_default_sell_rescue_min_bid")]
    pub sell_rescue_min_bid: f64,
    #[serde(default = "j_default_sell_rescue_min_gap_z")]
    pub sell_rescue_min_gap_z: f64,
    #[serde(default = "j_default_sell_rescue_min_value_usd")]
    pub sell_rescue_min_value_usd: f64,
    #[serde(default = "j_default_sell_rescue_min_improvement_usd")]
    pub sell_rescue_min_improvement_usd: f64,
    #[serde(default = "j_default_sell_rescue_fraction")]
    pub sell_rescue_fraction: f64,
    #[serde(default)]
    pub sell_rescue_use_market: bool,
    #[serde(default = "j_default_sell_rescue_market_secs")]
    pub sell_rescue_market_secs: i64,
    /// Skip value/late/impulse when |spot-ptb|/ptb * 100 is below this (e.g. 0.05 = 0.05%).
    #[serde(default = "j_default_min_ptb_dist_pct")]
    pub min_ptb_dist_pct: f64,
    /// Block new directional clips when significant mid crosses reach this count (flip hedge still allowed).
    #[serde(default = "j_default_max_sig_crosses_directional")]
    pub max_sig_crosses_directional: u32,
    /// Block new directional entries when raw mid-cross count reaches this (chop guard). 0 = off.
    #[serde(default = "j_default_max_crosses_directional")]
    pub max_crosses_directional: u32,
    /// Window close goal (redeem PnL).
    #[serde(default = "j_default_target_profit_usd")]
    pub target_profit_usd: f64,
    /// Hard J trading gate: ignore CLOB market ticks older than this.
    #[serde(default = "j_default_max_clob_age_ms")]
    pub max_clob_age_ms: i64,
    #[serde(default = "j_default_probe_clip_usd")]
    pub probe_clip_usd: f64,
    #[serde(default = "j_default_rescue_zone_secs")]
    pub rescue_zone_secs: i64,
    #[serde(default = "j_default_max_rescue_usd")]
    pub max_rescue_usd: f64,
    #[serde(default = "j_default_abort_rescue_if_ask_above")]
    pub abort_rescue_if_ask_above: f64,
    /// Price-tier exposure caps for primary winner buys. These trim loss tails:
    /// the more expensive the winner ask, the less USD J may deploy.
    #[serde(default = "j_default_tail_cap_ask70_usd")]
    pub tail_cap_ask70_usd: f64,
    #[serde(default = "j_default_tail_cap_ask88_usd")]
    pub tail_cap_ask88_usd: f64,
    #[serde(default = "j_default_tail_cap_ask94_usd")]
    pub tail_cap_ask94_usd: f64,
    #[serde(default = "j_default_tail_cap_ask97_usd")]
    pub tail_cap_ask97_usd: f64,
    /// Temporarily freeze fresh directional buys after a mid-price side cross.
    /// Flip hedge and sell-rescue are still allowed.
    #[serde(default = "j_default_fresh_cross_freeze_secs")]
    pub fresh_cross_freeze_secs: i64,
    /// Buy a small extra clip on the existing primary side after a deep discount,
    /// but only while that side is still the spot/PTB winner.
    #[serde(default = "j_default_true")]
    pub discount_reload_enabled: bool,
    #[serde(default = "j_default_discount_reload_max_ask")]
    pub discount_reload_max_ask: f64,
    #[serde(default = "j_default_discount_reload_min_drop")]
    pub discount_reload_min_drop: f64,
    #[serde(default = "j_default_discount_reload_min_gap_z")]
    pub discount_reload_min_gap_z: f64,
    #[serde(default = "j_default_discount_reload_clip_usd")]
    pub discount_reload_clip_usd: f64,
    #[serde(default = "j_default_discount_reload_max_usd")]
    pub discount_reload_max_usd: f64,
    #[serde(default = "j_default_discount_reload_max_clips")]
    pub discount_reload_max_clips: u16,
    #[serde(default = "j_default_true")]
    pub insurance_enabled: bool,
    #[serde(default = "j_default_insurance_max_elapsed_pct")]
    pub insurance_max_elapsed_pct: f64,
    #[serde(default = "j_default_insurance_max_ask")]
    pub insurance_max_ask: f64,
    #[serde(default = "j_default_insurance_max_ptb_dist_pct")]
    pub insurance_max_ptb_dist_pct: f64,
    #[serde(default = "j_default_insurance_max_lead_gap")]
    pub insurance_max_lead_gap: f64,
    #[serde(default = "j_default_insurance_max_clips")]
    pub insurance_max_clips: u16,
    #[serde(default = "j_default_insurance_clip_usd")]
    pub insurance_clip_usd: f64,
    #[serde(default = "j_default_insurance_tier_usd")]
    pub insurance_tier_usd: f64,
    /// Last N seconds: taker sweep winner @ up to 0.99 to lock +target.
    #[serde(default = "j_default_final_seal_secs")]
    pub final_seal_secs: i64,
    #[serde(default = "j_default_final_seal_max_ask")]
    pub final_seal_max_ask: f64,
    #[serde(default = "j_default_final_seal_min_gap_z")]
    pub final_seal_min_gap_z: f64,
    /// gap_z at which the dynamic clip reaches max size (full confidence).
    #[serde(default = "j_default_full_size_gap_z")]
    pub full_size_gap_z: f64,
    /// Upper bound for a single dynamic clip (USD).
    #[serde(default = "j_default_max_clip_usd")]
    pub max_clip_usd: f64,
    /// Percent-based sizing for one process/asset. When enabled, fixed USD fields
    /// become max clamps and effective sizes are bank_pct clamped by min/max fix.
    #[serde(default)]
    pub bank_sizing_enabled: bool,
    #[serde(default = "j_default_min_trade_usd")]
    pub min_trade_usd: f64,
    #[serde(default = "j_default_max_usd_per_window_pct")]
    pub max_usd_per_window_pct: f64,
    #[serde(default = "j_default_max_usd_per_window_min_fix")]
    pub max_usd_per_window_min_fix: f64,
    #[serde(default = "j_default_max_usd_per_window")]
    pub max_usd_per_window_max_fix: f64,
    #[serde(default = "j_default_max_rescue_usd_pct")]
    pub max_rescue_usd_pct: f64,
    #[serde(default = "j_default_max_rescue_usd_min_fix")]
    pub max_rescue_usd_min_fix: f64,
    #[serde(default = "j_default_max_rescue_usd")]
    pub max_rescue_usd_max_fix: f64,
    #[serde(default = "j_default_tail_cap_ask70_pct")]
    pub tail_cap_ask70_pct: f64,
    #[serde(default = "j_default_tail_cap_ask70_min_fix")]
    pub tail_cap_ask70_min_fix: f64,
    #[serde(default = "j_default_tail_cap_ask70_usd")]
    pub tail_cap_ask70_max_fix: f64,
    #[serde(default = "j_default_tail_cap_ask88_pct")]
    pub tail_cap_ask88_pct: f64,
    #[serde(default = "j_default_tail_cap_ask88_min_fix")]
    pub tail_cap_ask88_min_fix: f64,
    #[serde(default = "j_default_tail_cap_ask88_usd")]
    pub tail_cap_ask88_max_fix: f64,
    #[serde(default = "j_default_tail_cap_ask94_pct")]
    pub tail_cap_ask94_pct: f64,
    #[serde(default = "j_default_tail_cap_ask94_min_fix")]
    pub tail_cap_ask94_min_fix: f64,
    #[serde(default = "j_default_tail_cap_ask94_usd")]
    pub tail_cap_ask94_max_fix: f64,
    #[serde(default = "j_default_tail_cap_ask97_pct")]
    pub tail_cap_ask97_pct: f64,
    #[serde(default = "j_default_tail_cap_ask97_min_fix")]
    pub tail_cap_ask97_min_fix: f64,
    #[serde(default = "j_default_tail_cap_ask97_usd")]
    pub tail_cap_ask97_max_fix: f64,
    #[serde(default = "j_default_first_clip_pct")]
    pub first_clip_pct: f64,
    #[serde(default = "j_default_first_clip_min_fix")]
    pub first_clip_min_fix: f64,
    #[serde(default = "j_default_first_clip_usd")]
    pub first_clip_max_fix: f64,
    #[serde(default = "j_default_max_clip_pct")]
    pub max_clip_pct: f64,
    #[serde(default = "j_default_max_clip_min_fix")]
    pub max_clip_min_fix: f64,
    #[serde(default = "j_default_max_clip_usd")]
    pub max_clip_max_fix: f64,
    #[serde(default = "j_default_min_increment_pct")]
    pub min_increment_pct: f64,
    #[serde(default = "j_default_min_increment_min_fix")]
    pub min_increment_min_fix: f64,
    #[serde(default = "j_default_min_increment_usd")]
    pub min_increment_max_fix: f64,
    #[serde(default = "j_default_flip_tier_pct")]
    pub flip_tier_pct: f64,
    #[serde(default = "j_default_flip_tier_min_fix")]
    pub flip_tier_min_fix: f64,
    #[serde(default = "j_default_flip_tier_usd")]
    pub flip_tier_max_fix: f64,
    #[serde(default = "j_default_flip_tier_max_pct")]
    pub flip_tier_max_pct: f64,
    #[serde(default = "j_default_flip_tier_max_min_fix")]
    pub flip_tier_max_min_fix: f64,
    #[serde(default = "j_default_flip_tier_max_usd")]
    pub flip_tier_max_max_fix: f64,
    #[serde(default = "j_default_flip_hedge_clip_pct")]
    pub flip_hedge_clip_pct: f64,
    #[serde(default = "j_default_flip_hedge_clip_min_fix")]
    pub flip_hedge_clip_min_fix: f64,
    #[serde(default = "j_default_flip_hedge_clip_usd")]
    pub flip_hedge_clip_max_fix: f64,
    #[serde(default = "j_default_discount_reload_clip_pct")]
    pub discount_reload_clip_pct: f64,
    #[serde(default = "j_default_discount_reload_clip_min_fix")]
    pub discount_reload_clip_min_fix: f64,
    #[serde(default = "j_default_discount_reload_clip_usd")]
    pub discount_reload_clip_max_fix: f64,
    #[serde(default = "j_default_discount_reload_max_pct")]
    pub discount_reload_max_pct: f64,
    #[serde(default = "j_default_discount_reload_max_min_fix")]
    pub discount_reload_max_min_fix: f64,
    #[serde(default = "j_default_discount_reload_max_usd")]
    pub discount_reload_max_max_fix: f64,
    // ---- Composite-confidence endgame (target-exposure) ----
    /// Minimum composite confidence C (0..1) to deploy ANY endgame USD on the
    /// winner. Below this the window is treated as a coin flip and skipped.
    #[serde(default = "j_default_conf_enter")]
    pub conf_enter: f64,
    /// Weight of gap_z (winner distance from PTB / expected move) in C.
    #[serde(default = "j_default_conf_w_gap")]
    pub conf_w_gap: f64,
    /// Weight of spot momentum (Binance/Bybit smoothed velocity toward winner).
    #[serde(default = "j_default_conf_w_mom")]
    pub conf_w_mom: f64,
    /// Weight of order-book agreement (mid-cross lead on winner, chop-penalized).
    #[serde(default = "j_default_conf_w_book")]
    pub conf_w_book: f64,
    /// Weight of order-flow (tape imbalance + CEX buy/sell imbalance) toward winner.
    #[serde(default = "j_default_conf_w_flow")]
    pub conf_w_flow: f64,
    /// Spot velocity (USD/sec, toward winner) at which the momentum score saturates.
    #[serde(default = "j_default_mom_full_vel_usd_per_sec")]
    pub mom_full_vel_usd_per_sec: f64,
    /// Mid lead_gap at which the book score saturates.
    #[serde(default = "j_default_book_full_lead_gap")]
    pub book_full_lead_gap: f64,
    /// Significant mid-crosses that fully discount the book score (chop).
    #[serde(default = "j_default_book_max_sig_cross")]
    pub book_max_sig_cross: u32,
    /// If the book leads the OPPOSITE side by at least this lead_gap, veto the buy.
    #[serde(default = "j_default_book_contradict_gap")]
    pub book_contradict_gap: f64,
    #[serde(default = "j_default_final_seal_sweep_clips")]
    pub final_seal_sweep_clips: u8,
    /// Cap the very first endgame clip (USD); later clips ramp toward max_clip_usd.
    #[serde(default = "j_default_first_clip_usd")]
    pub first_clip_usd: f64,
    /// Skip follow-up buys until target − deployed ≥ this (reduces per-tick spam).
    #[serde(default = "j_default_min_increment_usd")]
    pub min_increment_usd: f64,
    /// Minimum wall-clock gap between composite endgame buys (ms).
    #[serde(default = "j_default_min_buy_interval_ms")]
    pub min_buy_interval_ms: u64,
    /// Above this winner ask, fresh entry needs at least expensive_min_gap_z.
    #[serde(default = "j_default_expensive_ask_threshold")]
    pub expensive_ask_threshold: f64,
    #[serde(default = "j_default_expensive_min_gap_z")]
    pub expensive_min_gap_z: f64,
    #[serde(default)]
    pub fee_rate_bps: Option<f64>,
}

fn j_default_endgame_secs() -> i64 {
    120
}
fn j_default_min_winner_ask() -> f64 {
    0.88
}
fn j_default_max_winner_ask() -> f64 {
    0.98
}
fn j_default_taker_max_ask() -> f64 {
    0.99
}
fn j_default_min_abs_gap_z() -> f64 {
    0.80
}
fn j_default_limit_ask_offset() -> f64 {
    0.02
}
fn j_default_clip_usd() -> f64 {
    1.0
}
fn j_default_max_usd_per_window() -> f64 {
    500.0
}
fn j_default_max_clips() -> u16 {
    0
}
fn j_default_sweep_clips_per_tick() -> u8 {
    1
}
fn j_default_tape_window_ms() -> i64 {
    5000
}
fn j_default_min_tape_usd() -> f64 {
    3.0
}
fn j_default_min_tape_buys() -> u32 {
    2
}
fn j_default_true() -> bool {
    true
}
fn j_default_cheap_max_ask() -> f64 {
    0.88
}
fn j_default_cheap_min_gap_z() -> f64 {
    1.0
}
fn j_default_cheap_tier_usd() -> f64 {
    9.0
}
fn j_default_cheap_sweep_clips() -> u8 {
    1
}
fn j_default_cheap_min_elapsed_pct() -> f64 {
    50.0
}
fn j_default_cheap_max_clips() -> u16 {
    9
}
fn j_default_late_max_secs() -> i64 {
    25
}
fn j_default_late_min_gap_z() -> f64 {
    0.85
}
fn j_default_late_tier_usd() -> f64 {
    12.0
}
fn j_default_late_heavy_secs() -> i64 {
    15
}
fn j_default_late_heavy_sweep_clips() -> u8 {
    8
}
fn j_default_impulse_max_ask() -> f64 {
    0.92
}
fn j_default_impulse_min_gap_z() -> f64 {
    1.2
}
fn j_default_impulse_tier_usd() -> f64 {
    0.0
}
fn j_default_impulse_sweep_clips() -> u8 {
    1
}
fn j_default_flip_min_sig_crosses() -> u32 {
    2
}
fn j_default_flip_min_crosses() -> u32 {
    6
}
fn j_default_flip_min_gap_z() -> f64 {
    0.4
}
fn j_default_flip_tier_usd() -> f64 {
    4.0
}
fn j_default_flip_hedge_exposure_ratio() -> f64 {
    0.25
}
fn j_default_flip_tier_max_usd() -> f64 {
    8.0
}
fn j_default_flip_hedge_clip_usd() -> f64 {
    4.0
}
fn j_default_flip_sweep_clips() -> u8 {
    1
}
fn j_default_flip_max_ask() -> f64 {
    0.85
}
fn j_default_sell_rescue_min_bid() -> f64 {
    0.20
}
fn j_default_sell_rescue_min_gap_z() -> f64 {
    1.20
}
fn j_default_sell_rescue_min_value_usd() -> f64 {
    1.0
}
fn j_default_sell_rescue_min_improvement_usd() -> f64 {
    1.0
}
fn j_default_sell_rescue_fraction() -> f64 {
    1.0
}
fn j_default_sell_rescue_market_secs() -> i64 {
    5
}
fn j_default_false() -> bool {
    false
}
fn j_default_min_ptb_dist_pct() -> f64 {
    0.05
}
fn j_default_max_sig_crosses_directional() -> u32 {
    3
}
fn j_default_max_crosses_directional() -> u32 {
    9
}
fn j_default_target_profit_usd() -> f64 {
    1.0
}
fn j_default_max_clob_age_ms() -> i64 {
    2_500
}
fn j_default_probe_clip_usd() -> f64 {
    1.0
}
fn j_default_rescue_zone_secs() -> i64 {
    20
}
fn j_default_max_rescue_usd() -> f64 {
    500.0
}
fn j_default_abort_rescue_if_ask_above() -> f64 {
    0.995
}
fn j_default_tail_cap_ask70_usd() -> f64 {
    75.0
}
fn j_default_tail_cap_ask88_usd() -> f64 {
    55.0
}
fn j_default_tail_cap_ask94_usd() -> f64 {
    32.0
}
fn j_default_tail_cap_ask97_usd() -> f64 {
    14.0
}
fn j_default_fresh_cross_freeze_secs() -> i64 {
    8
}
fn j_default_discount_reload_max_ask() -> f64 {
    0.74
}
fn j_default_discount_reload_min_drop() -> f64 {
    0.12
}
fn j_default_discount_reload_min_gap_z() -> f64 {
    1.10
}
fn j_default_discount_reload_clip_usd() -> f64 {
    4.0
}
fn j_default_discount_reload_max_usd() -> f64 {
    12.0
}
fn j_default_discount_reload_max_clips() -> u16 {
    2
}
fn j_default_insurance_max_elapsed_pct() -> f64 {
    30.0
}
fn j_default_insurance_max_ask() -> f64 {
    0.18
}
fn j_default_insurance_max_ptb_dist_pct() -> f64 {
    0.05
}
fn j_default_insurance_max_lead_gap() -> f64 {
    0.15
}
fn j_default_insurance_max_clips() -> u16 {
    2
}
fn j_default_insurance_clip_usd() -> f64 {
    1.0
}
fn j_default_insurance_tier_usd() -> f64 {
    2.0
}
fn j_default_final_seal_secs() -> i64 {
    5
}
fn j_default_final_seal_max_ask() -> f64 {
    0.99
}
fn j_default_final_seal_min_gap_z() -> f64 {
    0.35
}
fn j_default_full_size_gap_z() -> f64 {
    1.8
}
fn j_default_max_clip_usd() -> f64 {
    35.0
}
fn j_default_min_trade_usd() -> f64 {
    1.0
}
fn j_default_max_usd_per_window_pct() -> f64 {
    16.0
}
fn j_default_max_usd_per_window_min_fix() -> f64 {
    3.0
}
fn j_default_max_rescue_usd_pct() -> f64 {
    15.0
}
fn j_default_max_rescue_usd_min_fix() -> f64 {
    3.0
}
fn j_default_tail_cap_ask70_pct() -> f64 {
    15.0
}
fn j_default_tail_cap_ask70_min_fix() -> f64 {
    3.0
}
fn j_default_tail_cap_ask88_pct() -> f64 {
    11.0
}
fn j_default_tail_cap_ask88_min_fix() -> f64 {
    2.0
}
fn j_default_tail_cap_ask94_pct() -> f64 {
    6.5
}
fn j_default_tail_cap_ask94_min_fix() -> f64 {
    1.0
}
fn j_default_tail_cap_ask97_pct() -> f64 {
    3.0
}
fn j_default_tail_cap_ask97_min_fix() -> f64 {
    1.0
}
fn j_default_first_clip_pct() -> f64 {
    1.6
}
fn j_default_first_clip_min_fix() -> f64 {
    1.0
}
fn j_default_max_clip_pct() -> f64 {
    7.0
}
fn j_default_max_clip_min_fix() -> f64 {
    1.0
}
fn j_default_min_increment_pct() -> f64 {
    1.0
}
fn j_default_min_increment_min_fix() -> f64 {
    1.0
}
fn j_default_flip_tier_pct() -> f64 {
    2.0
}
fn j_default_flip_tier_min_fix() -> f64 {
    1.0
}
fn j_default_flip_tier_max_pct() -> f64 {
    4.0
}
fn j_default_flip_tier_max_min_fix() -> f64 {
    1.0
}
fn j_default_flip_hedge_clip_pct() -> f64 {
    2.0
}
fn j_default_flip_hedge_clip_min_fix() -> f64 {
    1.0
}
fn j_default_discount_reload_clip_pct() -> f64 {
    2.0
}
fn j_default_discount_reload_clip_min_fix() -> f64 {
    1.0
}
fn j_default_discount_reload_max_pct() -> f64 {
    6.0
}
fn j_default_discount_reload_max_min_fix() -> f64 {
    1.0
}
fn j_default_conf_enter() -> f64 {
    0.58
}
fn j_default_conf_w_gap() -> f64 {
    0.55
}
fn j_default_conf_w_mom() -> f64 {
    0.10
}
fn j_default_conf_w_book() -> f64 {
    0.20
}
fn j_default_conf_w_flow() -> f64 {
    0.15
}
fn j_default_mom_full_vel_usd_per_sec() -> f64 {
    2.0
}
fn j_default_book_full_lead_gap() -> f64 {
    0.15
}
fn j_default_book_max_sig_cross() -> u32 {
    3
}
fn j_default_book_contradict_gap() -> f64 {
    0.04
}
fn j_default_final_seal_sweep_clips() -> u8 {
    20
}
fn j_default_first_clip_usd() -> f64 {
    8.0
}
fn j_default_min_increment_usd() -> f64 {
    5.0
}
fn j_default_min_buy_interval_ms() -> u64 {
    3000
}
fn j_default_expensive_ask_threshold() -> f64 {
    0.94
}
fn j_default_expensive_min_gap_z() -> f64 {
    1.35
}
fn j_default_insurance_enabled() -> bool {
    false
}

impl Default for JEndgameConfig {
    fn default() -> Self {
        Self {
            endgame_secs: j_default_endgame_secs(),
            min_winner_ask: j_default_min_winner_ask(),
            max_winner_ask: j_default_max_winner_ask(),
            taker_max_ask: j_default_taker_max_ask(),
            min_abs_gap_z: j_default_min_abs_gap_z(),
            limit_ask_offset: j_default_limit_ask_offset(),
            clip_usd: j_default_clip_usd(),
            max_usd_per_window: j_default_max_usd_per_window(),
            max_clips_per_window: j_default_max_clips(),
            sweep_clips_per_tick: j_default_sweep_clips_per_tick(),
            tape_window_ms: j_default_tape_window_ms(),
            min_tape_usd: j_default_min_tape_usd(),
            min_tape_buys: j_default_min_tape_buys(),
            require_tape: j_default_true(),
            taker_mode: j_default_true(),
            cheap_max_ask: j_default_cheap_max_ask(),
            cheap_min_gap_z: j_default_cheap_min_gap_z(),
            cheap_tier_usd: j_default_cheap_tier_usd(),
            cheap_sweep_clips_per_tick: j_default_cheap_sweep_clips(),
            cheap_require_tape: false,
            late_max_secs: j_default_late_max_secs(),
            late_min_gap_z: j_default_late_min_gap_z(),
            late_tier_usd: j_default_late_tier_usd(),
            late_require_tape: j_default_true(),
            impulse_enabled: j_default_false(),
            impulse_max_ask: j_default_impulse_max_ask(),
            impulse_min_gap_z: j_default_impulse_min_gap_z(),
            impulse_tier_usd: j_default_impulse_tier_usd(),
            impulse_sweep_clips_per_tick: j_default_impulse_sweep_clips(),
            impulse_require_tape: j_default_true(),
            cheap_min_elapsed_pct: j_default_cheap_min_elapsed_pct(),
            cheap_max_clips: j_default_cheap_max_clips(),
            late_heavy_secs: j_default_late_heavy_secs(),
            late_heavy_sweep_clips: j_default_late_heavy_sweep_clips(),
            flip_hedge_enabled: j_default_true(),
            flip_min_sig_crosses: j_default_flip_min_sig_crosses(),
            flip_min_crosses: j_default_flip_min_crosses(),
            flip_min_gap_z: j_default_flip_min_gap_z(),
            flip_tier_usd: j_default_flip_tier_usd(),
            flip_hedge_exposure_ratio: j_default_flip_hedge_exposure_ratio(),
            flip_tier_max_usd: j_default_flip_tier_max_usd(),
            flip_hedge_clip_usd: j_default_flip_hedge_clip_usd(),
            flip_sweep_clips_per_tick: j_default_flip_sweep_clips(),
            flip_max_ask: j_default_flip_max_ask(),
            flip_require_spot_cross: j_default_true(),
            flip_require_tape: j_default_false(),
            sell_rescue_enabled: j_default_true(),
            sell_rescue_min_bid: j_default_sell_rescue_min_bid(),
            sell_rescue_min_gap_z: j_default_sell_rescue_min_gap_z(),
            sell_rescue_min_value_usd: j_default_sell_rescue_min_value_usd(),
            sell_rescue_min_improvement_usd: j_default_sell_rescue_min_improvement_usd(),
            sell_rescue_fraction: j_default_sell_rescue_fraction(),
            sell_rescue_use_market: false,
            sell_rescue_market_secs: j_default_sell_rescue_market_secs(),
            min_ptb_dist_pct: j_default_min_ptb_dist_pct(),
            max_sig_crosses_directional: j_default_max_sig_crosses_directional(),
            max_crosses_directional: j_default_max_crosses_directional(),
            target_profit_usd: j_default_target_profit_usd(),
            max_clob_age_ms: j_default_max_clob_age_ms(),
            probe_clip_usd: j_default_probe_clip_usd(),
            rescue_zone_secs: j_default_rescue_zone_secs(),
            max_rescue_usd: j_default_max_rescue_usd(),
            abort_rescue_if_ask_above: j_default_abort_rescue_if_ask_above(),
            tail_cap_ask70_usd: j_default_tail_cap_ask70_usd(),
            tail_cap_ask88_usd: j_default_tail_cap_ask88_usd(),
            tail_cap_ask94_usd: j_default_tail_cap_ask94_usd(),
            tail_cap_ask97_usd: j_default_tail_cap_ask97_usd(),
            fresh_cross_freeze_secs: j_default_fresh_cross_freeze_secs(),
            discount_reload_enabled: j_default_true(),
            discount_reload_max_ask: j_default_discount_reload_max_ask(),
            discount_reload_min_drop: j_default_discount_reload_min_drop(),
            discount_reload_min_gap_z: j_default_discount_reload_min_gap_z(),
            discount_reload_clip_usd: j_default_discount_reload_clip_usd(),
            discount_reload_max_usd: j_default_discount_reload_max_usd(),
            discount_reload_max_clips: j_default_discount_reload_max_clips(),
            insurance_enabled: j_default_insurance_enabled(),
            insurance_max_elapsed_pct: j_default_insurance_max_elapsed_pct(),
            insurance_max_ask: j_default_insurance_max_ask(),
            insurance_max_ptb_dist_pct: j_default_insurance_max_ptb_dist_pct(),
            insurance_max_lead_gap: j_default_insurance_max_lead_gap(),
            insurance_max_clips: j_default_insurance_max_clips(),
            insurance_clip_usd: j_default_insurance_clip_usd(),
            insurance_tier_usd: j_default_insurance_tier_usd(),
            final_seal_secs: j_default_final_seal_secs(),
            final_seal_max_ask: j_default_final_seal_max_ask(),
            final_seal_min_gap_z: j_default_final_seal_min_gap_z(),
            full_size_gap_z: j_default_full_size_gap_z(),
            max_clip_usd: j_default_max_clip_usd(),
            bank_sizing_enabled: false,
            min_trade_usd: j_default_min_trade_usd(),
            max_usd_per_window_pct: j_default_max_usd_per_window_pct(),
            max_usd_per_window_min_fix: j_default_max_usd_per_window_min_fix(),
            max_usd_per_window_max_fix: j_default_max_usd_per_window(),
            max_rescue_usd_pct: j_default_max_rescue_usd_pct(),
            max_rescue_usd_min_fix: j_default_max_rescue_usd_min_fix(),
            max_rescue_usd_max_fix: j_default_max_rescue_usd(),
            tail_cap_ask70_pct: j_default_tail_cap_ask70_pct(),
            tail_cap_ask70_min_fix: j_default_tail_cap_ask70_min_fix(),
            tail_cap_ask70_max_fix: j_default_tail_cap_ask70_usd(),
            tail_cap_ask88_pct: j_default_tail_cap_ask88_pct(),
            tail_cap_ask88_min_fix: j_default_tail_cap_ask88_min_fix(),
            tail_cap_ask88_max_fix: j_default_tail_cap_ask88_usd(),
            tail_cap_ask94_pct: j_default_tail_cap_ask94_pct(),
            tail_cap_ask94_min_fix: j_default_tail_cap_ask94_min_fix(),
            tail_cap_ask94_max_fix: j_default_tail_cap_ask94_usd(),
            tail_cap_ask97_pct: j_default_tail_cap_ask97_pct(),
            tail_cap_ask97_min_fix: j_default_tail_cap_ask97_min_fix(),
            tail_cap_ask97_max_fix: j_default_tail_cap_ask97_usd(),
            first_clip_pct: j_default_first_clip_pct(),
            first_clip_min_fix: j_default_first_clip_min_fix(),
            first_clip_max_fix: j_default_first_clip_usd(),
            max_clip_pct: j_default_max_clip_pct(),
            max_clip_min_fix: j_default_max_clip_min_fix(),
            max_clip_max_fix: j_default_max_clip_usd(),
            min_increment_pct: j_default_min_increment_pct(),
            min_increment_min_fix: j_default_min_increment_min_fix(),
            min_increment_max_fix: j_default_min_increment_usd(),
            flip_tier_pct: j_default_flip_tier_pct(),
            flip_tier_min_fix: j_default_flip_tier_min_fix(),
            flip_tier_max_fix: j_default_flip_tier_usd(),
            flip_tier_max_pct: j_default_flip_tier_max_pct(),
            flip_tier_max_min_fix: j_default_flip_tier_max_min_fix(),
            flip_tier_max_max_fix: j_default_flip_tier_max_usd(),
            flip_hedge_clip_pct: j_default_flip_hedge_clip_pct(),
            flip_hedge_clip_min_fix: j_default_flip_hedge_clip_min_fix(),
            flip_hedge_clip_max_fix: j_default_flip_hedge_clip_usd(),
            discount_reload_clip_pct: j_default_discount_reload_clip_pct(),
            discount_reload_clip_min_fix: j_default_discount_reload_clip_min_fix(),
            discount_reload_clip_max_fix: j_default_discount_reload_clip_usd(),
            discount_reload_max_pct: j_default_discount_reload_max_pct(),
            discount_reload_max_min_fix: j_default_discount_reload_max_min_fix(),
            discount_reload_max_max_fix: j_default_discount_reload_max_usd(),
            conf_enter: j_default_conf_enter(),
            conf_w_gap: j_default_conf_w_gap(),
            conf_w_mom: j_default_conf_w_mom(),
            conf_w_book: j_default_conf_w_book(),
            conf_w_flow: j_default_conf_w_flow(),
            mom_full_vel_usd_per_sec: j_default_mom_full_vel_usd_per_sec(),
            book_full_lead_gap: j_default_book_full_lead_gap(),
            book_max_sig_cross: j_default_book_max_sig_cross(),
            book_contradict_gap: j_default_book_contradict_gap(),
            final_seal_sweep_clips: j_default_final_seal_sweep_clips(),
            first_clip_usd: j_default_first_clip_usd(),
            min_increment_usd: j_default_min_increment_usd(),
            min_buy_interval_ms: j_default_min_buy_interval_ms(),
            expensive_ask_threshold: j_default_expensive_ask_threshold(),
            expensive_min_gap_z: j_default_expensive_min_gap_z(),
            fee_rate_bps: None,
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    pub strategy: String,
    #[serde(default, deserialize_with = "deserialize_llm_config")]
    pub llm: LlmConfig,
    #[serde(rename = "minBtcAtr")]
    pub min_btc_atr: f64,
    #[serde(rename = "minEthAtr", default = "default_zero")]
    pub min_eth_atr: f64,
    pub session: SessionConfig,
    #[serde(rename = "preStartEntry")]
    pub pre_start_entry: PreStartConfig,
    #[serde(rename = "sellStrategy")]
    pub sell_strategy: SellStrategyConfig,
    #[serde(rename = "asymmetricLadder")]
    pub asymmetric_ladder: Option<AsymmetricLadderConfig>,
    #[serde(rename = "dynamicBreakeven")]
    pub dynamic_breakeven: Option<DynamicBreakevenConfig>,
    #[serde(rename = "exitBeforeEndSeconds")]
    pub exit_before_end_seconds: i64,
    #[serde(rename = "forceCloseAtEnd")]
    pub force_close_at_end: bool,
    #[serde(default)]
    pub execution: ExecutionConfig,
    #[serde(rename = "jEndgame", default)]
    pub j_endgame: JEndgameConfig,
}

impl Config {
    pub fn load<P: AsRef<Path>>(path: P) -> anyhow::Result<Self> {
        let mut file = File::open(path)?;
        let mut contents = String::new();
        file.read_to_string(&mut contents)?;
        let config: Config = serde_json::from_str(&contents)?;
        Ok(config)
    }

    pub fn min_atr_for(&self, asset: &str) -> f64 {
        match asset.to_uppercase().as_str() {
            "ETH" => self.min_eth_atr,
            _ => self.min_btc_atr,
        }
    }
}

impl JEndgameConfig {
    fn sized_usd(
        &self,
        session: &SessionConfig,
        fixed: f64,
        pct: f64,
        min_fix: f64,
        max_fix: f64,
    ) -> f64 {
        if !self.bank_sizing_enabled {
            return fixed.max(0.0);
        }
        let bank = session.starting_bank.max(0.0);
        let min_fix = min_fix.max(self.min_trade_usd.max(1.0));
        let max_fix = max_fix.max(min_fix);
        (bank * pct.max(0.0) / 100.0).clamp(min_fix, max_fix)
    }

    pub fn effective_probe_clip_usd(&self, session: &SessionConfig) -> f64 {
        if self.bank_sizing_enabled {
            self.min_trade_usd.max(1.0)
        } else {
            self.probe_clip_usd.max(1e-9)
        }
    }

    pub fn effective_max_usd_per_window(&self, session: &SessionConfig) -> f64 {
        self.sized_usd(
            session,
            self.max_usd_per_window,
            self.max_usd_per_window_pct,
            self.max_usd_per_window_min_fix,
            self.max_usd_per_window_max_fix,
        )
    }

    pub fn effective_max_rescue_usd(&self, session: &SessionConfig) -> f64 {
        self.sized_usd(
            session,
            self.max_rescue_usd,
            self.max_rescue_usd_pct,
            self.max_rescue_usd_min_fix,
            self.max_rescue_usd_max_fix,
        )
    }

    pub fn effective_tail_cap_ask70_usd(&self, session: &SessionConfig) -> f64 {
        self.sized_usd(
            session,
            self.tail_cap_ask70_usd,
            self.tail_cap_ask70_pct,
            self.tail_cap_ask70_min_fix,
            self.tail_cap_ask70_max_fix,
        )
    }

    pub fn effective_tail_cap_ask88_usd(&self, session: &SessionConfig) -> f64 {
        self.sized_usd(
            session,
            self.tail_cap_ask88_usd,
            self.tail_cap_ask88_pct,
            self.tail_cap_ask88_min_fix,
            self.tail_cap_ask88_max_fix,
        )
    }

    pub fn effective_tail_cap_ask94_usd(&self, session: &SessionConfig) -> f64 {
        self.sized_usd(
            session,
            self.tail_cap_ask94_usd,
            self.tail_cap_ask94_pct,
            self.tail_cap_ask94_min_fix,
            self.tail_cap_ask94_max_fix,
        )
    }

    pub fn effective_tail_cap_ask97_usd(&self, session: &SessionConfig) -> f64 {
        self.sized_usd(
            session,
            self.tail_cap_ask97_usd,
            self.tail_cap_ask97_pct,
            self.tail_cap_ask97_min_fix,
            self.tail_cap_ask97_max_fix,
        )
    }

    pub fn effective_first_clip_usd(&self, session: &SessionConfig) -> f64 {
        self.sized_usd(
            session,
            self.first_clip_usd,
            self.first_clip_pct,
            self.first_clip_min_fix,
            self.first_clip_max_fix,
        )
    }

    pub fn effective_max_clip_usd(&self, session: &SessionConfig) -> f64 {
        self.sized_usd(
            session,
            self.max_clip_usd,
            self.max_clip_pct,
            self.max_clip_min_fix,
            self.max_clip_max_fix,
        )
    }

    pub fn effective_min_increment_usd(&self, session: &SessionConfig) -> f64 {
        self.sized_usd(
            session,
            self.min_increment_usd,
            self.min_increment_pct,
            self.min_increment_min_fix,
            self.min_increment_max_fix,
        )
    }

    pub fn effective_flip_tier_usd(&self, session: &SessionConfig) -> f64 {
        self.sized_usd(
            session,
            self.flip_tier_usd,
            self.flip_tier_pct,
            self.flip_tier_min_fix,
            self.flip_tier_max_fix,
        )
    }

    pub fn effective_flip_tier_max_usd(&self, session: &SessionConfig) -> f64 {
        self.sized_usd(
            session,
            self.flip_tier_max_usd,
            self.flip_tier_max_pct,
            self.flip_tier_max_min_fix,
            self.flip_tier_max_max_fix,
        )
    }

    pub fn effective_flip_hedge_clip_usd(&self, session: &SessionConfig) -> f64 {
        self.sized_usd(
            session,
            self.flip_hedge_clip_usd,
            self.flip_hedge_clip_pct,
            self.flip_hedge_clip_min_fix,
            self.flip_hedge_clip_max_fix,
        )
    }

    pub fn effective_discount_reload_clip_usd(&self, session: &SessionConfig) -> f64 {
        self.sized_usd(
            session,
            self.discount_reload_clip_usd,
            self.discount_reload_clip_pct,
            self.discount_reload_clip_min_fix,
            self.discount_reload_clip_max_fix,
        )
    }

    pub fn effective_discount_reload_max_usd(&self, session: &SessionConfig) -> f64 {
        self.sized_usd(
            session,
            self.discount_reload_max_usd,
            self.discount_reload_max_pct,
            self.discount_reload_max_min_fix,
            self.discount_reload_max_max_fix,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn j_endgame_reads_camel_case_from_config_json() {
        let cfg = Config::load("config.json").expect("config.json");
        assert!(
            (cfg.j_endgame.clip_usd - 10.0).abs() < 1e-9,
            "clip_usd={}",
            cfg.j_endgame.clip_usd
        );
        assert!((cfg.session.starting_bank - 100.0).abs() < 1e-9);
        assert!((cfg.session.min_window_budget - 0.0).abs() < 1e-9);
        assert!((cfg.session.max_window_budget - 0.0).abs() < 1e-9);
        assert!((cfg.session.window_budget_pct - 100.0).abs() < 1e-9);
        assert_eq!(cfg.j_endgame.max_clips_per_window, 0);
        assert!((cfg.j_endgame.max_usd_per_window - 80.0).abs() < 1e-9);
        assert!((cfg.j_endgame.max_rescue_usd - 75.0).abs() < 1e-9);
        assert!(cfg.j_endgame.bank_sizing_enabled);
        assert!((cfg.j_endgame.effective_max_usd_per_window(&cfg.session) - 16.0).abs() < 1e-9);
        assert!((cfg.j_endgame.effective_max_rescue_usd(&cfg.session) - 15.0).abs() < 1e-9);
        assert!((cfg.j_endgame.effective_first_clip_usd(&cfg.session) - 1.6).abs() < 1e-9);
        assert!((cfg.j_endgame.effective_max_clip_usd(&cfg.session) - 7.0).abs() < 1e-9);
        assert!(
            (cfg.j_endgame
                .effective_discount_reload_clip_usd(&cfg.session)
                - 2.0)
                .abs()
                < 1e-9
        );
        assert!((cfg.j_endgame.conf_enter - 0.58).abs() < 1e-9);
        assert!((cfg.j_endgame.max_clip_usd - 35.0).abs() < 1e-9);
        assert!((cfg.j_endgame.insurance_max_ask - 0.18).abs() < 1e-9);
        assert!((cfg.j_endgame.final_seal_max_ask - 0.99).abs() < 1e-9);
        assert!((cfg.j_endgame.min_ptb_dist_pct - 0.05).abs() < 1e-9);
        assert!((cfg.j_endgame.cheap_tier_usd - 9.0).abs() < 1e-9);
        assert!((cfg.j_endgame.late_tier_usd - 12.0).abs() < 1e-9);
        assert_eq!(cfg.j_endgame.cheap_max_clips, 9);
        assert!((cfg.j_endgame.insurance_clip_usd - 1.0).abs() < 1e-9);
        assert!((cfg.j_endgame.probe_clip_usd - 1.0).abs() < 1e-9);
        assert!(!cfg.j_endgame.impulse_enabled);
    }
}
