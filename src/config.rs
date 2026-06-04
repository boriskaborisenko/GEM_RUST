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
pub struct Config {
    pub strategy: String,
    #[serde(default, deserialize_with = "deserialize_llm_config")]
    pub llm: LlmConfig,
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
