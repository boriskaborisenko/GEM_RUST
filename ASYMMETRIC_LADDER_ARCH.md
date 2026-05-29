# Архитектура Модульных Стратегий «stratBox» — GEM_RUST

Этот документ содержит готовый архитектурный чертеж, математический паспорт порогов асимметричной лесенки и шаблоны кода на Rust для создания расширяемой модульной системы стратегий (**`stratBox`**).

---

## 1. Спецификация Асимметричной Лесенки (План порогов)

Модель основана на асимметрии **55% на 45%** (наш Sweet Spot). Общий бюджет сделки $C = 30\$$ (всего 60 акций при цене входа 0.50$).

### Пропорции входа (Hedge):
* **Сильная сторона (по тренду):** `16.5$` (закупаем **33 акции**).
* **Слабая сторона (против тренда):** `13.5$` (закупаем **27 акций**).

### Правила выхода по ступеням (Лесенка):
Каждая сторона распродается строго в **две ступени (по 50% от объема стороны)**:

| Сторона | Ступень | Объем | Цена выхода (Тейк) | Назначение |
| :--- | :---: | :---: | :---: | :--- |
| **Сильная (UP)** | **1** | `16.5 штук` | **`0.62$`** | Быстрый безубыток (возвращает `40%` всех затрат) |
| **Сильная (UP)** | **2** | `16.5 штук` | **`0.72$`** | Фиксация основной прибыли |
| **Слабая (DOWN)** | **1** | `13.5 штук` | **`0.70$`** | Взятие первого импульса разворота спота |
| **Слабая (DOWN)** | **2** | `13.5 штук` | **`0.85$`** | Экстремальный профит разворота (жадный тейк) |

---

## 2. Архитектура Модуля «strategies/»

Для масштабирования бота мы реорганизуем код, выделив его в отдельную папку `strategies/` с использованием полиморфизма (Rust Traits).

### Карта будущей структуры файлов:
```text
GEM_RUST/src/
├── main.rs
├── trader.rs
├── client.rs
├── config.rs
└── strategies/              <-- Папка со стратегиями
    ├── mod.rs               <-- stratBox (диспетчер стратегий)
    ├── strategy_a.rs        <-- Базовая «тупая» стратегия (текущая)
    └── strategy_b.rs        <-- Асимметричная лесенка (наш Sweet Spot)
```

---

## 3. Программный Чертеж (Шаблоны Rust-кода)

### А. Трейт стратегии (`strategies/mod.rs`)
Общий интерфейс, который обязана реализовать любая стратегия:

```rust
use crate::client::{MarketWindow, PricesState};
use crate::trader::WindowState;
use crate::config::Config;
use crate::strategy::OrderSignal;

pub trait TradeStrategy {
    // Проверка условий входа pre-start
    fn check_pre_start_entry(
        &mut self,
        config: &Config,
        prices: &PricesState,
        window_number: usize,
        secs_to_start: i64,
    ) -> Option<(f64, f64)>;

    // Обработка тиков во время LIVE
    fn process_live_tick(
        &mut self,
        config: &Config,
        prices: &PricesState,
        spot_price: Option<f64>,
        market: &MarketWindow,
        win_state: &WindowState,
        secs_to_end: i64,
    ) -> Vec<OrderSignal>;
}
```

---

### Б. Диспетчер Стратегий `stratBox` (`strategies/mod.rs`)
Определяет, какую стратегию запустить на основе параметра `"strategy"` в `config.json`:

