use crate::client::{MarketWindow, PricesState};
use crate::config::Config;
use crate::strategy::{
    EntryMode, EntrySignal, OrderSignal, StrategyState, TradeStrategy, LEGACY_CHEAPER_SIDE_RATIO,
};
use crate::trader::WindowState;
use std::collections::HashMap;

// ─── СТРАТЕГИЯ Б: Асимметричная лесенка (Asymmetric Ladder Strategy) ───
pub struct AsymmetricLadderStrategy {
    pub entered_windows: std::collections::HashSet<usize>,
    pub states: HashMap<usize, StrategyState>,
    pub up_steps_hit: HashMap<usize, usize>, // Отслеживаем шаги лесенки UP на окно
    pub dn_steps_hit: HashMap<usize, usize>, // Отслеживаем шаги лесенки DOWN на окно
}

impl AsymmetricLadderStrategy {
    pub fn new() -> Self {
        Self {
            entered_windows: std::collections::HashSet::new(),
            states: HashMap::new(),
            up_steps_hit: HashMap::new(),
            dn_steps_hit: HashMap::new(),
        }
    }
}

impl TradeStrategy for AsymmetricLadderStrategy {
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

        // Допускаем сбаланнорованное отклонение на базе динамических порогов из конфига
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
            reason: "asymmetric_ladder_balanced_pre_start".to_string(),
        })
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
        _current_atr: f64,
        _spot_signal: crate::strategy::SpotSignalSnapshot,
        _mid_cross: &crate::strategy::MidCrossSnapshot,
        _cex_micro: &crate::strategy::CexMicroSnapshot,
        _tape: &crate::trade_tape::TradeTapeSnapshot,
    ) -> Vec<OrderSignal> {
        let mut signals = vec![];
        let window_number = win_state.window_number;

        let up_bid = prices.up.bid;
        let dn_bid = prices.down.bid;

        // Пороги лесенки из конфига или наши дефолтные Sweet Spot
        let default_strong = vec![0.62, 0.72];
        let default_weak = vec![0.70, 0.85];

        let (mut strong_steps, mut weak_steps) = match &config.asymmetric_ladder {
            Some(ladder) => (ladder.strong_steps.clone(), ladder.weak_steps.clone()),
            None => (default_strong, default_weak),
        };

        // Рассчитываем временной распад порогов в процентах (Theta decay)
        let decay_enabled = config
            .asymmetric_ladder
            .as_ref()
            .map(|l| l.decay_enabled)
            .unwrap_or(false);
        if decay_enabled {
            let duration_ms = match (
                chrono::DateTime::parse_from_rfc3339(&market.start_time),
                chrono::DateTime::parse_from_rfc3339(&market.end_time),
            ) {
                (Ok(s), Ok(e)) => (e.timestamp_millis() - s.timestamp_millis()) as f64,
                _ => 900_000.0, // 15m fallback
            };
            let duration_sec = duration_ms / 1000.0;
            let elapsed_sec = (duration_sec - secs_to_end as f64).clamp(0.0, duration_sec);
            let progress_pct = elapsed_sec / duration_sec; // от 0% до 100%

            let decay_mult = if progress_pct < 0.50 {
                1.0
            } else if progress_pct >= 0.50 && progress_pct < 0.80 {
                0.90
            } else {
                0.80
            };

            for step in &mut strong_steps {
                *step *= decay_mult;
            }
            for step in &mut weak_steps {
                *step *= decay_mult;
            }
        }

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
                    order_type: crate::strategy::OrderType::Market,
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
                    order_type: crate::strategy::OrderType::Market,
                    amount: win_state.down_shares,
                    price: dn_bid,
                    reason: "emergency_15pct_time_stop_bid_ge_0.20".to_string(),
                });
            }
        }

        // Автоматически определяем силу по объемам на входе
        let is_up_strong = win_state.initial_up_shares >= win_state.initial_down_shares;

        // ─── СТУПЕНЧАТАЯ LADDER-C СИСТЕМА ПРОДАЖ С ВРЕМЕННЫМ РАСПАДОМ ───
        // А. Выход для стороны UP
        if !state.up_sold && win_state.up_shares > 0.0 {
            let steps = if is_up_strong {
                &strong_steps
            } else {
                &weak_steps
            };
            let current_step = self.up_steps_hit.entry(window_number).or_insert(0);

            if *current_step < steps.len() {
                let target = steps[*current_step];
                if up_bid >= target {
                    let sell_amount = if *current_step == 0 && steps.len() > 1 {
                        win_state.initial_up_shares * 0.50 // Продаем 50% объема первой ступенью
                    } else {
                        win_state.up_shares // Остатки на последней ступени
                    };

                    *current_step += 1;
                    if *current_step >= steps.len() {
                        state.up_sold = true;
                    }
                    if state.first_sold_side.is_none() {
                        state.first_sold_side = Some("UP".to_string());
                    }

                    signals.push(OrderSignal {
                        side: "UP".to_string(),
                        is_buy: false,
                        order_type: crate::strategy::OrderType::Market,
                        amount: sell_amount,
                        price: up_bid,
                        reason: format!("ladder_exit_step_{}_{:.2}", *current_step, target),
                    });
                }
            }
        }

        // Б. Выход для стороны DOWN
        if !state.down_sold && win_state.down_shares > 0.0 {
            let steps = if !is_up_strong {
                &strong_steps
            } else {
                &weak_steps
            };
            let current_step = self.dn_steps_hit.entry(window_number).or_insert(0);

            if *current_step < steps.len() {
                let target = steps[*current_step];
                if dn_bid >= target {
                    let sell_amount = if *current_step == 0 && steps.len() > 1 {
                        win_state.initial_down_shares * 0.50
                    } else {
                        win_state.down_shares
                    };

                    *current_step += 1;
                    if *current_step >= steps.len() {
                        state.down_sold = true;
                    }
                    if state.first_sold_side.is_none() {
                        state.first_sold_side = Some("DOWN".to_string());
                    }

                    signals.push(OrderSignal {
                        side: "DOWN".to_string(),
                        is_buy: false,
                        order_type: crate::strategy::OrderType::Market,
                        amount: sell_amount,
                        price: dn_bid,
                        reason: format!("ladder_exit_step_{}_{:.2}", *current_step, target),
                    });
                }
            }
        }

        signals
    }

    fn get_strategy_state(&self, window_number: usize) -> Option<StrategyState> {
        self.states.get(&window_number).cloned()
    }
}
