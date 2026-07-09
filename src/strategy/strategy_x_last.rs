use crate::client::{MarketWindow, PricesState};
use crate::config::{Config, ExecutionMode};
use crate::strategy::{
    CexMicroSnapshot, MidCrossSnapshot, OrderSignal, OrderType, SpotSignalSnapshot, StrategyState,
    TradeStrategy,
};
use crate::trader::WindowState;
use std::collections::HashMap;

const X_MIN_TRADEABLE_WINDOW: usize = 1;
const EPS: f64 = 1e-9;

#[derive(Debug, Clone, Default)]
struct XLastWindowState {
    entry_side: Option<String>,
    entry_done: bool,
}

pub struct XLastStrategy {
    windows: HashMap<usize, XLastWindowState>,
    runtime_cash: f64,
}

impl XLastStrategy {
    pub fn new() -> Self {
        Self {
            windows: HashMap::new(),
            runtime_cash: 0.0,
        }
    }
}

fn current_itm_side(spot: f64, ptb: f64) -> Option<&'static str> {
    if spot > ptb {
        Some("UP")
    } else if spot < ptb {
        Some("DOWN")
    } else {
        None
    }
}

fn best_ask(side: &str, prices: &PricesState) -> f64 {
    match side {
        "UP" => book_or_top_ask(prices.up.book.best_ask(), prices.up.ask),
        "DOWN" => book_or_top_ask(prices.down.book.best_ask(), prices.down.ask),
        _ => 0.0,
    }
}

fn book_or_top_ask(book_ask: f64, top_ask: f64) -> f64 {
    if book_ask > 0.0 {
        book_ask
    } else {
        top_ask
    }
}

