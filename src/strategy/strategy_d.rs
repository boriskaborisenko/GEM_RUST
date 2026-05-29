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

        // Допускаем сбалансированное отклонение на базе динамических порогов из конфига
        let min_ask = config.pre_start_entry.min_side_ask;
        let max_ask = config.pre_start_entry.max_side_ask;
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
        current_atr: f64,
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

        // Вычисляем абсолютное процентное отклонение спота от страйка (PTB)
        let mut pct_abs = 0.0;
        if let (Some(spot), Some(ptb)) = (spot_price, market.price_to_beat) {
            if ptb > 0.0 {
                pct_abs = ((spot - ptb).abs() / ptb) * 100.0;
            }
        }

        // Автоматически определяем силу по объемам на входе
        let is_up_strong = win_state.initial_up_shares >= win_state.initial_down_shares;
        
        // Выявляем, является ли сильная сторона более дешевой (с бóльшим весом долей 60/40)
        let initial_up = win_state.initial_up_shares;
        let initial_dn = win_state.initial_down_shares;
        let is_strong_side_cheaper = if is_up_strong { initial_up > initial_dn } else { initial_dn > initial_up };

        // ─── ТРЕХМЕРНАЯ АДАПТИВНАЯ СЕТКА СИЛЬНОЙ СТОРОНЫ (TVDS STRONG GRID) ───
        // Ступень 1 (40%):
        let step1_target = if time_pct < 30.0 && current_atr >= 30.0 {
            0.62 // Резкий старт: не спешим продавать лидера
        } else if time_pct >= 60.0 || current_atr < 15.0 {
            0.54 // Затухание или тухляк: выходим быстрее
        } else {
            0.58 // Стандарт
        };

        // Ступень 2 (40%):
        let step2_target = if time_pct < 60.0 && current_atr >= 30.0 {
            0.72 // Сильный мид-гейм импульс
        } else if time_pct >= 80.0 || current_atr < 15.0 {
            0.60 // Сброс перед финалом / при затухании
        } else {
            0.66 // Стандарт
        };

        // Ступень 3 (Раннер — 20%):
        let step3_target = if is_strong_side_cheaper {
            // Крупная дешевая сторона (60%): даем прибыли течь при мощном тренде
            if pct_abs >= 0.20 && current_atr >= 30.0 {
                0.92 // Держим до победных 92 центов
            } else if time_pct >= 80.0 || current_atr < 15.0 {
                0.68 // Сейв в боковике/конце
            } else {
                0.75 // Стандарт
            }
        } else {
            // Менее объемная дорогая сторона (40%): выходим умеренно
            if pct_abs >= 0.20 && current_atr >= 30.0 {
                0.85
            } else if time_pct >= 80.0 || current_atr < 15.0 {
                0.68
            } else {
                0.75
            }
        };

        let strong_grid = vec![step1_target, step2_target, step3_target];

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
                            reason: format!("tvds_strong_grid_exit_step_{}_{:.2}", *current_step, target),
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
                            reason: format!("tvds_strong_grid_exit_step_{}_{:.2}", *current_step, target),
                        });
                    }
                }
            }
        }

        // ─── 2. МОДУЛЬ DYNAMIC BUY (ДОКУПКА ПРИ СИЛЬНОМ ТРЕНДЕ) ───
        let buy_flag = self.buy_triggered.entry(window_number).or_insert(false);
        if time_pct <= 60.0 && !*buy_flag {
            // Определяем порог допустимого отклонения спота от страйка на основе волатильности (ATR)
            let max_allowed_deviation = if current_atr >= 30.0 {
                0.12 // Высокая волатильность: разрешаем докупку при отклонении до 0.12%
            } else if current_atr < 15.0 {
                0.03 // Тухляк: только до 0.03% (почти на страйке)
            } else {
                0.08 // Нормальный рынок: до 0.08% отклонения
            };

            // Допускаем закуп только если спот находится в пределах досягаемости для потенциального разворота
            let is_spot_within_reach = pct_abs <= max_allowed_deviation;

            if is_spot_within_reach {
                if is_up_strong && up_bid >= 0.75 && dn_ask <= 0.16 && dn_ask > 0.0 {
                    *buy_flag = true;
                    signals.push(OrderSignal {
                        side: "DOWN".to_string(),
                        is_buy: true,
                        amount: win_state.initial_down_shares * 0.50, // Докупка 50% от начального объема
                        price: dn_ask,
                        reason: format!("dynamic_buy_weak_down_deviation_ok_pct_{:.4}", pct_abs),
                    });
                } else if !is_up_strong && dn_bid >= 0.75 && up_ask <= 0.16 && up_ask > 0.0 {
                    *buy_flag = true;
                    signals.push(OrderSignal {
                        side: "UP".to_string(),
                        is_buy: true,
                        amount: win_state.initial_up_shares * 0.50, // Докупка 50% от начального объема
                        price: up_ask,
                        reason: format!("dynamic_buy_weak_up_deviation_ok_pct_{:.4}", pct_abs),
                    });
                }
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

                // ─── АДАПТИВНАЯ МОДЕЛЬ ВЫХОДА ДЛЯ СЛАБОЙ СТОРОНЫ (TVDS MATRIX) ───
                let mut should_sell = false;
                let mut reason_str = String::new();

                // 1. Определение целевого Bid на основе матрицы TVDS (Время × Волатильность × Отклонение)
                let (weak_target, zone_desc) = if time_pct < 30.0 {
                    // Ранняя фаза (Времени полно, распада нет): ждем глубокого разворота
                    if current_atr >= 30.0 {
                        (0.65, "early_high_vol_wait_reversal") // Высокая волатильность: ждем полноценный взлет
                    } else if current_atr < 15.0 {
                        (0.40, "early_sluggish_exit_fast") // Боковик: выходим при первой же возможности
                    } else {
                        (0.50, "early_normal_vol_wait")
                    }
                } else if time_pct < 60.0 {
                    // Средняя фаза (Разгар битвы)
                    if pct_abs <= 0.05 {
                        (0.65, "mid_near_strike_wait_reversal") // Сверхблизко: выжидаем полноценный разворот
                    } else if pct_abs <= 0.15 {
                        (0.30, "mid_moderate_take_30") // Умеренно: фиксируем отличные x2 от закупа по 0.15
                    } else {
                        // Спот ушел далеко
                        if current_atr >= 30.0 {
                            (0.22, "mid_far_high_vol_wait") // Волатильно: ждем небольшого отскока
                        } else if current_atr < 15.0 {
                            (0.17, "mid_far_sluggish_dump_17") // Затухание: сливаем по 0.17 для сохранения кэша
                        } else {
                            (0.20, "mid_far_normal_vol_wait")
                        }
                    }
                } else if time_pct < 80.0 {
                    // Поздняя фаза (Пошел сильный временной распад)
                    if pct_abs <= 0.05 {
                        (0.45, "late_near_strike_wait_reversal")
                    } else if pct_abs <= 0.20 {
                        (0.20, "late_moderate_take_20") // Быстро сбрасываем в легкий плюс
                    } else {
                        (0.12, "late_far_dump_12") // Спасаем крохи
                    }
                } else if time_pct < 90.0 {
                    // Финальная фаза (Последние минуты, распад критический)
                    if pct_abs <= 0.03 {
                        (0.30, "end_near_strike_reversal_hope")
                    } else {
                        (0.10, "end_far_emergency_dump_10") // Шансов почти нет: забираем 10 центов вместо 0
                    }
                } else {
                    // Фаза экспирации (Последний шанс перед сгоранием в ноль)
                    (0.08, "expiration_unconditional_dump_08")
                };

                if second_bid >= weak_target {
                    should_sell = true;
                    reason_str = format!("tvds_weak_exit_{}_bid_ge_{:.2}", zone_desc, weak_target);
                }

                // 2. Дополнительная страховка: Окупаемость раунда по формуле безубытка в поздней фазе (60% - 80%)
                if !should_sell && time_pct >= 60.0 && time_pct < 80.0 {
                    let min_safe_price = (win_state.spent - win_state.cash_returned) / second_shares;
                    if min_safe_price > 0.0 && min_safe_price < 0.65 && second_bid >= min_safe_price {
                        should_sell = true;
                        reason_str = format!("tvds_late_exact_breakeven_bid_ge_{:.2}", min_safe_price);
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
