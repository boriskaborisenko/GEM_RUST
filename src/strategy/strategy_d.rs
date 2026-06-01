use crate::client::{MarketWindow, PricesState};
use crate::config::Config;
use crate::strategy::{EntrySignal, OrderSignal, SpotSignalSnapshot, StrategyState, TradeStrategy};
use crate::trader::WindowState;
use std::collections::HashMap;

const ATR_ULTRA_LOW_MAX: f64 = 8.0;
const ATR_LOW_MAX: f64 = 18.0;
const ATR_HIGH_MIN: f64 = 45.0;
const ATR_EXTREME_MIN: f64 = 90.0;

const ENTRY_MAX_ASK_SPREAD: f64 = 0.03;
const ENTRY_ULTRA_LOW_ATR_MAX_COMBINED_ASK: f64 = 1.00;
const ENTRY_LOW_ATR_MAX_COMBINED_ASK: f64 = 1.01;
const ENTRY_HIGH_ATR_MAX_COMBINED_ASK: f64 = 1.04;
const ENTRY_EXTREME_ATR_MAX_COMBINED_ASK: f64 = 1.02;

const DYNAMIC_BUY_WEAK_SHARE_FRACTION: f64 = 0.35;
const DYNAMIC_BUY_MAX_USD_FRACTION_OF_SPENT: f64 = 0.12;
const DYNAMIC_BUY_COUNTER_SOFT_FACTOR: f64 = 0.35;
const REDEEM_HOLD_RELEASE_BID: f64 = 0.90;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeviationZone {
    Unknown,
    Near,
    Moderate,
    Far,
    Runaway,
}

impl DeviationZone {
    fn as_str(self) -> &'static str {
        match self {
            DeviationZone::Unknown => "unknown",
            DeviationZone::Near => "near",
            DeviationZone::Moderate => "moderate",
            DeviationZone::Far => "far",
            DeviationZone::Runaway => "runaway",
        }
    }

    fn is_far_or_runaway(self) -> bool {
        matches!(self, DeviationZone::Far | DeviationZone::Runaway)
    }
}

#[derive(Debug, Clone, Copy)]
struct PtbDeviation {
    known: bool,
    signed_usd: f64,
    abs_usd: f64,
    pct_abs: f64,
    zone: DeviationZone,
}

#[derive(Debug, Clone, Copy)]
struct EntryRegime {
    budget_multiplier: f64,
    cheaper_side_ratio: f64,
    max_combined_ask: f64,
    reason: &'static str,
}

fn entry_regime(current_btc_atr: f64) -> EntryRegime {
    if current_btc_atr < ATR_ULTRA_LOW_MAX {
        EntryRegime {
            budget_multiplier: 0.25,
            cheaper_side_ratio: 0.50,
            max_combined_ask: ENTRY_ULTRA_LOW_ATR_MAX_COMBINED_ASK,
            reason: "d_v3_entry_ultra_low_atr_micro",
        }
    } else if current_btc_atr < ATR_LOW_MAX {
        EntryRegime {
            budget_multiplier: 0.55,
            cheaper_side_ratio: 0.52,
            max_combined_ask: ENTRY_LOW_ATR_MAX_COMBINED_ASK,
            reason: "d_v3_entry_low_atr_scout",
        }
    } else if current_btc_atr < ATR_HIGH_MIN {
        EntryRegime {
            budget_multiplier: 1.00,
            cheaper_side_ratio: 0.56,
            max_combined_ask: ENTRY_HIGH_ATR_MAX_COMBINED_ASK,
            reason: "d_v3_entry_normal_atr_balanced",
        }
    } else if current_btc_atr < ATR_EXTREME_MIN {
        EntryRegime {
            budget_multiplier: 0.85,
            cheaper_side_ratio: 0.50,
            max_combined_ask: ENTRY_HIGH_ATR_MAX_COMBINED_ASK,
            reason: "d_v3_entry_high_atr_delta_neutral",
        }
    } else {
        EntryRegime {
            budget_multiplier: 0.45,
            cheaper_side_ratio: 0.50,
            max_combined_ask: ENTRY_EXTREME_ATR_MAX_COMBINED_ASK,
            reason: "d_v3_entry_extreme_atr_micro_neutral",
        }
    }
}

