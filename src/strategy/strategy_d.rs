use crate::client::{MarketWindow, PricesState};
use crate::trader::WindowState;
use crate::config::Config;
use crate::strategy::{OrderSignal, StrategyState, TradeStrategy};
use std::collections::HashMap;

// ─── СТРАТЕГИЯ Д: Dynamic Grid + Dynamic BUY + Time-Decay Crossover Block ───
pub struct DynamicGridStrategy {
    pub entered_windows: std::collections::HashSet<usize>,
    pub states: HashMap<usize, StrategyState>,
    pub up_steps_hit: HashMap<usize, usize>, // Ступени сетки UP (0..3)
    pub dn_steps_hit: HashMap<usize, usize>, // Ступени сетки DOWN (0..3)
    pub buy_triggered: HashMap<usize, bool>, // Флаг срабатывания Dynamic BUY на окно
}

impl DynamicGridStrategy {
    pub fn new() -> Self {
        Self {
            entered_windows: std::collections::HashSet::new(),
            states: HashMap::new(),
            up_steps_hit: HashMap::new(),
            dn_steps_hit: HashMap::new(),
            buy_triggered: HashMap::new(),
        }
    }
}

impl TradeStrategy for DynamicGridStrategy {
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
        current_btc_atr: f64,
    ) -> Option<(f64, f64)> {
        if !config.pre_start_entry.enabled {
            return None;
        }
        if self.entered_windows.contains(&window_number) {
            return None;
        }

        // Регулируем закуп строго в заданном временном диапазоне [120 сек - 5 сек] перед стартом
        if secs_to_start < config.pre_start_entry.min_seconds_before_start
            || secs_to_start > config.pre_start_entry.max_seconds_before_start
        {
            return None;
        }

        // ─── ФИЛЬТР ВОЛАТИЛЬНОСТИ СНЕСЕТ МУСОРНЫЕ ОКНА ───
        if current_btc_atr < config.min_btc_atr {
            // Рынок слишком дохлый, пропускаем раунд во избежание Time Decay убытков
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
     * Evaluate live tick exit/buy rules for a CURRENT window.
     */
    fn process_live_tick(
        &mut self,
        config: &Config,
        prices: &PricesState,
        spot_price: Option<f64>,
        market: &MarketWindow,
        win_state: &WindowState,
        secs_to_end: i64,
    ) -> Vec<OrderSignal> {
        let mut signals = vec![];
        let window_number = win_state.window_number;

        let up_bid = prices.up.bid;
        let dn_bid = prices.down.bid;
        let up_ask = prices.up.ask;
        let dn_ask = prices.down.ask;

        // Рассчитываем временной фильтр в процентах (0.0% - 100.0%)
        let duration_ms = match (
            chrono::DateTime::parse_from_rfc3339(&market.start_time),
            chrono::DateTime::parse_from_rfc3339(&market.end_time),
        ) {
            (Ok(s), Ok(e)) => (e.timestamp_millis() - s.timestamp_millis()) as f64,
            _ => 900_000.0, // 15m fallback
        };
        let duration_sec = duration_ms / 1000.0;
        let elapsed_sec = (duration_sec - secs_to_end as f64).clamp(0.0, duration_sec);
        let time_pct = (elapsed_sec / duration_sec) * 100.0;

        let state = self.states.entry(window_number).or_insert(StrategyState {
            up_sold: false,
            down_sold: false,
            first_sold_side: None,
            ptb_crossed: false,
            ptb_baseline: None,
        });

        // ─── EMERGENCY RULE: 15% remaining time stop (Unconditional!) ───────
        let emergency_time_threshold = (duration_sec * 0.15) as i64; // 135s for 15m

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

        // Автоматически определяем силу по объемам на входе
        let is_up_strong = win_state.initial_up_shares >= win_state.initial_down_shares;
        let strong_grid = vec![0.58, 0.66, 0.75];

        // ─── 1. МОНИТОРИНГ СЕТКИ ПРОДАЖ ДЛЯ СИЛЬНОЙ СТОРОНЫ ───
        if is_up_strong {
            // UP - Сильная нога
            if !state.up_sold && win_state.up_shares > 0.0 {
                let current_step = self.up_steps_hit.entry(window_number).or_insert(0);
                if *current_step < strong_grid.len() {
                    let target = strong_grid[*current_step];
                    if up_bid >= target {
                        let sell_amount = match *current_step {
                            0 => win_state.initial_up_shares * 0.40,
                            1 => win_state.initial_up_shares * 0.40,
                            _ => win_state.up_shares, // Сливаем остатки на 3-й ступени
                        };

                        *current_step += 1;
                        if *current_step >= strong_grid.len() {
                            state.up_sold = true;
                        }
                        if state.first_sold_side.is_none() {
                            state.first_sold_side = Some("UP".to_string());
                        }

                        signals.push(OrderSignal {
                            side: "UP".to_string(),
                            is_buy: false,
                            amount: sell_amount,
                            price: up_bid,
                            reason: format!("dynamic_grid_exit_step_{}_{:.2}", *current_step, target),
                        });
                    }
                }
            }
        } else {
            // DOWN - Сильная нога
            if !state.down_sold && win_state.down_shares > 0.0 {
                let current_step = self.dn_steps_hit.entry(window_number).or_insert(0);
                if *current_step < strong_grid.len() {
                    let target = strong_grid[*current_step];
                    if dn_bid >= target {
                        let sell_amount = match *current_step {
                            0 => win_state.initial_down_shares * 0.40,
                            1 => win_state.initial_down_shares * 0.40,
                            _ => win_state.down_shares, // Сливаем остатки на 3-й ступени
                        };

                        *current_step += 1;
                        if *current_step >= strong_grid.len() {
                            state.down_sold = true;
                        }
                        if state.first_sold_side.is_none() {
                            state.first_sold_side = Some("DOWN".to_string());
                        }

                        signals.push(OrderSignal {
                            side: "DOWN".to_string(),
                            is_buy: false,
                            amount: sell_amount,
                            price: dn_bid,
                            reason: format!("dynamic_grid_exit_step_{}_{:.2}", *current_step, target),
                        });
                    }
                }
            }
        }

        // ─── 2. МОДУЛЬ DYNAMIC BUY (ДОКУПКА ПРИ СИЛЬНОМ ТРЕНДЕ) ───
        let buy_flag = self.buy_triggered.entry(window_number).or_insert(false);
        if time_pct <= 60.0 && !*buy_flag {
            if is_up_strong && up_bid >= 0.75 && dn_ask <= 0.16 && dn_ask > 0.0 {
                *buy_flag = true;
                signals.push(OrderSignal {
                    side: "DOWN".to_string(),
                    is_buy: true,
                    amount: win_state.initial_down_shares * 0.50, // Докупка 50% от начального объема
                    price: dn_ask,
                    reason: "dynamic_buy_weak_down_at_trend_peak".to_string(),
                });
            } else if !is_up_strong && dn_bid >= 0.75 && up_ask <= 0.16 && up_ask > 0.0 {
                *buy_flag = true;
                signals.push(OrderSignal {
                    side: "UP".to_string(),
                    is_buy: true,
                    amount: win_state.initial_up_shares * 0.50, // Докупка 50% от начального объема
                    price: up_ask,
                    reason: "dynamic_buy_weak_up_at_trend_peak".to_string(),
                });
            }
        }

        // ─── 3. ТРИГГЕР ПЕРЕСЕЧЕНИЯ ЛИНИИ СТАРТА (CROSSOVER БЛОК) ───
        if let Some(ref first_sold) = state.first_sold_side {
            let second_side = if first_sold == "UP" { "DOWN" } else { "UP" };
            let second_bid = if second_side == "UP" { up_bid } else { dn_bid };
            let second_shares = if second_side == "UP" { win_state.up_shares } else { win_state.down_shares };
            let second_sold = if second_side == "UP" { state.up_sold } else { state.down_sold };

            if second_shares > 0.0 && !second_sold {
                // Инициализируем бейслайн спота к страйку при первой продаже
                if state.ptb_baseline.is_none() {
                    if let (Some(spot), Some(ptb)) = (spot_price, market.price_to_beat) {
                        let rel = if spot < ptb { "BELOW".to_string() } else { "ABOVE".to_string() };
                        state.ptb_baseline = Some(rel);
                    }
                }

                // Мониторим пересечение спот-курсом уровня страйка обратно (crossover)
                if !state.ptb_crossed {
                    if let (Some(spot), Some(ptb), Some(ref baseline)) = (spot_price, market.price_to_beat, &state.ptb_baseline) {
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

                // ─── А. УЛЬТИМАТИВНЫЙ ТЕЙК-ПРОФИТ СЛАБОЙ СТОРОНЫ ───
                // Если слабая сторона сама взлетела до целевого тейка из конфига (например, >= 0.65$),
                // мы забираем этот жирный профит моментально и без всяких условий!
                let mut should_sell = false;
                let mut reason_str = String::new();

                if second_bid >= config.sell_strategy.exit_bid {
                    should_sell = true;
                    reason_str = format!("unconditional_profit_take_ge_{:.2}", config.sell_strategy.exit_bid);
                }

                // ─── Б. ОППОРТУНИСТИЧЕСКИЙ ВЫХОД ДЛЯ DYNAMIC BUY (СЛИВ ПО 30 КОПЕЕК!) ───
                // Если мы совершили усреднение (Dynamic BUY), у нас огромный объем по низкой цене.
                // При росте контракта >= 0.30$, мы сливаем DOWN для надежной фиксации leveraged прибыли!
                let has_dynamic_buy = *self.buy_triggered.entry(window_number).or_insert(false);
                if !should_sell && has_dynamic_buy && second_bid >= 0.30 {
                    should_sell = true;
                    reason_str = "dynamic_buy_opportunistic_exit_bid_ge_0.30".to_string();
                }

                // ─── В. ВЫХОД ПО ДИСТАНЦИИ СПОТА К СТРАЙКУ (DISTANCE ABS & PCT) ───
                // Если спот-курс прижался вплотную к страйку (расстояние <= 40$ или <= 0.05% от цены),
                // и слабая сторона уже стоит прилично (например, >= 0.30$), мы выходим, не дожидаясь физического пробития!
                if !should_sell {
                    if let (Some(spot), Some(ptb)) = (spot_price, market.price_to_beat) {
                        let distance_abs = (spot - ptb).abs();
                        let distance_pct = (distance_abs / spot) * 100.0;
                        
                        let is_near_strike = distance_abs <= 40.0 || distance_pct <= 0.05;
                        if is_near_strike && second_bid >= 0.30 {
                            should_sell = true;
                            reason_str = format!("spot_near_strike_exit_dist_{:.1}_bid_{:.2}", distance_abs, second_bid);
                        }
                    }
                }

                // ─── Г. ЛОГИКА НА РЕАЛЬНОМ КРОССОВЕРЕ (ЕСЛИ ДРУГИЕ УСЛОВИЯ ЕЩЕ НЕ СРАБОТАЛИ) ───
                if !should_sell && state.ptb_crossed {
                    // Математическая нелинейная модель Time-Decay распада
                    let time_factor = (time_pct / 100.0).powf(1.5);
                    let target_decay_bid = 0.50 * (1.0 - time_factor);

                    if time_pct < 50.0 && second_bid >= target_decay_bid {
                        should_sell = true;
                        reason_str = format!("time_decay_crossover_sell_pct_{:.1}_bid_{:.2}", time_pct, second_bid);
                    } else if time_pct >= 50.0 && time_pct <= 75.0 {
                        // Точный расчет безубыточности раунда по фактическому кэшу
                        let min_safe_price = (win_state.spent - win_state.cash_returned) / second_shares;
                        if second_bid >= min_safe_price {
                            should_sell = true;
                            reason_str = format!("exact_breakeven_exit_bid_ge_{:.2}", min_safe_price);
                        }
                    }
                }

                if should_sell {
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
                        reason: reason_str,
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
