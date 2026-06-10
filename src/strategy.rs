pub mod strategy_a;
pub mod strategy_b;
pub mod strategy_c;
pub mod strategy_d;
pub mod strategy_d1;
pub mod strategy_d_cross;
pub mod strategy_dx;

pub use crate::cex_micro::CexMicroSnapshot;
pub use crate::mid_cross_tracker::MidCrossSnapshot;

use crate::client::{MarketWindow, PricesState};
use crate::config::Config;
use crate::trader::WindowState;

pub const LEGACY_CHEAPER_SIDE_RATIO: f64 = 0.60;

#[derive(Debug, Clone)]
pub struct OrderSignal {
    pub side: String,
    pub is_buy: bool,
    pub amount: f64,
    pub price: f64,
    pub reason: String,
}

#[derive(Debug, Clone)]
pub enum EntryMode {
    Both,
    OneSide(String),
}

#[derive(Debug, Clone)]
pub struct EntrySignal {
    pub up_ask: f64,
    pub down_ask: f64,
    pub budget_multiplier: f64,
    pub cheaper_side_ratio: f64,
    pub mode: EntryMode,
    pub reason: String,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SpotSignalSnapshot {
    pub raw_velocity_usd_per_sec: Option<f64>,
    pub smoothed_velocity_usd_per_sec: Option<f64>,
    pub acceleration_usd_per_sec2: Option<f64>,
}

#[derive(Debug, Clone, Default)]
pub struct LlmForecast {
    pub side: String,
    pub confidence: f64,
    pub signal_strength: String,
    pub reason_short: String,
    pub key_drivers: Vec<String>,
    pub risk_note: String,
}

#[derive(Debug, Clone)]
pub struct StrategyState {
    pub up_sold: bool,
    pub down_sold: bool,
    pub first_sold_side: Option<String>,
    pub ptb_crossed: bool,
    pub ptb_baseline: Option<String>,
}

// ─── ИНТЕРФЕЙС ПЛАГИНОВ СТРАТЕГИЙ (Strategy Trait) ───
pub trait TradeStrategy {
    fn check_pre_start_entry(
        &mut self,
        config: &Config,
        prices: &PricesState,
        market: &MarketWindow,
        spot_price: Option<f64>,
        window_number: usize,
        secs_to_start: i64,
        current_btc_atr: f64,
        spot_signal: SpotSignalSnapshot,
        llm_forecast: Option<LlmForecast>,
    ) -> Option<EntrySignal>;

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
    ) -> Vec<OrderSignal>;

    fn get_strategy_state(&self, window_number: usize) -> Option<StrategyState>;
}

// ─── ДИСПЕТЧЕР СТРАТЕГИЙ / STRATBOX (StrategyEngine) ───
pub struct StrategyEngine {
    pub active_strategy: Box<dyn TradeStrategy + Send>,
}

impl StrategyEngine {
    pub fn new(strategy_name: &str) -> Self {
        let active: Box<dyn TradeStrategy + Send> = match strategy_name {
            "asymmetric_ladder" => Box::new(strategy_b::AsymmetricLadderStrategy::new()),
            "dynamic_breakeven" => Box::new(strategy_c::DynamicBreakEvenStrategy::new()),
            "dynamic_grid" => Box::new(strategy_d::DynamicGridStrategy::new()),
            "dynamic_grid_d1" => Box::new(strategy_d1::DynamicGridD1Strategy::new()),
            "dynamic_grid_dx" => Box::new(strategy_dx::DynamicGridDxStrategy::new()),
            "dynamic_grid_dcross" => Box::new(strategy_d_cross::DynamicGridDCrossStrategy::new()),
            _ => Box::new(strategy_a::SimpleBothStrategy::new()),
        };
        Self {
            active_strategy: active,
        }
    }

    pub fn check_pre_start_entry(
        &mut self,
        config: &Config,
        prices: &PricesState,
        market: &MarketWindow,
        spot_price: Option<f64>,
        window_number: usize,
        secs_to_start: i64,
        current_btc_atr: f64,
        spot_signal: SpotSignalSnapshot,
        llm_forecast: Option<LlmForecast>,
    ) -> Option<EntrySignal> {
        self.active_strategy.check_pre_start_entry(
            config,
            prices,
            market,
            spot_price,
            window_number,
            secs_to_start,
            current_btc_atr,
            spot_signal,
            llm_forecast,
        )
    }

    pub fn process_live_tick(
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
        self.active_strategy.process_live_tick(
            config,
            prices,
            spot_price,
            market,
            win_state,
            secs_to_end,
            current_atr,
            spot_signal,
            mid_cross,
            cex_micro,
        )
    }

    pub fn get_strategy_state(&self, window_number: usize) -> Option<StrategyState> {
        self.active_strategy.get_strategy_state(window_number)
    }
}
