pub mod strategy_a;
pub mod strategy_b;
pub mod strategy_c;
pub mod strategy_d;

use crate::client::{MarketWindow, PricesState};
use crate::trader::WindowState;
use crate::config::Config;

#[derive(Debug, Clone)]
pub struct OrderSignal {
    pub side: String,
    pub is_buy: bool,
    pub amount: f64,
    pub price: f64,
    pub reason: String,
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
        window_number: usize,
        secs_to_start: i64,
        current_btc_atr: f64,
    ) -> Option<(f64, f64)>;

    fn process_live_tick(
        &mut self,
        config: &Config,
        prices: &PricesState,
        spot_price: Option<f64>,
        market: &MarketWindow,
        win_state: &WindowState,
        secs_to_end: i64,
        current_atr: f64,
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
            _ => Box::new(strategy_a::SimpleBothStrategy::new()),
        };
        Self { active_strategy: active }
    }

    pub fn check_pre_start_entry(
        &mut self,
        config: &Config,
        prices: &PricesState,
        window_number: usize,
        secs_to_start: i64,
        current_btc_atr: f64,
    ) -> Option<(f64, f64)> {
        self.active_strategy.check_pre_start_entry(config, prices, window_number, secs_to_start, current_btc_atr)
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
    ) -> Vec<OrderSignal> {
        self.active_strategy.process_live_tick(config, prices, spot_price, market, win_state, secs_to_end, current_atr)
    }

    pub fn get_strategy_state(&self, window_number: usize) -> Option<StrategyState> {
        self.active_strategy.get_strategy_state(window_number)
    }
}
