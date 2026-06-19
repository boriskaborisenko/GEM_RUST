use serde::{Deserialize, Deserializer};
use std::fs::File;
use std::io::Read;
use std::path::Path;

#[derive(Debug, Deserialize, Clone)]
pub struct SessionConfig {
    #[serde(rename = "startingBank")]
    pub starting_bank: f64,
    #[serde(rename = "minWindowBudget")]
    pub min_window_budget: f64,
    #[serde(rename = "maxWindowBudget")]
    pub max_window_budget: f64,
    #[serde(rename = "windowBudgetPct")]
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
    #[serde(default = "j_default_flip_sweep_clips")]
    pub flip_sweep_clips_per_tick: u8,
    #[serde(default = "j_default_flip_max_ask")]
    pub flip_max_ask: f64,
    #[serde(default)]
    pub flip_require_tape: bool,
    /// Skip value/late/impulse when |spot-ptb|/ptb * 100 is below this (e.g. 0.05 = 0.05%).
    #[serde(default = "j_default_min_ptb_dist_pct")]
    pub min_ptb_dist_pct: f64,
    /// Block new directional clips when significant mid crosses reach this count (flip hedge still allowed).
    #[serde(default = "j_default_max_sig_crosses_directional")]
    pub max_sig_crosses_directional: u32,
    /// Window close goal (redeem PnL).
    #[serde(default = "j_default_target_profit_usd")]
    pub target_profit_usd: f64,
    #[serde(default = "j_default_probe_clip_usd")]
    pub probe_clip_usd: f64,
    #[serde(default = "j_default_rescue_zone_secs")]
    pub rescue_zone_secs: i64,
    #[serde(default = "j_default_max_rescue_usd")]
    pub max_rescue_usd: f64,
    #[serde(default = "j_default_abort_rescue_if_ask_above")]
    pub abort_rescue_if_ask_above: f64,
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
    12.0
}
fn j_default_flip_sweep_clips() -> u8 {
    4
}
fn j_default_flip_max_ask() -> f64 {
    0.97
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
fn j_default_target_profit_usd() -> f64 {
    1.0
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
            flip_sweep_clips_per_tick: j_default_flip_sweep_clips(),
            flip_max_ask: j_default_flip_max_ask(),
            flip_require_tape: j_default_false(),
            min_ptb_dist_pct: j_default_min_ptb_dist_pct(),
            max_sig_crosses_directional: j_default_max_sig_crosses_directional(),
            target_profit_usd: j_default_target_profit_usd(),
            probe_clip_usd: j_default_probe_clip_usd(),
            rescue_zone_secs: j_default_rescue_zone_secs(),
            max_rescue_usd: j_default_max_rescue_usd(),
            abort_rescue_if_ask_above: j_default_abort_rescue_if_ask_above(),
            insurance_enabled: j_default_true(),
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
        assert!((cfg.session.starting_bank - 500.0).abs() < 1e-9);
        assert_eq!(cfg.j_endgame.max_clips_per_window, 0);
        assert!((cfg.j_endgame.max_usd_per_window - 80.0).abs() < 1e-9);
        assert!((cfg.j_endgame.max_rescue_usd - 75.0).abs() < 1e-9);
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
