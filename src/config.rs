use serde::Deserialize;
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
    #[serde(rename = "cheaperSideRatio")]
    pub cheaper_side_ratio: f64,
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
pub struct Config {
    pub strategy: String,
    #[serde(rename = "minBtcAtr")]
    pub min_btc_atr: f64,
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
}

impl Config {
    pub fn load<P: AsRef<Path>>(path: P) -> anyhow::Result<Self> {
        let mut file = File::open(path)?;
        let mut contents = String::new();
        file.read_to_string(&mut contents)?;
        let config: Config = serde_json::from_str(&contents)?;
        Ok(config)
    }
}