impl TradeStrategy for XLastStrategy {
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
        _mid_cross: &MidCrossSnapshot,
        _cex_micro: &CexMicroSnapshot,
        _tape: &crate::trade_tape::TradeTapeSnapshot,
    ) -> Vec<OrderSignal> {
        let mut signals = Vec::new();
        let window_number = win_state.window_number;
        if window_number < X_MIN_TRADEABLE_WINDOW {
            return signals;
        }
        if config.execution.mode != ExecutionMode::Paper {
            return signals;
        }
        if secs_to_end <= 0 || secs_to_end > config.x_last.entry_secs {
            return signals;
        }

        let state = self.windows.entry(window_number).or_default();
        if state.entry_done || win_state.up_shares > EPS || win_state.down_shares > EPS {
            return signals;
        }

        let (Some(spot), Some(ptb)) = (spot_price, market.price_to_beat) else {
            return signals;
        };
        let Some(side) = current_itm_side(spot, ptb) else {
            return signals;
        };

        let ask = best_ask(side, prices);
        if ask <= 0.0 || ask > config.x_last.max_ask + EPS {
            return signals;
        }

        let min_trade = config.execution.min_order_usd.max(1.0);
        let amount = config.x_last.clip_usd.min(self.runtime_cash.max(0.0));
        if amount + EPS < min_trade {
            return signals;
        }

        signals.push(OrderSignal {
            side: side.to_string(),
            is_buy: true,
            order_type: OrderType::Market,
            amount,
            price: ask,
            reason: format!(
                "x_last_buy_{}_ask_{:.2}_spot_{:.2}_ptb_{:.2}_atr_{:.1}_secs_{}",
                side.to_lowercase(),
                ask,
                spot,
                ptb,
                current_atr,
                secs_to_end
            ),
        });
        signals
    }

    fn get_strategy_state(&self, window_number: usize) -> Option<StrategyState> {
        self.windows.get(&window_number).map(|s| StrategyState {
            up_sold: false,
            down_sold: false,
            first_sold_side: None,
            ptb_crossed: false,
            ptb_baseline: None,
            e_conviction_side: None,
            e_tranches_done: 0,
            e_grid_steps_done: 0,
            h_entry_side: s.entry_side.clone(),
            h_entry_done: s.entry_done,
            h_salvage_done: false,
        })
    }

    fn set_runtime_cash(&mut self, cash: f64) {
        self.runtime_cash = cash.max(0.0);
    }

    fn notify_order_executed(&mut self, window_number: usize, signal: &OrderSignal) {
        if !signal.reason.starts_with("x_last_") || !signal.is_buy {
            return;
        }
        let state = self.windows.entry(window_number).or_default();
        state.entry_side = Some(signal.side.clone());
        state.entry_done = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{ContractPrices, MarketWindow, TokenInfo, TokensMap};
    use crate::config::{
        ExecutionConfig, JEndgameConfig, LlmConfig, PreStartConfig, SellStrategyConfig,
        SessionConfig, XLastConfig,
    };

    fn config() -> Config {
        Config {
            strategy: "x_last".to_string(),
            llm: LlmConfig::default(),
            min_btc_atr: 0.0,
            min_eth_atr: 0.0,
            session: SessionConfig {
                starting_bank: 100.0,
                min_window_budget: 0.0,
                max_window_budget: 0.0,
                window_budget_pct: 100.0,
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
            execution: ExecutionConfig {
                dry_run: false,
                ..ExecutionConfig::default()
            },
            j_endgame: JEndgameConfig::default(),
            x_last: XLastConfig::default(),
        }
    }

    fn market() -> MarketWindow {
        MarketWindow {
            id: "m".into(),
            slug: "m".into(),
            question: "m".into(),
            asset: "BTC".into(),
            interval: "5m".into(),
            start_time: "2026-07-09T00:00:00Z".into(),
            end_time: "2026-07-09T00:05:00Z".into(),
            price_to_beat: Some(100.0),
            tokens: TokensMap {
                up: TokenInfo {
                    token_id: "up".into(),
                    outcome_name: "Up".into(),
                },
                down: TokenInfo {
                    token_id: "down".into(),
                    outcome_name: "Down".into(),
                },
            },
        }
    }

    fn prices(up_ask: f64, down_ask: f64) -> PricesState {
        PricesState {
            up: ContractPrices {
                ask: up_ask,
                bid: (up_ask - 0.01).max(0.0),
                ..ContractPrices::default()
            },
            down: ContractPrices {
                ask: down_ask,
                bid: (down_ask - 0.01).max(0.0),
                ..ContractPrices::default()
            },
        }
    }

    fn window_state() -> WindowState {
        WindowState {
            window_number: 1,
            role: "CURRENT".to_string(),
            status: "LIVE".to_string(),
            market: market(),
            spent: 0.0,
            cash_returned: 0.0,
            up_shares: 0.0,
            down_shares: 0.0,
            initial_up_shares: 0.0,
            initial_down_shares: 0.0,
            trades: vec![],
            prices: prices(0.5, 0.5),
        }
    }

    #[test]
    fn buys_itm_side_inside_last_ten_seconds_when_ask_allows() {
        let mut strat = XLastStrategy::new();
        strat.set_runtime_cash(100.0);
        let signals = strat.process_live_tick(
            &config(),
            &prices(0.98, 0.03),
            Some(101.0),
            &market(),
            &window_state(),
            10,
            30.0,
            SpotSignalSnapshot::default(),
            &MidCrossSnapshot::default(),
            &CexMicroSnapshot::default(),
            &crate::trade_tape::TradeTapeSnapshot::default(),
        );
        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0].side, "UP");
        assert!((signals[0].amount - 30.0).abs() < 1e-9);
        assert!(signals[0].reason.starts_with("x_last_"));
    }

    #[test]
    fn skips_when_current_winner_ask_is_above_max() {
        let mut strat = XLastStrategy::new();
        strat.set_runtime_cash(100.0);
        let signals = strat.process_live_tick(
            &config(),
            &prices(1.0, 0.01),
            Some(101.0),
            &market(),
            &window_state(),
            5,
            30.0,
            SpotSignalSnapshot::default(),
            &MidCrossSnapshot::default(),
            &CexMicroSnapshot::default(),
            &crate::trade_tape::TradeTapeSnapshot::default(),
        );
        assert!(signals.is_empty());
    }
}