fn live_leader_is_up(up_bid: f64, dn_bid: f64, initial_up: f64, initial_dn: f64) -> bool {
    if up_bid >= dn_bid + 0.02 {
        true
    } else if dn_bid >= up_bid + 0.02 {
        false
    } else {
        initial_up >= initial_dn
    }
}

fn ptb_deviation(market: &MarketWindow, spot_price: Option<f64>) -> PtbDeviation {
    let Some(spot) = spot_price else {
        return PtbDeviation {
            known: false,
            signed_usd: 0.0,
            abs_usd: 0.0,
            pct_abs: 0.25,
            zone: DeviationZone::Unknown,
        };
    };
    let Some(ptb) = market.price_to_beat else {
        return PtbDeviation {
            known: false,
            signed_usd: 0.0,
            abs_usd: 0.0,
            pct_abs: 0.25,
            zone: DeviationZone::Unknown,
        };
    };
    if ptb <= 0.0 {
        return PtbDeviation {
            known: false,
            signed_usd: 0.0,
            abs_usd: 0.0,
            pct_abs: 0.25,
            zone: DeviationZone::Unknown,
        };
    }

    let signed_usd = spot - ptb;
    let abs_usd = signed_usd.abs();
    let pct_abs = (abs_usd / ptb) * 100.0;

    let asset = market.asset.to_uppercase();
    let (near_usd, moderate_usd, far_usd) = match asset.as_str() {
        "BTC" => (25.0, 100.0, 250.0),
        "ETH" => (2.0, 8.0, 20.0),
        "SOL" => (0.20, 0.80, 2.00),
        _ => (25.0, 100.0, 250.0),
    };

    let zone = if abs_usd <= near_usd || pct_abs <= 0.03 {
        DeviationZone::Near
    } else if abs_usd <= moderate_usd || pct_abs <= 0.10 {
        DeviationZone::Moderate
    } else if abs_usd <= far_usd || pct_abs <= 0.25 {
        DeviationZone::Far
    } else {
        DeviationZone::Runaway
    };

    PtbDeviation {
        known: true,
        signed_usd,
        abs_usd,
        pct_abs,
        zone,
    }
}

fn strong_side_is_itm(is_up_strong: bool, dev: PtbDeviation) -> bool {
    dev.known && ((is_up_strong && dev.signed_usd > 0.0) || (!is_up_strong && dev.signed_usd < 0.0))
}

fn spot_velocity(spot_signal: SpotSignalSnapshot) -> Option<f64> {
    spot_signal
        .smoothed_velocity_usd_per_sec
        .or(spot_signal.raw_velocity_usd_per_sec)
}

fn dynamic_buy_counter_velocity_limit(current_atr: f64) -> f64 {
    (current_atr / 15.0).clamp(0.75, 6.0)
}

fn redeem_hold_counter_velocity_limit(current_atr: f64) -> f64 {
    (current_atr / 12.0).clamp(1.0, 8.0)
}

fn counter_velocity_blocks_side(
    side_to_buy: &str,
    spot_signal: SpotSignalSnapshot,
    current_atr: f64,
) -> bool {
    let Some(velocity) = spot_velocity(spot_signal) else {
        return false;
    };
    let limit = dynamic_buy_counter_velocity_limit(current_atr) * DYNAMIC_BUY_COUNTER_SOFT_FACTOR;
    match side_to_buy {
        "UP" => velocity < -limit,
        "DOWN" => velocity > limit,
        _ => false,
    }
}

fn velocity_bias_for_side(
    is_up_side: bool,
    spot_signal: SpotSignalSnapshot,
    current_atr: f64,
) -> i8 {
    let Some(velocity) = spot_velocity(spot_signal) else {
        return 0;
    };
    let limit = (current_atr / 20.0).clamp(0.50, 4.0);
    if is_up_side {
        if velocity > limit {
            1
        } else if velocity < -limit {
            -1
        } else {
            0
        }
    } else if velocity < -limit {
        1
    } else if velocity > limit {
        -1
    } else {
        0
    }
}

