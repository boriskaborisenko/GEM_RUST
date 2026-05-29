use crate::client::{MarketWindow, PricesState};
use crate::trader::WindowState;
use crate::config::Config;
use crate::strategy::{OrderSignal, StrategyState, TradeStrategy};
use std::collections::HashMap;

// ─── СТРАТЕГИЯ А: Базовая «тупая» (Simple Both Strategy) ───
pub struct SimpleBothStrategy {
    pub entered_windows: std::collections::HashSet<usize>,
    pub states: HashMap<usize, StrategyState>,
}

impl SimpleBothStrategy {
    pub fn new() -> Self {
        Self {
            entered_windows: std::collections::HashSet::new(),
            states: HashMap::new(),
        }
    }
}

impl TradeStrategy for SimpleBothStrategy {
    /**
     * Check pre-start entry conditions for a NEXT window.
     * Ratio must be EXACTLY 50/51 or 51/50 before window starts.
     */
    fn check_pre_start_entry(
        &mut self,
        config: &Config,
        prices: &PricesState,
        window_number: usize,
        secs_to_start: i64,
        _current_btc_atr: f64,
    ) -> Option<(f64, f64)> {
        if !config.pre_start_entry.enabled {
            return None;
        }
        if self.entered_windows.contains(&window_number) {
            return None;
        }

        // Must not have started yet (at least 5s before start)
        if secs_to_start < 5 {
            return None;
        }

        let up_ask = prices.up.ask;
        let dn_ask = prices.down.ask;

        if up_ask <= 0.0 || dn_ask <= 0.0 {
            return None;
        }

        // Допускаем сбалансированное отклонение +/-1 цент для гарантированного входа
        let min_ask = 0.49;
        let max_ask = 0.52;
        if up_ask < min_ask || up_ask > max_ask || dn_ask < min_ask || dn_ask > max_ask {
            return None;
        }

        self.entered_windows.insert(window_number);
        Some((up_ask, dn_ask))
    }

    /**
     * Evaluate live tick exit rules for a CURRENT window.
     */
    fn process_live_tick(
        &mut self,
        config: &Config,
        prices: &PricesState,
        _spot_price: Option<f64>,
        market: &MarketWindow,
        win_state: &WindowState,
        secs_to_end: i64,
    ) -> Vec<OrderSignal> {
        let mut signals = vec![];
        let window_number = win_state.window_number;

        let up_bid = prices.up.bid;
        let dn_bid = prices.down.bid;
        let target_bid = config.sell_strategy.exit_bid;

        let state = self.states.entry(window_number).or_insert(StrategyState {
            up_sold: false,
            down_sold: false,
            first_sold_side: None,
            ptb_crossed: false,
            ptb_baseline: None,
        });

        // ─── EMERGENCY RULE: 15% remaining time stop (Unconditional!) ───────
        let duration_ms = match (
            chrono::DateTime::parse_from_rfc3339(&market.start_time),
            chrono::DateTime::parse_from_rfc3339(&market.end_time),
        ) {
            (Ok(s), Ok(e)) => (e.timestamp_millis() - s.timestamp_millis()) as f64,
            _ => 300_000.0, // 5m fallback
        };
        let duration_sec = duration_ms / 1000.0;
        let emergency_time_threshold = (duration_sec * 0.15) as i64; // 45s for 5m, 135s for 15m

        if secs_to_end <= emergency_time_threshold && secs_to_end > -10 {
            // Sell whatever remains, if its bid price is at least 0.20
            if win_state.up_shares > 0.0 && up_bid >= 0.20 && !state.up_sold {
                state.up_sold = true;
                signals.push(OrderSignal {
                    side: "UP".to_string(),
                    is_buy: false,
                    amount: win_state.up_shares,
                    price: up_bid,
                    reason: "emergency_15pct_time_stop_bid_ge_0.20".to_string(),
                });
            }
            if win_state.down_shares > 0.0 && dn_bid >= 0.20 && !state.down_sold {
                state.down_sold = true;
                signals.push(OrderSignal {
                    side: "DOWN".to_string(),
                    is_buy: false,
                    amount: win_state.down_shares,
                    price: dn_bid,
                    reason: "emergency_15pct_time_stop_bid_ge_0.20".to_string(),
                });
            }
        }

        // ─── SIMPLE EXIT RULE: Sell 100% of any side that reaches >= target_bid ───
        if up_bid >= target_bid && !state.up_sold && win_state.up_shares > 0.0 {
            state.up_sold = true;
            if state.first_sold_side.is_none() {
                state.first_sold_side = Some("UP".to_string());
            }
            signals.push(OrderSignal {
                side: "UP".to_string(),
                is_buy: false,
                amount: win_state.up_shares,
                price: up_bid,
                reason: format!("strategy_exit_{}", target_bid),
            });
        }
        if dn_bid >= target_bid && !state.down_sold && win_state.down_shares > 0.0 {
            state.down_sold = true;
            if state.first_sold_side.is_none() {
                state.first_sold_side = Some("DOWN".to_string());
            }
            signals.push(OrderSignal {
                side: "DOWN".to_string(),
                is_buy: false,
                amount: win_state.down_shares,
                price: dn_bid,
                reason: format!("strategy_exit_{}", target_bid),
            });
        }

        signals
    }

    fn get_strategy_state(&self, window_number: usize) -> Option<StrategyState> {
        self.states.get(&window_number).cloned()
    }
}
