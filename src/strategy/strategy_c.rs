use crate::client::{MarketWindow, PricesState};
use crate::config::Config;
use crate::strategy::{
    EntryMode, EntrySignal, OrderSignal, StrategyState, TradeStrategy, LEGACY_CHEAPER_SIDE_RATIO,
};
use crate::trader::WindowState;
use std::collections::HashMap;

// ─── СТРАТЕГИЯ Ц: Динамический Безубыток (Dynamic Break-Even Strategy) ───
pub struct DynamicBreakEvenStrategy {
    pub entered_windows: std::collections::HashSet<usize>,
    pub states: HashMap<usize, StrategyState>,
}

impl DynamicBreakEvenStrategy {
    pub fn new() -> Self {
        Self {
            entered_windows: std::collections::HashSet::new(),
            states: HashMap::new(),
        }
    }
}

impl TradeStrategy for DynamicBreakEvenStrategy {
    /**
     * Check pre-start entry conditions for a NEXT window.
     * Ratio must be EXACTLY 50/51 or 51/50 before window starts.
     */
    fn check_pre_start_entry(
        &mut self,
        config: &Config,
        prices: &PricesState,
        _market: &MarketWindow,
        _spot_price: Option<f64>,
        window_number: usize,
        secs_to_start: i64,
        _current_btc_atr: f64,
        _spot_signal: crate::strategy::SpotSignalSnapshot,
        _llm_forecast: Option<crate::strategy::LlmForecast>,
        _cex_micro: &crate::strategy::CexMicroSnapshot,
    ) -> Option<EntrySignal> {
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

        // Допускаем сбалансированное отклонение на базе динамических порогов из конфига
        let min_ask = config.pre_start_entry.min_side_ask;
        let max_ask = config.pre_start_entry.max_side_ask;
        if up_ask < min_ask || up_ask > max_ask || dn_ask < min_ask || dn_ask > max_ask {
            return None;
        }

        self.entered_windows.insert(window_number);
        Some(EntrySignal {
            up_ask,
            down_ask: dn_ask,
            budget_multiplier: 1.0,
            cheaper_side_ratio: LEGACY_CHEAPER_SIDE_RATIO,
            mode: EntryMode::Both,
            reason: "dynamic_breakeven_balanced_pre_start".to_string(),
        })
    }

    /**
     * Evaluate live tick exit rules for a CURRENT window.
     */
    fn process_live_tick(
        &mut self,
        config: &Config,
        prices: &PricesState,
        spot_price: Option<f64>,
        market: &MarketWindow,
        win_state: &WindowState,
        secs_to_end: i64,
        _current_atr: f64,
        _spot_signal: crate::strategy::SpotSignalSnapshot,
        _mid_cross: &crate::strategy::MidCrossSnapshot,
        _cex_micro: &crate::strategy::CexMicroSnapshot,
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
            e_conviction_side: None,
            e_tranches_done: 0,
            e_grid_steps_done: 0,
            h_entry_side: None,
            h_entry_done: false,
            h_salvage_done: false,
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

        // ─── ВЫХОД 1: Продажа первого дошедшего до цели контракта (например, 0.65$) ───
        if state.first_sold_side.is_none() {
            if up_bid >= target_bid && !state.up_sold && win_state.up_shares > 0.0 {
                state.up_sold = true;
                state.first_sold_side = Some("UP".to_string());
                signals.push(OrderSignal {
                    side: "UP".to_string(),
                    is_buy: false,
                    amount: win_state.up_shares,
                    price: up_bid,
                    reason: format!("strategy_first_side_exit_{}", target_bid),
                });
            } else if dn_bid >= target_bid && !state.down_sold && win_state.down_shares > 0.0 {
                state.down_sold = true;
                state.first_sold_side = Some("DOWN".to_string());
                signals.push(OrderSignal {
                    side: "DOWN".to_string(),
                    is_buy: false,
                    amount: win_state.down_shares,
                    price: dn_bid,
                    reason: format!("strategy_first_side_exit_{}", target_bid),
                });
            }
        }

        // ─── ВЫХОД 2: Ждем разворота спота за страйк и выходим строго по Динамическому Безубытку! ───
        if let Some(ref first_sold) = state.first_sold_side {
            let second_side = if first_sold == "UP" { "DOWN" } else { "UP" };
            let second_bid = if second_side == "UP" { up_bid } else { dn_bid };
            let second_shares = if second_side == "UP" {
                win_state.up_shares
            } else {
                win_state.down_shares
            };
            let second_sold = if second_side == "UP" {
                state.up_sold
            } else {
                state.down_sold
            };

            if second_shares > 0.0 && !second_sold {
                // Инициализируем бейслайн спота к страйку при первой продаже
                if state.ptb_baseline.is_none() {
                    if let (Some(spot), Some(ptb)) = (spot_price, market.price_to_beat) {
                        let rel = if spot < ptb {
                            "BELOW".to_string()
                        } else {
                            "ABOVE".to_string()
                        };
                        state.ptb_baseline = Some(rel);
                    }
                }

                // Мониторим пересечение спот-курсом уровня страйка
                if !state.ptb_crossed {
                    if let (Some(spot), Some(ptb), Some(ref baseline)) =
                        (spot_price, market.price_to_beat, &state.ptb_baseline)
                    {
                        if first_sold == "UP" {
                            if baseline == "ABOVE" && spot < ptb {
                                state.ptb_crossed = true;
                            }
                            if spot > ptb {
                                state.ptb_baseline = Some("ABOVE".to_string());
                            }
                        } else if first_sold == "DOWN" {
                            if baseline == "BELOW" && spot > ptb {
                                state.ptb_crossed = true;
                            }
                            if spot < ptb {
                                state.ptb_baseline = Some("BELOW".to_string());
                            }
                        }
                    }
                }

                // Если спот совершил пересечение страйка обратно, вычисляем Динамический Тейк!
                if state.ptb_crossed {
                    let slippage_buffer = config
                        .dynamic_breakeven
                        .as_ref()
                        .map(|db| db.slippage_buffer)
                        .unwrap_or(0.02);

                    // Формула: Min_Safe_Price = (Spent - Cash_Returned) / Remaining_Shares + Buffer
                    let min_safe_price = (win_state.spent - win_state.cash_returned)
                        / second_shares
                        + slippage_buffer;

                    // Если текущий Bid слабой стороны покрывает динамический порог безубыточности — продаем!
                    if second_bid >= min_safe_price {
                        if second_side == "UP" {
                            state.up_sold = true;
                        } else {
                            state.down_sold = true;
                        }
                        signals.push(OrderSignal {
                            side: second_side.to_string(),
                            is_buy: false,
                            amount: second_shares,
                            price: second_bid,
                            reason: format!("dynamic_breakeven_exit_bid_ge_{:.2}", min_safe_price),
                        });
                    }
                }
            }
        }

        signals
    }

    fn get_strategy_state(&self, window_number: usize) -> Option<StrategyState> {
        self.states.get(&window_number).cloned()
    }
}