fn velocity_bias_label(bias: i8) -> &'static str {
    match bias {
        1 => "with",
        -1 => "against",
        _ => "neutral",
    }
}

fn adjust_strong_grid_target(base: f64, step_idx: usize, velocity_bias: i8, time_pct: f64) -> f64 {
    let adjustment = match (velocity_bias, step_idx) {
        (1, 0) if time_pct < 70.0 => 0.01,
        (1, 1) if time_pct < 80.0 => 0.03,
        (1, 2) if time_pct < 90.0 => 0.04,
        (-1, 0) => -0.02,
        (-1, 1) => -0.03,
        (-1, 2) => -0.05,
        _ => 0.0,
    };
    (base + adjustment).clamp(0.08, 0.94)
}

fn adjust_weak_exit_target(base: f64, velocity_bias: i8, time_pct: f64) -> f64 {
    let adjustment = match velocity_bias {
        1 if time_pct < 60.0 => 0.04,
        1 if time_pct < 80.0 => 0.02,
        -1 if time_pct < 60.0 => -0.03,
        -1 => -0.04,
        _ => 0.0,
    };
    (base + adjustment).clamp(0.06, 0.70)
}

fn velocity_blocks_redeem_hold(
    is_up_strong: bool,
    spot_signal: SpotSignalSnapshot,
    current_atr: f64,
) -> bool {
    let Some(velocity) = spot_velocity(spot_signal) else {
        return false;
    };
    let limit = redeem_hold_counter_velocity_limit(current_atr);
    (is_up_strong && velocity < -limit) || (!is_up_strong && velocity > limit)
}

fn should_hold_winner_for_redeem(
    is_up_strong: bool,
    dev: PtbDeviation,
    time_pct: f64,
    secs_to_end: i64,
    strong_bid: f64,
    current_atr: f64,
    spot_signal: SpotSignalSnapshot,
) -> bool {
    if strong_bid < 0.66 || !strong_side_is_itm(is_up_strong, dev) {
        return false;
    }
    if velocity_blocks_redeem_hold(is_up_strong, spot_signal, current_atr) {
        return false;
    }

    let enough_conviction = dev.zone == DeviationZone::Runaway
        || (dev.zone == DeviationZone::Far && current_atr >= 18.0)
        || (dev.zone == DeviationZone::Moderate && time_pct >= 70.0);
    let close_enough_to_redeem = time_pct >= 45.0 || secs_to_end <= 240;

    enough_conviction && close_enough_to_redeem
}