```rust
pub mod strategy_a;
pub mod strategy_b;

pub enum StrategyType {
    SimpleBoth,        // Базовая «тупая» (Strategy A)
    AsymmetricLadder,  // Асимметричная лесенка (Strategy B)
}

pub struct StratBox {
    pub active_type: StrategyType,
    pub strat_a: strategy_a::SimpleBothStrategy,
    pub strat_b: strategy_b::AsymmetricLadderStrategy,
}

impl StratBox {
    pub fn new(strategy_name: &str) -> Self {
        let active_type = match strategy_name {
            "asymmetric_ladder" => StrategyType::AsymmetricLadder,
            _ => StrategyType::SimpleBoth,
        };
        Self {
            active_type,
            strat_a: strategy_a::SimpleBothStrategy::new(),
            strat_b: strategy_b::AsymmetricLadderStrategy::new(),
        }
    }
}

// Перенаправляем вызовы к активной стратегии
impl TradeStrategy for StratBox {
    fn check_pre_start_entry(&mut self, config: &Config, prices: &PricesState, window_number: usize, secs_to_start: i64) -> Option<(f64, f64)> {
        match self.active_type {
            StrategyType::SimpleBoth => self.strat_a.check_pre_start_entry(config, prices, window_number, secs_to_start),
            StrategyType::AsymmetricLadder => self.strat_b.check_pre_start_entry(config, prices, window_number, secs_to_start),
        }
    }

    fn process_live_tick(&mut self, config: &Config, prices: &PricesState, spot_price: Option<f64>, market: &MarketWindow, win_state: &WindowState, secs_to_end: i64) -> Vec<OrderSignal> {
        match self.active_type {
            StrategyType::SimpleBoth => self.strat_a.process_live_tick(config, prices, spot_price, market, win_state, secs_to_end),
            StrategyType::AsymmetricLadder => self.strat_b.process_live_tick(config, prices, spot_price, market, win_state, secs_to_end),
        }
    }
}
```

---

### В. Шаблон Асимметричной Лесенки (`strategies/strategy_b.rs`)
Внутренний стейт и исполнение лесенки без проверки спота и PTB:

```rust
use crate::strategies::TradeStrategy;
// ... импорты ...

pub struct AsymmetricLadderStrategy {
    pub up_ladder_step: usize,   // Отслеживаем текущую ступень продажи UP
    pub down_ladder_step: usize, // Отслеживаем текущую ступень продажи DOWN
}

impl AsymmetricLadderStrategy {
    pub fn new() -> Self {
        Self {
            up_ladder_step: 0,
            down_ladder_step: 0,
        }
    }
}

impl TradeStrategy for AsymmetricLadderStrategy {
    fn check_pre_start_entry(...) -> Option<(f64, f64)> {
        // Логика закупа 50/51...
    }

    fn process_live_tick(...) -> Vec<OrderSignal> {
        let mut signals = vec![];
        
        // Массив ступеней тейка для сильной и слабой стороны
        let up_steps = [0.62, 0.72];   // Для сильной UP
        let dn_steps = [0.70, 0.85];   // Для слабой DOWN

        // ─── Ступенчатый выход для Сильной стороны (UP) ───
        if self.up_ladder_step < up_steps.len() {
            let target = up_steps[self.up_ladder_step];
            if up_bid >= target && win_state.up_shares > 0.0 {
                let sell_amount = if self.up_ladder_step == 0 {
                    win_state.initial_up_shares * 0.50 // Продаем 50% от НАЧАЛЬНЫХ акций
                } else {
                    win_state.up_shares // Сливаем весь остаток на последней ступени
                };
                
                self.up_ladder_step += 1;
                signals.push(OrderSignal {
                    side: "UP".to_string(),
                    is_buy: false,
                    amount: sell_amount,
                    price: up_bid,
                    reason: format!("ladder_exit_step_{}_{}", self.up_ladder_step, target),
                });
            }
        }

        // ─── Ступенчатый выход для Слабой стороны (DOWN) ───
        if self.down_ladder_step < dn_steps.len() {
            let target = dn_steps[self.down_ladder_step];
            if dn_bid >= target && win_state.down_shares > 0.0 {
                let sell_amount = if self.down_ladder_step == 0 {
                    win_state.initial_down_shares * 0.50
                } else {
                    win_state.down_shares
                };
                
                self.down_ladder_step += 1;
                signals.push(OrderSignal {
                    side: "DOWN".to_string(),
                    is_buy: false,
                    amount: sell_amount,
                    price: dn_bid,
                    reason: format!("ladder_exit_step_{}_{}", self.down_ladder_step, target),
                });
            }
        }

        signals
    }
}
```

Этот чертеж дает полную свободу для калибровки и сборки абсолютно любых новых стратегий в будущем без коверкания основного кода бота!