impl TradeStrategy for DynamicGridStrategy {
    /**
     * Check pre-start entry conditions for a NEXT window.
     */
    fn check_pre_start_entry(
        &mut self,
        config: &Config,
        prices: &PricesState,
        window_number: usize,
        secs_to_start: i64,
        current_btc_atr: f64,
    ) -> Option<EntrySignal> {
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

        if current_btc_atr < config.min_btc_atr {
            return None;
        }

        let regime = entry_regime(current_btc_atr);

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

        let ask_spread = (up_ask - dn_ask).abs();
        let combined_ask = up_ask + dn_ask;
        if ask_spread > ENTRY_MAX_ASK_SPREAD || combined_ask > regime.max_combined_ask {
            return None;
        }

        self.entered_windows.insert(window_number);
        Some(EntrySignal {
            up_ask,
            down_ask: dn_ask,
            budget_multiplier: regime.budget_multiplier,
            cheaper_side_ratio: regime.cheaper_side_ratio,
            reason: format!(
                "{}_atr_{:.2}_combined_{:.2}_spread_{:.2}",
                regime.reason, current_btc_atr, combined_ask, ask_spread
            ),
        })
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
        spot_signal: SpotSignalSnapshot,
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

        // PTB deviation uses both absolute dollars and percent.
        let dev = ptb_deviation(market, spot_price);

        // Сильная сторона определяется текущим лидерством по bid, а не только стартовым перекосом.
        let is_up_strong = live_leader_is_up(
            up_bid,
            dn_bid,
            win_state.initial_up_shares,
            win_state.initial_down_shares,
        );

        let initial_up = win_state.initial_up_shares;
        let initial_dn = win_state.initial_down_shares;
        let is_strong_side_cheaper = if is_up_strong {
            initial_up > initial_dn
        } else {
            initial_dn > initial_up
        };

        let strong_bid = if is_up_strong { up_bid } else { dn_bid };
        let redeem_hold_active = should_hold_winner_for_redeem(
            is_up_strong,
            dev,
            time_pct,
            secs_to_end,
            strong_bid,
            current_atr,
            spot_signal,
        );

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
            if win_state.up_shares > 0.0
                && up_bid >= 0.20
                && !state.up_sold
                && !(redeem_hold_active && is_up_strong)
            {
                state.up_sold = true;
                signals.push(OrderSignal {
                    side: "UP".to_string(),
                    is_buy: false,
                    amount: win_state.up_shares,
                    price: up_bid,
                    reason: "emergency_15pct_time_stop_bid_ge_0.20".to_string(),
                });
            }
            if win_state.down_shares > 0.0
                && dn_bid >= 0.20
                && !state.down_sold
                && !(redeem_hold_active && !is_up_strong)
            {
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

        // ─── ТРЕХМЕРНАЯ АДАПТИВНАЯ СЕТКА СИЛЬНОЙ СТОРОНЫ (TVDS STRONG GRID) ───
        // Ступень 1: частичный de-risk. Не продаем слишком много, чтобы оставить upside до redeem.
        let step1_target = if time_pct < 30.0 && current_atr >= 30.0 {
            0.62 // Резкий старт: не спешим продавать лидера
        } else if time_pct >= 60.0 || current_atr < 15.0 {
            0.54 // Затухание или тухляк: выходим быстрее
        } else {
            0.58 // Стандарт
        };

        // Ступень 2: обычный take-profit, но redeem-hold может ее заблокировать.
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
            if dev.zone.is_far_or_runaway() && current_atr >= 30.0 {
                0.92 // Держим до победных 92 центов
            } else if time_pct >= 80.0 || current_atr < 15.0 {
                0.68 // Сейв в боковике/конце
            } else {
                0.75 // Стандарт
            }
        } else {
            // Менее объемная дорогая сторона (40%): выходим умеренно
            if dev.zone.is_far_or_runaway() && current_atr >= 30.0 {
                0.85
            } else if time_pct >= 80.0 || current_atr < 15.0 {
                0.68
            } else {
                0.75
            }
        };

        let strong_velocity_bias = velocity_bias_for_side(is_up_strong, spot_signal, current_atr);
        let strong_velocity_label = velocity_bias_label(strong_velocity_bias);
        let strong_grid = vec![
            adjust_strong_grid_target(step1_target, 0, strong_velocity_bias, time_pct),
            adjust_strong_grid_target(step2_target, 1, strong_velocity_bias, time_pct),
            adjust_strong_grid_target(step3_target, 2, strong_velocity_bias, time_pct),
        ];

        // ─── 1. МОНИТОРИНГ СЕТКИ ПРОДАЖ ДЛЯ СИЛЬНОЙ СТОРОНЫ ───
        if is_up_strong {
            // UP - Сильная нога
            if !state.up_sold && win_state.up_shares > 0.0 {
                let current_step = self.up_steps_hit.entry(window_number).or_insert(0);
                if *current_step < strong_grid.len() {
                    let target = strong_grid[*current_step];
                    if up_bid >= target {
                        let hold_blocks_mid_exit = redeem_hold_active
                            && *current_step >= 1
                            && up_bid < REDEEM_HOLD_RELEASE_BID;
                        if !hold_blocks_mid_exit {
                            let sell_amount = if redeem_hold_active && *current_step >= 1 {
                                (win_state.initial_up_shares * 0.20).min(win_state.up_shares)
                            } else {
                                match *current_step {
                                    0 => (win_state.initial_up_shares * 0.30)
                                        .min(win_state.up_shares),
                                    1 => (win_state.initial_up_shares * 0.30)
                                        .min(win_state.up_shares),
                                    _ => win_state.up_shares,
                                }
                            };

                            if redeem_hold_active && *current_step >= 1 {
                                *current_step = strong_grid.len();
                            } else {
                                *current_step += 1;
                            }
                            if !redeem_hold_active && *current_step >= strong_grid.len() {
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
                                reason: format!(
                                    "tvds_strong_grid_exit_step_{}_{:.2}_zone_{}_usd_{:.2}_pct_{:.4}{}_vel_{}",
                                    *current_step,
                                    target,
                                    dev.zone.as_str(),
                                    dev.abs_usd,
                                    dev.pct_abs,
                                    if redeem_hold_active { "_redeem_hold_runner" } else { "" },
                                    strong_velocity_label
                                ),
                            });
                        }
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
                        let hold_blocks_mid_exit = redeem_hold_active
                            && *current_step >= 1
                            && dn_bid < REDEEM_HOLD_RELEASE_BID;
                        if !hold_blocks_mid_exit {
                            let sell_amount = if redeem_hold_active && *current_step >= 1 {
                                (win_state.initial_down_shares * 0.20).min(win_state.down_shares)
                            } else {
                                match *current_step {
                                    0 => (win_state.initial_down_shares * 0.30)
                                        .min(win_state.down_shares),
                                    1 => (win_state.initial_down_shares * 0.30)
                                        .min(win_state.down_shares),
                                    _ => win_state.down_shares,
                                }
                            };

                            if redeem_hold_active && *current_step >= 1 {
                                *current_step = strong_grid.len();
                            } else {
                                *current_step += 1;
                            }
                            if !redeem_hold_active && *current_step >= strong_grid.len() {
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
                                reason: format!(
                                    "tvds_strong_grid_exit_step_{}_{:.2}_zone_{}_usd_{:.2}_pct_{:.4}{}_vel_{}",
                                    *current_step,
                                    target,
                                    dev.zone.as_str(),
                                    dev.abs_usd,
                                    dev.pct_abs,
                                    if redeem_hold_active { "_redeem_hold_runner" } else { "" },
                                    strong_velocity_label
                                ),
                            });
                        }
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
            let is_spot_within_reach = dev.known && dev.pct_abs <= max_allowed_deviation;

            if is_spot_within_reach {
                // ─── ПОЛНОСТЬЮ АВТОНОМНЫЙ ДИНАМИЧЕСКИЙ ПОРОГ ПО ATR ───
                let dynamic_max_ask = if current_atr >= 30.0 {
                    0.24 // При высокой волатильности готовы брать слабую ногу до 24 центов
                } else if current_atr < 15.0 {
                    0.10 // В боковике/тухляке берем только по ультра-скидке до 10 центов
                } else {
                    // Плавная математическая интерполяция между 0.10$ и 0.24$ под живой ATR!
                    let range = 30.0 - 15.0;
                    let factor = (current_atr - 15.0) / range;
                    0.10 + factor * 0.14
                };
                let spot_velocity_for_reason = spot_velocity(spot_signal)
                    .map(|v| format!("{:+.2}", v))
                    .unwrap_or_else(|| "na".to_string());

                if is_up_strong
                    && up_bid >= 0.75
                    && dn_ask <= dynamic_max_ask
                    && dn_ask > 0.0
                    && !counter_velocity_blocks_side("DOWN", spot_signal, current_atr)
                {
                    let target_shares =
                        win_state.initial_down_shares * DYNAMIC_BUY_WEAK_SHARE_FRACTION;
                    let max_usd = win_state.spent * DYNAMIC_BUY_MAX_USD_FRACTION_OF_SPENT;
                    let buy_usd = (target_shares * dn_ask).min(max_usd);
                    if buy_usd > 0.0 {
                        *buy_flag = true;
                        signals.push(OrderSignal {
                            side: "DOWN".to_string(),
                            is_buy: true,
                            amount: buy_usd,
                            price: dn_ask,
                            reason: format!(
                                "dynamic_buy_weak_down_deviation_ok_atr_limit_{:.2}_vel_{}",
                                dynamic_max_ask, spot_velocity_for_reason
                            ),
                        });
                    }
                } else if !is_up_strong
                    && dn_bid >= 0.75
                    && up_ask <= dynamic_max_ask
                    && up_ask > 0.0
                    && !counter_velocity_blocks_side("UP", spot_signal, current_atr)
                {
                    let target_shares =
                        win_state.initial_up_shares * DYNAMIC_BUY_WEAK_SHARE_FRACTION;
                    let max_usd = win_state.spent * DYNAMIC_BUY_MAX_USD_FRACTION_OF_SPENT;
                    let buy_usd = (target_shares * up_ask).min(max_usd);
                    if buy_usd > 0.0 {
                        *buy_flag = true;
                        signals.push(OrderSignal {
                            side: "UP".to_string(),
                            is_buy: true,
                            amount: buy_usd,
                            price: up_ask,
                            reason: format!(
                                "dynamic_buy_weak_up_deviation_ok_atr_limit_{:.2}_vel_{}",
                                dynamic_max_ask, spot_velocity_for_reason
                            ),
                        });
                    }
                }
            }
        }

        // ─── 3. ТРИГГЕР ПЕРЕСЕЧЕНИЯ ЛИНИИ СТАРТА (CROSSOVER БЛОК) ───
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

                // Мониторим пересечение спот-курсом уровня страйка обратно (crossover)
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
                    if dev.zone == DeviationZone::Near {
                        (0.65, "mid_near_strike_wait_reversal") // Сверхблизко: выжидаем полноценный разворот
                    } else if dev.zone == DeviationZone::Moderate {
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
                    if dev.zone == DeviationZone::Near {
                        (0.45, "late_near_strike_wait_reversal")
                    } else if dev.zone == DeviationZone::Moderate || dev.zone == DeviationZone::Far
                    {
                        (0.20, "late_moderate_take_20") // Быстро сбрасываем в легкий плюс
                    } else {
                        (0.12, "late_far_dump_12") // Спасаем крохи
                    }
                } else if time_pct < 90.0 {
                    // Финальная фаза (Последние минуты, распад критический)
                    if dev.zone == DeviationZone::Near {
                        (0.30, "end_near_strike_reversal_hope")
                    } else {
                        (0.10, "end_far_emergency_dump_10") // Шансов почти нет: забираем 10 центов вместо 0
                    }
                } else {
                    // Фаза экспирации (Последний шанс перед сгоранием в ноль)
                    (0.08, "expiration_unconditional_dump_08")
                };
                let weak_velocity_bias =
                    velocity_bias_for_side(second_side == "UP", spot_signal, current_atr);
                let weak_velocity_label = velocity_bias_label(weak_velocity_bias);
                let weak_target =
                    adjust_weak_exit_target(weak_target, weak_velocity_bias, time_pct);

                if second_bid >= weak_target {
                    should_sell = true;
                    reason_str = format!(
                        "tvds_weak_exit_{}_bid_ge_{:.2}_zone_{}_usd_{:.2}_pct_{:.4}_vel_{}",
                        zone_desc,
                        weak_target,
                        dev.zone.as_str(),
                        dev.abs_usd,
                        dev.pct_abs,
                        weak_velocity_label
                    );
                }

                // 2. Дополнительная страховка: Окупаемость раунда по формуле безубытка в поздней фазе (60% - 80%)
                if !should_sell && time_pct >= 60.0 && time_pct < 80.0 {
                    let min_safe_price =
                        (win_state.spent - win_state.cash_returned) / second_shares;
                    if min_safe_price > 0.0 && min_safe_price < 0.65 && second_bid >= min_safe_price
                    {
                        should_sell = true;
                        reason_str =
                            format!("tvds_late_exact_breakeven_bid_ge_{:.2}", min_safe_price);
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
