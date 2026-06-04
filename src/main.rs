#![allow(
    dead_code,
    unused_imports,
    non_snake_case,
    unused_variables,
    unused_mut
)]

mod analytics;
mod client;
mod config;
mod llm;
mod strategy;
mod trader;
mod volatility;

use client::{get_now_ms, MarketEvent, MarketWindow, PricesState};
use config::Config;
use llm::{LlmForecastRequest, LlmForecaster};
use strategy::{
    EntryMode, EntrySignal, LlmForecast, OrderSignal, SpotSignalSnapshot, StrategyEngine,
    LEGACY_CHEAPER_SIDE_RATIO,
};
use trader::{Portfolio, WindowState};
use volatility::VolatilityManager;

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

struct AppState {
    asset: String,
    interval: String,
    config: Config,
    portfolio: Arc<Mutex<Portfolio>>,
    strategy: Arc<Mutex<StrategyEngine>>,
    current_window: Option<WindowState>,
    next_window: Option<WindowState>,
    current_sub: Option<tokio::task::JoinHandle<()>>,
    next_sub: Option<tokio::task::JoinHandle<()>>,
    exclude_slugs: Vec<String>,
    next_window_number: usize,
    system_logs: Vec<String>,
    started_at: i64,
    spot_price: Option<f64>,
    volatility_mgr: VolatilityManager,
    shutdown_pending: bool,
    run_log_dir: String,
    spot_series: SpotSeries,
    llm_forecaster: Option<Arc<LlmForecaster>>,
    llm_forecasts: HashMap<usize, LlmForecast>,
    llm_forecast_attempted: HashSet<usize>,
    llm_forecast_scored: HashSet<usize>,
    llm_correct: u32,
    llm_wrong: u32,
}

#[derive(Debug, Clone, Copy)]
struct SpotSample {
    timestamp_ms: i64,
    price: f64,
}

struct SpotSeries {
    samples: VecDeque<SpotSample>,
    max_samples: usize,
    smoothing_period_sec: f64,
    smoothed_velocity_usd_per_sec: Option<f64>,
    prev_smoothed_velocity_usd_per_sec: Option<f64>,
    acceleration_usd_per_sec2: Option<f64>,
}

impl SpotSeries {
    fn new(max_samples: usize, smoothing_period_sec: f64) -> Self {
        Self {
            samples: VecDeque::with_capacity(max_samples),
            max_samples,
            smoothing_period_sec,
            smoothed_velocity_usd_per_sec: None,
            prev_smoothed_velocity_usd_per_sec: None,
            acceleration_usd_per_sec2: None,
        }
    }

    fn observe(&mut self, timestamp: i64, price: f64) {
        if price <= 0.0 {
            return;
        }

        let timestamp_ms = normalize_event_timestamp_ms(timestamp);
        if let Some(prev) = self.samples.back().copied() {
            if timestamp_ms <= prev.timestamp_ms {
                return;
            }
            let dt_sec = ((timestamp_ms - prev.timestamp_ms) as f64 / 1000.0).max(0.001);
            let raw_velocity = (price - prev.price) / dt_sec;
            let alpha = 1.0 - (-dt_sec / self.smoothing_period_sec.max(0.001)).exp();
            let next_smoothed = match self.smoothed_velocity_usd_per_sec {
                Some(prev_smoothed) => prev_smoothed + alpha * (raw_velocity - prev_smoothed),
                None => raw_velocity,
            };

            self.acceleration_usd_per_sec2 = self
                .smoothed_velocity_usd_per_sec
                .map(|prev_smoothed| (next_smoothed - prev_smoothed) / dt_sec);
            self.prev_smoothed_velocity_usd_per_sec = self.smoothed_velocity_usd_per_sec;
            self.smoothed_velocity_usd_per_sec = Some(next_smoothed);
        }

        self.samples.push_back(SpotSample {
            timestamp_ms,
            price,
        });
        while self.samples.len() > self.max_samples {
            self.samples.pop_front();
        }
    }

    fn snapshot(&self) -> SpotSignalSnapshot {
        SpotSignalSnapshot {
            raw_velocity_usd_per_sec: self.raw_velocity_over_recent_window(20_000),
            smoothed_velocity_usd_per_sec: self.smoothed_velocity_usd_per_sec,
            acceleration_usd_per_sec2: self.acceleration_usd_per_sec2,
        }
    }

    fn raw_velocity_over_recent_window(&self, window_ms: i64) -> Option<f64> {
        let latest = self.samples.back()?;
        let mut earliest = *latest;
        for sample in self.samples.iter().rev() {
            earliest = *sample;
            if latest.timestamp_ms - sample.timestamp_ms >= window_ms {
                break;
            }
        }

        let dt_sec = (latest.timestamp_ms - earliest.timestamp_ms) as f64 / 1000.0;
        if dt_sec <= 0.0 {
            return None;
        }
        Some((latest.price - earliest.price) / dt_sec)
    }
}

fn normalize_event_timestamp_ms(timestamp: i64) -> i64 {
    if timestamp > 0 && timestamp < 10_000_000_000 {
        timestamp * 1000
    } else {
        timestamp
    }
}

fn allocate_entry_usd(
    budget: f64,
    up_ask: f64,
    down_ask: f64,
    cheaper_side_ratio: f64,
) -> (f64, f64) {
    if budget <= 0.0 || up_ask <= 0.0 || down_ask <= 0.0 {
        return (0.0, 0.0);
    }

    if (up_ask - down_ask).abs() < 0.0001 {
        return (budget / 2.0, budget / 2.0);
    }

    let surplus_fraction = ((cheaper_side_ratio.clamp(0.50, 0.70) - 0.50) * 2.0).clamp(0.0, 0.20);
    let core_budget = budget * (1.0 - surplus_fraction);
    let core_shares = core_budget / (up_ask + down_ask);

    let mut buy_up_usd = core_shares * up_ask;
    let mut buy_down_usd = core_shares * down_ask;
    let surplus_usd = budget - buy_up_usd - buy_down_usd;

    if up_ask < down_ask {
        buy_up_usd += surplus_usd;
    } else {
        buy_down_usd += surplus_usd;
    }

    (buy_up_usd, buy_down_usd)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // ─── 1. CLI Arguments & Config ─────────────────────────────────────────
    let args: Vec<String> = std::env::args().collect();
    if args.len() > 1 && (args[1] == "--help" || args[1] == "-h") {
        println!("GEM_RUST — Event-Driven Polymarket Volatility Harvester in Rust\n");
        println!("Usage:\n  cargo run -- <asset> <interval>\n");
        println!("Examples:\n  cargo run -- BTC 5m\n  cargo run -- ETH 15m");
        return Ok(());
    }

    let asset = args
        .get(1)
        .cloned()
        .unwrap_or_else(|| "BTC".to_string())
        .to_uppercase();
    let interval = args
        .get(2)
        .cloned()
        .unwrap_or_else(|| "5m".to_string())
        .to_lowercase();

    let config = match Config::load("config.json") {
        Ok(c) => c,
        Err(_) => {
            eprintln!("Missing or invalid config.json!");
            std::process::exit(1);
        }
    };

    println!("Synchronizing clock with Polymarket server...");
    match client::fetch_time_offset().await {
        Ok(offset) => {
            client::set_time_offset(offset);
            println!("Clock synchronized! Offset: {}ms", offset);
        }
        Err(e) => {
            println!(
                "Warning: Clock sync failed: {}. Using local system clock.",
                e
            );
        }
    }

    // ─── 2. Initialize Channels & Modules ──────────────────────────────────
    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<MarketEvent>();

    let volatility_mgr = VolatilityManager::new();

    // Мгновенный прогрев ATR через REST API Bybit на старте (без ожидания 15 минут!)
    println!("Инициализация GEM_RUST, мгновенный прогрев данных ATR через Bybit REST...");
    if let Err(e) = volatility_mgr.warmup_from_rest().await {
        println!("[ATR Warmup] Предупреждение: не удалось выполнить быстрый прогрев: {:?}. Начинаем стандартное накопление...", e);
    }

    // Запускаем фоновое отслеживание живых тиков
    volatility_mgr.start_tracking();

    let run_id = chrono::Utc::now().format("%Y%m%d_%H%M%S").to_string();
    let run_log_dir = format!(
        "logs/runs/{}_{}_{}_{}",
        run_id,
        asset.to_lowercase(),
        interval,
        config.strategy
    );
    if let Err(e) = std::fs::create_dir_all(&run_log_dir) {
        eprintln!(
            "Failed to create run log directory {}: {:?}",
            run_log_dir, e
        );
    }

    let portfolio = Arc::new(Mutex::new(Portfolio::new_with_log_dir(
        config.session.starting_bank,
        run_log_dir.clone(),
    )));
    let strategy_engine = Arc::new(Mutex::new(StrategyEngine::new(&config.strategy)));
    let llm_forecaster = if config.llm.enabled {
        match LlmForecaster::new("llm.json", config.llm.model.clone()) {
            Ok(forecaster) => {
                println!(
                    "[LLM] Vertex forecast enabled via llm.json | model: {}",
                    config.llm.model
                );
                Some(Arc::new(forecaster))
            }
            Err(e) => {
                println!("[LLM] Forecast disabled: {}", e);
                None
            }
        }
    } else {
        println!("[LLM] Forecast disabled by config");
        None
    };

    let mut app_state = AppState {
        asset: asset.clone(),
        interval: interval.clone(),
        config: config.clone(),
        portfolio: portfolio.clone(),
        strategy: strategy_engine.clone(),
        current_window: None,
        next_window: None,
        current_sub: None,
        next_sub: None,
        exclude_slugs: vec![],
        next_window_number: 0,
        system_logs: vec![],
        started_at: get_now_ms(),
        spot_price: None,
        volatility_mgr: volatility_mgr.clone(),
        shutdown_pending: false,
        run_log_dir,
        spot_series: SpotSeries::new(180, 12.0),
        llm_forecaster,
        llm_forecasts: HashMap::new(),
        llm_forecast_attempted: HashSet::new(),
        llm_forecast_scored: HashSet::new(),
        llm_correct: 0,
        llm_wrong: 0,
    };

    app_state
        .system_logs
        .push(format!("GEM System Initialized for {} {}", asset, interval));
    app_state
        .system_logs
        .push(format!("Run logs: {}", app_state.run_log_dir));
    app_state
        .system_logs
        .push(format!("System clock synchronized. Offset updated."));

    // Spawn Chainlink Spot WS Feed
    let tx_spot = event_tx.clone();
    client::subscribe_chainlink(asset.clone(), tx_spot);

    // Initial Market Discovery
    discover_initial_markets(&mut app_state, &event_tx).await;

    // ─── 3. Event Loop & Tickers ───────────────────────────────────────────
    let mut render_interval = tokio::time::interval(Duration::from_millis(250));
    let mut monitor_interval = tokio::time::interval(Duration::from_millis(1000));

    loop {
        if app_state.shutdown_pending {
            let mut can_exit = false;
            {
                let port = app_state.portfolio.lock().unwrap();
                let has_active = port
                    .windows
                    .values()
                    .any(|w| w.status == "LIVE" || w.status == "ENTERED_PRE_START");
                if !has_active {
                    can_exit = true;
                }
            }
            if can_exit {
                render_dashboard(&app_state);
                println!("\n=================================================================================");
                println!("  \x1b[38;5;114mSESSION DONE!\x1b[0m - All active positions concluded.");
                println!("=================================================================================\n");
                return Ok(());
            }
        }

        tokio::select! {
            // A. Render Terminal Dashboard
            _ = render_interval.tick() => {
                render_dashboard(&app_state);
            }

            // B. Monitor Time Boundaries and Promotion
            _ = monitor_interval.tick() => {
                monitor_time(&mut app_state, &event_tx).await;
            }

            // C. Ingest Events from Central mpsc channel
            maybe_event = event_rx.recv() => {
                if let Some(event) = maybe_event {
                    process_event(&mut app_state, event, &event_tx).await;
                }
            }
        }
    }
}

/**
 * Handle initial market discovery on startup.
 */
async fn discover_initial_markets(
    app: &mut AppState,
    event_tx: &mpsc::UnboundedSender<MarketEvent>,
) {
    app.system_logs
        .push("Searching for active and upcoming windows on Polymarket...".to_string());

    // A. Detect CURRENT Active Window
    if let Some(active) = client::find_active_market(&app.asset, &app.interval).await {
        app.system_logs
            .push(format!("FOUND ACTIVE CURRENT WINDOW: {}", active.slug));
        app.exclude_slugs.push(active.slug.clone());

        let mut port = app.portfolio.lock().unwrap();
        let win_state = port.get_or_create_window_state(0, "CURRENT", &active);
        win_state.status = "LIVE".to_string(); // Live since startup
        app.current_window = Some(win_state.clone());
        app.next_window_number = 1;

        // Subscribe prices
        let handle = client::subscribe_prices(0, "CURRENT".to_string(), active, event_tx.clone());
        app.current_sub = Some(handle);
    } else {
        app.system_logs
            .push("No active window found on Polymarket right now.".to_string());
        app.next_window_number = 1;
    }

    // B. Detect NEXT Upcoming Window
    find_and_subscribe_next(app, event_tx).await;
}

/**
 * Find and subscribe to the NEXT upcoming window.
 */
async fn find_and_subscribe_next(
    app: &mut AppState,
    event_tx: &mpsc::UnboundedSender<MarketEvent>,
) {
    if app.next_window.is_some() {
        return;
    }

    let after_time = match &app.current_window {
        Some(w) => match chrono::DateTime::parse_from_rfc3339(&w.market.start_time) {
            Ok(dt) => dt.timestamp_millis(),
            Err(_) => get_now_ms(),
        },
        None => get_now_ms(),
    };

    app.system_logs.push(format!(
        "Searching for NEXT WINDOW #{}...",
        app.next_window_number
    ));

    if let Some(next_m) =
        client::find_next_market(&app.asset, &app.interval, after_time, &app.exclude_slugs).await
    {
        app.system_logs.push(format!(
            "FOUND NEXT WINDOW #{}: {}",
            app.next_window_number, next_m.slug
        ));
        app.exclude_slugs.push(next_m.slug.clone());

        let mut port = app.portfolio.lock().unwrap();
        let win_state = port.get_or_create_window_state(app.next_window_number, "NEXT", &next_m);
        app.next_window = Some(win_state.clone());

        // Subscribe prices
        let handle = client::subscribe_prices(
            app.next_window_number,
            "NEXT".to_string(),
            next_m,
            event_tx.clone(),
        );
        app.next_sub = Some(handle);
    } else {
        app.system_logs
            .push("No upcoming NEXT window found. Retrying in 10s...".to_string());
        let tx = event_tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(10)).await;
            tx.send(MarketEvent::Log(
                "Triggering NEXT window retry...".to_string(),
            ))
            .unwrap_or_default();
        });
    }
}

async fn get_or_request_llm_forecast(
    app: &mut AppState,
    next: &WindowState,
    prices: &PricesState,
    current_atr: f64,
    secs_to_start: i64,
) -> Option<LlmForecast> {
    if !app.config.llm.enabled {
        return None;
    }
    if let Some(existing) = app.llm_forecasts.get(&next.window_number) {
        return Some(existing.clone());
    }
    if app.llm_forecast_attempted.contains(&next.window_number) {
        return None;
    }

    app.llm_forecast_attempted.insert(next.window_number);
    let Some(forecaster) = app.llm_forecaster.clone() else {
        append_llm_forecast_event(
            &app.run_log_dir,
            next.window_number,
            &next.market.slug,
            None,
            "disabled",
            current_atr,
            secs_to_start,
            app.spot_price,
            prices,
            app.spot_series.snapshot(),
            None,
            None,
        );
        return None;
    };

    let request = LlmForecastRequest {
        asset: app.asset.clone(),
        interval: app.interval.clone(),
        current_time_utc: chrono::Utc::now().to_rfc3339(),
        current_spot: app.spot_price,
        current_atr,
        prices: prices.clone(),
        market: next.market.clone(),
        secs_to_start,
        spot_signal: app.spot_series.snapshot(),
    };

    match tokio::time::timeout(Duration::from_secs(8), forecaster.forecast(request)).await {
        Ok(Ok(forecast)) => {
            app.system_logs.push(format!(
                "[LLM] Forecast Window #{}: {} {:.2} ({})",
                next.window_number, forecast.side, forecast.confidence, forecast.signal_strength
            ));
            append_llm_forecast_event(
                &app.run_log_dir,
                next.window_number,
                &next.market.slug,
                Some(&forecast),
                "ok",
                current_atr,
                secs_to_start,
                app.spot_price,
                prices,
                app.spot_series.snapshot(),
                None,
                None,
            );
            app.llm_forecasts
                .insert(next.window_number, forecast.clone());
            Some(forecast)
        }
        Ok(Err(err)) => {
            let err_text = err.to_string();
            app.system_logs.push(format!(
                "[LLM] Forecast Window #{} failed: {}",
                next.window_number, err_text
            ));
            append_llm_forecast_event(
                &app.run_log_dir,
                next.window_number,
                &next.market.slug,
                None,
                &err_text,
                current_atr,
                secs_to_start,
                app.spot_price,
                prices,
                app.spot_series.snapshot(),
                None,
                None,
            );
            None
        }
        Err(_) => {
            let err_text = "timeout_8s";
            app.system_logs.push(format!(
                "[LLM] Forecast Window #{} failed: {}",
                next.window_number, err_text
            ));
            append_llm_forecast_event(
                &app.run_log_dir,
                next.window_number,
                &next.market.slug,
                None,
                err_text,
                current_atr,
                secs_to_start,
                app.spot_price,
                prices,
                app.spot_series.snapshot(),
                None,
                None,
            );
            None
        }
    }
}

fn record_llm_result(
    app: &mut AppState,
    window_number: usize,
    market: &MarketWindow,
    spot_price: Option<f64>,
    prices: &PricesState,
) {
    if app.llm_forecast_scored.contains(&window_number) {
        return;
    }
    let Some(forecast) = app.llm_forecasts.get(&window_number).cloned() else {
        return;
    };
    let winner = match (spot_price, market.price_to_beat) {
        (Some(spot), Some(ptb)) if ptb > 0.0 && spot > ptb => "UP",
        (Some(_), Some(ptb)) if ptb > 0.0 => "DOWN",
        _ => return,
    };
    let correct = forecast.side == winner;
    if correct {
        app.llm_correct += 1;
    } else {
        app.llm_wrong += 1;
    }
    app.llm_forecast_scored.insert(window_number);
    app.system_logs.push(format!(
        "[LLM] Result Window #{}: forecast {} | winner {} | {}",
        window_number,
        forecast.side,
        winner,
        if correct { "RIGHT" } else { "WRONG" }
    ));
    append_llm_forecast_event(
        &app.run_log_dir,
        window_number,
        &market.slug,
        Some(&forecast),
        "result",
        app.volatility_mgr.get_current_atr(),
        0,
        spot_price,
        prices,
        app.spot_series.snapshot(),
        Some(winner),
        Some(correct),
    );
}

/**
 * Monitor time boundaries: Time Stop, safety close, and NEXT window promotion.
 */
async fn monitor_time(app: &mut AppState, event_tx: &mpsc::UnboundedSender<MarketEvent>) {
    let now = get_now_ms();

    // 1. Мониторинг закупа пре-старта NEXT окна каждую секунду по кэшированным ценам
    let mut trigger_buy = false;
    let mut up_ask_val = 0.0;
    let mut dn_ask_val = 0.0;
    let mut window_num = 0;
    let mut next_market_opt = None;
    let mut entry_signal_opt: Option<EntrySignal> = None;

    if let Some(next) = app.next_window.clone() {
        if next.status == "WAITING_ENTRY" {
            if let Ok(start) = chrono::DateTime::parse_from_rfc3339(&next.market.start_time) {
                let secs_to_start = (start.timestamp_millis() - now) / 1000;

                if secs_to_start <= 0 {
                    promote_next_to_current(app, event_tx).await;
                    return;
                }

                // Проверяем, укладываемся ли в коридор покупки (например, [120с - 5с])
                let is_within_time = secs_to_start
                    >= app.config.pre_start_entry.min_seconds_before_start
                    && secs_to_start <= app.config.pre_start_entry.max_seconds_before_start;
                if is_within_time && !app.shutdown_pending {
                    let current_atr = app.volatility_mgr.get_current_atr();

                    // Логируем причину пропуска по волатильности без спама
                    if current_atr < app.config.min_btc_atr {
                        let log_msg = format!("[STRATEGY] Skipping Window #{}: Volatility too low (ATR: ${:.2} < Min: ${:.2})", next.window_number, current_atr, app.config.min_btc_atr);
                        if !app.system_logs.contains(&log_msg) {
                            app.system_logs.push(log_msg);
                        }
                    } else {
                        let prices = next.prices.clone();
                        let llm_forecast = get_or_request_llm_forecast(
                            app,
                            &next,
                            &prices,
                            current_atr,
                            secs_to_start,
                        )
                        .await;
                        let mut strat = app.strategy.lock().unwrap();
                        if let Some(entry) = strat.check_pre_start_entry(
                            &app.config,
                            &prices,
                            &next.market,
                            app.spot_price,
                            next.window_number,
                            secs_to_start,
                            current_atr,
                            app.spot_series.snapshot(),
                            llm_forecast,
                        ) {
                            trigger_buy = true;
                            up_ask_val = entry.up_ask;
                            dn_ask_val = entry.down_ask;
                            window_num = next.window_number;
                            next_market_opt = Some(next.market.clone());
                            entry_signal_opt = Some(entry);
                        }
                    }
                }
            }
        } else if let Ok(start) = chrono::DateTime::parse_from_rfc3339(&next.market.start_time) {
            let secs_to_start = (start.timestamp_millis() - now) / 1000;
            if secs_to_start <= 0 {
                promote_next_to_current(app, event_tx).await;
                return;
            }
        }
    }

    if trigger_buy {
        if let Some(next_market) = next_market_opt {
            let mut port = app.portfolio.lock().unwrap();
            let entry_signal = entry_signal_opt.unwrap_or(EntrySignal {
                up_ask: up_ask_val,
                down_ask: dn_ask_val,
                budget_multiplier: 1.0,
                cheaper_side_ratio: LEGACY_CHEAPER_SIDE_RATIO,
                mode: EntryMode::Both,
                reason: "fallback_entry_signal".to_string(),
            });

            // Расчитываем динамический бюджет и распределение по сторонам
            let (buy_up_usd, buy_down_usd) = {
                let min_b = app.config.session.min_window_budget;
                let max_b = app.config.session.max_window_budget;
                let pct_b = app.config.session.window_budget_pct;

                // Бюджет на базе % от Equity
                let mut budget = port.equity * (pct_b / 100.0);
                if budget < min_b {
                    budget = min_b;
                }
                if budget > max_b {
                    budget = max_b;
                }
                budget *= entry_signal.budget_multiplier.clamp(0.10, 1.50);

                // Корректируем по доступному кэшу
                if port.available_cash < budget {
                    if port.available_cash >= min_b {
                        budget = port.available_cash;
                    } else {
                        budget = 0.0; // Сигнал отмены (мало средств)
                    }
                }

                if budget > 0.0 {
                    match &entry_signal.mode {
                        EntryMode::Both => allocate_entry_usd(
                            budget,
                            up_ask_val,
                            dn_ask_val,
                            entry_signal.cheaper_side_ratio,
                        ),
                        EntryMode::OneSide(side) if side == "UP" => (budget, 0.0),
                        EntryMode::OneSide(side) if side == "DOWN" => (0.0, budget),
                        EntryMode::OneSide(_) => (0.0, 0.0),
                    }
                } else {
                    (0.0, 0.0)
                }
            };

            let total_cost = buy_up_usd + buy_down_usd;
            if total_cost > 0.0 && port.available_cash >= total_cost {
                let buy_up_shares = if up_ask_val > 0.0 {
                    buy_up_usd / up_ask_val
                } else {
                    0.0
                };
                let buy_down_shares = if dn_ask_val > 0.0 {
                    buy_down_usd / dn_ask_val
                } else {
                    0.0
                };
                app.system_logs.push(format!(
                    "[STRATEGY] Pre-start entry Window #{}: total ${:.2} | UP ${:.2} -> {:.4} sh @ ${:.2} | DOWN ${:.2} -> {:.4} sh @ ${:.2}",
                    window_num,
                    total_cost,
                    buy_up_usd,
                    buy_up_shares,
                    up_ask_val,
                    buy_down_usd,
                    buy_down_shares,
                    dn_ask_val
                ));
                append_entry_event(
                    &app.run_log_dir,
                    window_num,
                    &next_market.slug,
                    &entry_signal,
                    app.llm_forecasts.get(&window_num),
                    app.volatility_mgr.get_current_atr(),
                    total_cost,
                    buy_up_usd,
                    buy_down_usd,
                );

                port.execute_buy(
                    window_num,
                    "UP",
                    buy_up_usd,
                    up_ask_val,
                    &entry_signal.reason,
                );
                port.execute_buy(
                    window_num,
                    "DOWN",
                    buy_down_usd,
                    dn_ask_val,
                    &entry_signal.reason,
                );

                let updated = port.get_or_create_window_state(window_num, "", &next_market);
                app.next_window = Some(updated.clone());
            } else {
                app.system_logs.push(format!(
                    "[STRATEGY] REJECTED entry for Window #{}: Insufficient cash (${:.2} needed, ${:.2} available)",
                    window_num, total_cost, port.available_cash
                ));
            }
        }
    }

    // 2. Safety/Time Stop check for CURRENT window
    let current_opt = app.current_window.clone();
    if let Some(current) = current_opt {
        if current.status == "LIVE" || current.status == "SKIPPED" {
            if let Ok(end) = chrono::DateTime::parse_from_rfc3339(&current.market.end_time) {
                let secs_to_end = (end.timestamp_millis() - now) / 1000;

                if secs_to_end < -10 {
                    // Safety force close past the end
                    app.system_logs.push(format!(
                        "[SAFETY CLOSE] Window #{} past end time ({}s). Force closing.",
                        current.window_number, secs_to_end
                    ));
                    let updated = {
                        let mut port = app.portfolio.lock().unwrap();
                        port.close_window(current.window_number, "CLOSED_TIME", app.spot_price);
                        port.get_or_create_window_state(current.window_number, "", &current.market)
                            .clone()
                    };
                    record_llm_result(
                        app,
                        current.window_number,
                        &updated.market,
                        app.spot_price,
                        &updated.prices,
                    );
                    app.current_window = Some(updated);
                }
            }
        }
    }
}

/**
 * Promote NEXT window to CURRENT (LIVE) window.
 */
async fn promote_next_to_current(
    app: &mut AppState,
    event_tx: &mpsc::UnboundedSender<MarketEvent>,
) {
    let next_win = match &app.next_window {
        Some(w) => w.clone(),
        None => return,
    };

    // Close old CURRENT window if still open
    if let Some(curr) = app.current_window.clone() {
        if curr.status == "LIVE" || curr.status == "SKIPPED" {
            app.system_logs.push(format!(
                "[Lifecycle] Force closing overlapping CURRENT Window #{}",
                curr.window_number
            ));
            let updated = {
                let mut port = app.portfolio.lock().unwrap();
                port.close_window(curr.window_number, "CLOSED_TIME", app.spot_price);
                port.get_or_create_window_state(curr.window_number, "", &curr.market)
                    .clone()
            };
            record_llm_result(
                app,
                curr.window_number,
                &updated.market,
                app.spot_price,
                &updated.prices,
            );
        }
    }

    if let Some(sub) = app.current_sub.take() {
        sub.abort();
    }
    if let Some(sub) = app.next_sub.take() {
        sub.abort();
    }

    // Promote
    let mut port = app.portfolio.lock().unwrap();

    // Explicitly set ALL other windows with role "CURRENT" to "PAST"
    for win in port.windows.values_mut() {
        if win.role == "CURRENT" {
            win.role = "PAST".to_string();
        }
    }

    let is_entered = next_win.status == "ENTERED_PRE_START";
    if is_entered {
        port.entered_windows += 1;
        app.system_logs.push(format!(
            "[Lifecycle] PROMOTE NEXT WINDOW #{} ({}) TO CURRENT (LIVE)",
            next_win.window_number, next_win.market.slug
        ));
    } else {
        port.skipped_windows += 1;
        app.system_logs.push(format!(
            "[Lifecycle] PROMOTE NEXT WINDOW #{} ({}) TO CURRENT (SKIPPED)",
            next_win.window_number, next_win.market.slug
        ));
    }
    append_lifecycle_event(
        &app.run_log_dir,
        next_win.window_number,
        &next_win.market,
        if is_entered {
            "promote_live"
        } else {
            "promote_skipped"
        },
        &next_win.status,
        app.volatility_mgr.get_current_atr(),
        app.spot_price,
        &next_win.prices,
    );

    let promoted =
        port.get_or_create_window_state(next_win.window_number, "CURRENT", &next_win.market);
    if is_entered {
        promoted.status = "LIVE".to_string();
    } else {
        promoted.status = "SKIPPED".to_string();
    }

    app.current_window = Some(promoted.clone());
    app.next_window = None;
    app.next_window_number += 1;

    // Re-subscribe prices under role 'CURRENT'
    let handle = client::subscribe_prices(
        next_win.window_number,
        "CURRENT".to_string(),
        next_win.market.clone(),
        event_tx.clone(),
    );
    app.current_sub = Some(handle);

    // Search for new upcoming NEXT window
    drop(port);
    find_and_subscribe_next(app, event_tx).await;
}

/**
 * Handle incoming channel events from CLOB and Spot price WebSockets.
 */
async fn process_event(
    app: &mut AppState,
    event: MarketEvent,
    _event_tx: &mpsc::UnboundedSender<MarketEvent>,
) {
    match event {
        MarketEvent::Log(msg) => {
            app.system_logs.push(msg);
            if app.system_logs.len() > 30 {
                app.system_logs.remove(0);
            }
        }
        MarketEvent::ShutdownRequested => {
            if !app.shutdown_pending {
                app.shutdown_pending = true;
                app.system_logs.push(
                    "[SYSTEM] SOFT SHUTDOWN INITIATED - NEXT window buys are now disabled!"
                        .to_string(),
                );
            }
        }
        MarketEvent::SpotTick {
            asset: _,
            price,
            timestamp,
        } => {
            let timestamp_ms = normalize_event_timestamp_ms(timestamp);
            app.spot_price = Some(price);
            app.spot_series.observe(timestamp_ms, price);

            // 1. Exact open price (PTB) capture at startTime for CURRENT window if missing
            if let Some(curr) = &mut app.current_window {
                if curr.market.price_to_beat.is_none() {
                    if let Ok(start) = chrono::DateTime::parse_from_rfc3339(&curr.market.start_time)
                    {
                        if timestamp_ms >= start.timestamp_millis() {
                            app.system_logs.push(format!(
                                "[CAPTURE PTB] Captured exact open price from Chainlink WS: ${:.2}",
                                price
                            ));
                            curr.market.price_to_beat = Some(price);

                            // Write back to paper trader!
                            let mut port = app.portfolio.lock().unwrap();
                            let win = port.get_or_create_window_state(
                                curr.window_number,
                                "",
                                &curr.market,
                            );
                            win.market.price_to_beat = Some(price);
                        }
                    }
                }
            }
        }
        MarketEvent::MarketTick {
            window_number,
            role,
            market,
            prices,
            timestamp,
        } => {
            // Update prices inside portfolio
            let mut port = app.portfolio.lock().unwrap();
            let win_state = port
                .get_or_create_window_state(window_number, "", &market)
                .clone();
            port.get_or_create_window_state(window_number, "", &market)
                .prices = prices.clone();

            // Run Strategy Engine
            let mut strat = app.strategy.lock().unwrap();

            if role == "CURRENT" {
                if let Ok(end) = chrono::DateTime::parse_from_rfc3339(&market.end_time) {
                    let secs_to_end = (end.timestamp_millis() - timestamp) / 1000;
                    let current_atr = app.volatility_mgr.get_current_atr();
                    let spot_signal = app.spot_series.snapshot();

                    let signals = strat.process_live_tick(
                        &app.config,
                        &prices,
                        app.spot_price,
                        &win_state.market,
                        &win_state,
                        secs_to_end,
                        current_atr,
                        spot_signal,
                    );

                    for sig in signals {
                        let executed = if sig.is_buy {
                            port.execute_buy(
                                window_number,
                                &sig.side,
                                sig.amount,
                                sig.price,
                                &sig.reason,
                            )
                            .is_some()
                        } else {
                            port.execute_sell(
                                window_number,
                                &sig.side,
                                sig.amount,
                                sig.price,
                                &sig.reason,
                            )
                            .is_some()
                        };
                        append_signal_event(
                            &app.run_log_dir,
                            window_number,
                            &market.slug,
                            &sig,
                            executed,
                            current_atr,
                            app.spot_price,
                            &win_state.market,
                            &prices,
                            &win_state,
                            secs_to_end,
                            spot_signal,
                        );
                    }

                    // Sync local app state
                    let updated = port.get_or_create_window_state(window_number, "", &market);
                    app.current_window = Some(updated.clone());
                }
            }

            // Sync the updated prices to local window
            if role == "CURRENT" && app.current_window.is_some() {
                app.current_window.as_mut().unwrap().prices = prices;
            } else if role == "NEXT" && app.next_window.is_some() {
                app.next_window.as_mut().unwrap().prices = prices;
            }
        }
    }
}

/**
 * ANSI Refreshing Dashboard UI.
 * Clears terminal and draws beautifully formatted, isolated stats and window tables!
 */
fn render_dashboard(app: &AppState) {
    let p = app.portfolio.lock().unwrap().get_portfolio_snapshot();

    // Clear screen and move cursor to top-left
    print!("\x1B[2J\x1B[H");

    println!(
        "{}",
        paint(
            "=================================================================================",
            "dim"
        )
    );
    let strategy_title = format!(
        "STRATEGY: {}",
        app.config.strategy.to_uppercase().replace("_", " ")
    );
    println!(
        "  {}     {}     {}",
        paint(&strategy_title, "bold"),
        paint(&format!("Asset: {}", app.asset), "cyan"),
        paint(&format!("Interval: {}", app.interval), "cyan")
    );
    let llm_total = app.llm_correct + app.llm_wrong;
    let llm_accuracy = if llm_total > 0 {
        (app.llm_correct as f64 / llm_total as f64) * 100.0
    } else {
        0.0
    };
    let llm_enabled = app.config.llm.enabled && app.llm_forecaster.is_some();
    println!(
        "  LLM-forecast: {} | Model: {} | Right {} | Wrong {} | Acc {:.1}%",
        paint(
            if llm_enabled { "enabled" } else { "disabled" },
            if llm_enabled { "green" } else { "dim" }
        ),
        paint(&app.config.llm.model, "cyan"),
        paint(&app.llm_correct.to_string(), "green"),
        paint(&app.llm_wrong.to_string(), "red"),
        llm_accuracy
    );
    if app.shutdown_pending {
        println!(
            "  {}",
            paint("SHUTDOWN PENDING | NEXT window buys are disabled.", "red")
        );
    }
    println!(
        "{}",
        paint(
            "=================================================================================",
            "dim"
        )
    );

    let runtime = format_runtime(get_now_ms() - app.started_at);
    let settled_windows = p.wins + p.losses;
    let open_windows = p.entered_windows.saturating_sub(p.closed_windows);
    let win_pct = if settled_windows > 0 {
        (p.wins as f64 / settled_windows as f64) * 100.0
    } else {
        0.0
    };
    let loss_pct = if settled_windows > 0 {
        (p.losses as f64 / settled_windows as f64) * 100.0
    } else {
        0.0
    };

    let atr = app.volatility_mgr.get_current_atr();
    let atr_str = if atr > 0.0 {
        format!("${:.2}", atr)
    } else {
        "Warming up...".to_string()
    };

    println!(
        "  Started: {} | Runtime: {} | BTC ATR(1m): {}",
        paint(&format_utc(app.started_at), "cyan"),
        paint(&runtime, "bold"),
        paint(&atr_str, "yellow")
    );

    let total_windows = p.entered_windows + p.skipped_windows;
    println!(
        "  Windows: Total {} | Entered {} | Closed {} | Open {} | Skipped {}",
        paint(&total_windows.to_string(), "bold"),
        paint(&p.entered_windows.to_string(), "cyan"),
        paint(&p.closed_windows.to_string(), "green"),
        paint(&open_windows.to_string(), "yellow"),
        paint(&p.skipped_windows.to_string(), "yellow")
    );
    println!(
        "  Results (closed only): Wins {} ({:.1}%) | Losses {} ({:.1}%)",
        paint(&p.wins.to_string(), "green"),
        win_pct,
        paint(&p.losses.to_string(), "red"),
        loss_pct
    );

    let pnl_sign = if p.overall_realized_pnl >= 0.0 {
        "+"
    } else {
        ""
    };
    let pnl_color = if p.overall_realized_pnl >= 0.0 {
        "green"
    } else {
        "red"
    };
    println!(
        "  Starting bank: ${:.2} | Cash: ${:.2} | Equity: ${:.2} | Realized PnL: {}",
        p.starting_bank,
        p.available_cash,
        p.equity,
        paint(
            &format!("{}{:.2}", pnl_sign, p.overall_realized_pnl),
            pnl_color
        )
    );
    println!(
        "{}",
        paint(
            "=================================================================================",
            "dim"
        )
    );

    // Direct reading from portfolio to ensure 100% synchronized, deadlock-free rendering
    let mut current_window = None;
    let mut next_window = None;
    {
        let port = app.portfolio.lock().unwrap();
        for win_state in port.windows.values() {
            if win_state.role == "CURRENT" {
                current_window = Some(win_state.clone());
            } else if win_state.role == "NEXT" {
                next_window = Some(win_state.clone());
            }
        }
    }

    let current_llm = current_window
        .as_ref()
        .and_then(|win| app.llm_forecasts.get(&win.window_number));
    let next_llm = next_window
        .as_ref()
        .and_then(|win| app.llm_forecasts.get(&win.window_number));

    let left_lines = render_window_block(
        &current_window,
        "CURRENT",
        app.spot_price,
        app.strategy.clone(),
        current_llm,
    );
    let right_lines = render_window_block(
        &next_window,
        "NEXT",
        app.spot_price,
        app.strategy.clone(),
        next_llm,
    );

    // Render blocks vertically
    for line in left_lines {
        println!("  {}", line);
    }
    println!(
        "{}",
        paint(
            "─────────────────────────────────────────────────────────────────────────────────",
            "dim"
        )
    );
    for line in right_lines {
        println!("  {}", line);
    }

    println!(
        "{}",
        paint(
            "=================================================================================",
            "dim"
        )
    );
    println!("  {}", paint("SYSTEM EVENT LOG:", "cyan"));
    let max_logs = 6;
    let start_idx = app.system_logs.len().saturating_sub(max_logs);
    for log in &app.system_logs[start_idx..] {
        println!("  • {}", paint(log, "dim"));
    }
    println!(
        "{}",
        paint(
            "=================================================================================",
            "dim"
        )
    );
}

fn render_window_block(
    win_opt: &Option<WindowState>,
    label: &str,
    spot_price: Option<f64>,
    strategy: Arc<Mutex<StrategyEngine>>,
    llm_forecast: Option<&LlmForecast>,
) -> Vec<String> {
    let mut lines = vec![];

    let label_colored = if label == "CURRENT" {
        paint(label, "green")
    } else {
        paint(label, "yellow")
    };

    let Some(win) = win_opt else {
        lines.push(format!("--- {} WINDOW ---", label_colored));
        lines.push(paint("Waiting for market stream...", "dim"));
        return lines;
    };

    let m = &win.market;
    lines.push(format!(
        "--- {} WINDOW #{} ---",
        label_colored,
        paint(&win.window_number.to_string(), "bold")
    ));
    lines.push(format!("Slug: {}", paint(&m.slug, "dim")));

    let start_time = m
        .start_time
        .chars()
        .take(19)
        .collect::<String>()
        .replace("T", " ");
    let end_time = m
        .end_time
        .chars()
        .take(19)
        .collect::<String>()
        .replace("T", " ");
    lines.push(format!("Start: {}", paint(&start_time, "dim")));
    lines.push(format!("End:   {}", paint(&end_time, "dim")));

    let now = get_now_ms();
    let start_dt = chrono::DateTime::parse_from_rfc3339(&m.start_time)
        .unwrap()
        .timestamp_millis();
    let end_dt = chrono::DateTime::parse_from_rfc3339(&m.end_time)
        .unwrap()
        .timestamp_millis();

    if now < start_dt {
        let secs = (start_dt - now) / 1000;
        lines.push(format!(
            "Status: {} | Starts In: {}",
            paint("WAITING", "yellow"),
            paint(&format_countdown(secs), "bold")
        ));
    } else if now < end_dt {
        let secs = (end_dt - now) / 1000;
        lines.push(format!(
            "Status: {} | Time Left: {}",
            paint("LIVE", "green"),
            paint(&format_countdown(secs), "bold")
        ));
    } else {
        lines.push(format!("Status: {}", paint("EXPIRED", "red")));
    }

    let strike_str = m
        .price_to_beat
        .map(|p| format!("${:.2}", p))
        .unwrap_or_else(|| "N/A".to_string());
    lines.push(format!(
        "Price to Beat (Strike): {}",
        paint(&strike_str, "magenta")
    ));

    let spot_str = spot_price
        .map(|p| format!("${:.2}", p))
        .unwrap_or_else(|| "N/A".to_string());
    let distance_str = match m.get_ptb_deviation(spot_price) {
        Some((delta, pct)) => {
            let (tone, formatted) = if delta >= 0.0 {
                ("green", format!("+${:.2} (+{:.4}%)", delta, pct))
            } else {
                ("red", format!("-${:.2} ({:.4}%)", delta.abs(), pct))
            };
            paint(&formatted, tone)
        }
        None => paint("N/A", "dim"),
    };
    lines.push(format!(
        "Live Spot Price: {} | Dist: {}",
        paint(&spot_str, "cyan"),
        distance_str
    ));
    lines.push(paint("--------------------------------------", "dim"));

    let UP = &win.prices.up;
    let DN = &win.prices.down;
    let up_price = if UP.ask > 0.0 { UP.ask } else { UP.bid };
    let dn_price = if DN.ask > 0.0 { DN.ask } else { DN.bid };
    let up_chance = (up_price * 100.0).clamp(0.0, 100.0);
    let dn_chance = (dn_price * 100.0).clamp(0.0, 100.0);

    lines.push(format!(
        "UP   YES Bid/Ask: {:.2} / {}  [{}]",
        UP.bid,
        paint(&format!("{:.2}", UP.ask), "green"),
        paint(&format!("{:.1}%", up_chance), "green")
    ));
    lines.push(format!(
        "DOWN YES Bid/Ask: {:.2} / {}  [{}]",
        DN.bid,
        paint(&format!("{:.2}", DN.ask), "red"),
        paint(&format!("{:.1}%", dn_chance), "red")
    ));
    lines.push(format!(
        "Combined Ask:    {}",
        paint(&format!("{:.2}", UP.ask + DN.ask), "bold")
    ));
    if let Some(forecast) = llm_forecast {
        let tone = if forecast.side == "UP" {
            "green"
        } else {
            "red"
        };
        lines.push(format!(
            "LLM Forecast: {} | conf {:.2} | {}",
            paint(&forecast.side, tone),
            forecast.confidence,
            paint(&forecast.signal_strength, "yellow")
        ));
    } else {
        lines.push(format!("LLM Forecast: {}", paint("N/A", "dim")));
    }
    lines.push(paint("--------------------------------------", "dim"));

    // Display Account position
    let spent = win.spent;
    let returned = win.cash_returned;
    let mtm = win.up_shares * UP.bid + win.down_shares * DN.bid;
    let pnl = (returned + mtm) - spent;
    lines.push(format!("Spent: ${:.2} | Returned: ${:.2}", spent, returned));

    let pnl_sign = if pnl >= 0.0 { "+" } else { "" };
    let pnl_tone = if pnl >= 0.0 { "green" } else { "red" };
    lines.push(format!(
        "Est. Val: ${:.2} | PnL: {}",
        returned + mtm,
        paint(&format!("{}{:.2}", pnl_sign, pnl), pnl_tone)
    ));
    lines.push(format!(
        "UP shares: {} | DOWN shares: {}",
        paint(&format!("{:.4}", win.up_shares), "green"),
        paint(&format!("{:.4}", win.down_shares), "red")
    ));
    let paired_shares = win.up_shares.min(win.down_shares);
    let terminal_floor = returned + paired_shares;
    let floor_gap = terminal_floor - spent;
    let floor_tone = if floor_gap >= 0.0 { "green" } else { "yellow" };
    lines.push(format!(
        "Paired floor: ${:.2} = returned ${:.2} + paired {:.4} sh | BE gap: {}",
        terminal_floor,
        returned,
        paired_shares,
        paint(&format!("{:+.2}", floor_gap), floor_tone)
    ));
    lines.push(paint("--------------------------------------", "dim"));

    // Strategy status
    let strat_engine = strategy.lock().unwrap();
    if let Some(strat) = strat_engine.get_strategy_state(win.window_number) {
        if strat.first_sold_side.is_none() {
            lines.push(format!(
                "Exit Trigger: {} (Active both)",
                paint(">= 0.65", "yellow")
            ));
        } else {
            let second = if strat.first_sold_side.as_deref() == Some("UP") {
                "DOWN"
            } else {
                "UP"
            };
            let live_leader = if UP.bid >= DN.bid + 0.02 {
                "UP"
            } else if DN.bid >= UP.bid + 0.02 {
                "DOWN"
            } else {
                "TIE"
            };
            let itm_side = match (spot_price, m.price_to_beat) {
                (Some(spot), Some(ptb)) if ptb > 0.0 && spot > ptb => Some("UP"),
                (Some(_), Some(ptb)) if ptb > 0.0 => Some("DOWN"),
                _ => None,
            };
            let weak_exit_blocked =
                live_leader == second || itm_side.map(|side| side == second).unwrap_or(false);
            let weak_exit_status = if weak_exit_blocked {
                paint("blocked: second is live-strong/ITM", "green")
            } else {
                paint("armed: partial sell, insurance tail kept", "yellow")
            };
            let crossed = if strat.ptb_crossed {
                paint("PTB Crossed! Active!", "green")
            } else {
                paint("Waiting PTB cross...", "yellow")
            };
            lines.push(format!(
                "First Sold: {}",
                paint(strat.first_sold_side.as_ref().unwrap(), "green")
            ));
            lines.push(format!(
                "Live Leader: {} | ITM: {}",
                paint(
                    live_leader,
                    match live_leader {
                        "UP" => "green",
                        "DOWN" => "red",
                        _ => "yellow",
                    }
                ),
                paint(
                    itm_side.unwrap_or("N/A"),
                    match itm_side.unwrap_or("N/A") {
                        "UP" => "green",
                        "DOWN" => "red",
                        _ => "dim",
                    }
                ),
            ));
            lines.push(format!(
                "Crossover Weak Exit ({}): {}",
                paint(second, "yellow"),
                weak_exit_status
            ));
            lines.push(format!("Cross Status: {}", crossed));
        }
    } else {
        lines.push(paint("Exit Trigger: WAITING PRE-ENTRY", "dim"));
    }

    // Trade Log
    lines.push(paint("TRADES LOG:", "dim"));
    let trades = &win.trades;
    let max_trades_vis = 10;
    let start_tr_idx = trades.len().saturating_sub(max_trades_vis);
    for t in &trades[start_tr_idx..] {
        let ts_str = format_timestamp(t.timestamp);
        let trade_tone = match t.trade_type.as_str() {
            "BUY" => "green",
            "SELL" => "yellow",
            "REDEEM" => "green",
            "EXPIRED" => "red",
            _ => "dim",
        };
        let side_tone = if t.side == "UP" { "green" } else { "red" };
        lines.push(format!(
            "[{}] {} {} {:.4} sh @ ${:.2} = ${:.2} | cash ${:.2}",
            paint(&ts_str, "dim"),
            paint(&t.trade_type, trade_tone),
            paint(&t.side, side_tone),
            t.shares,
            t.price,
            t.usd_value,
            t.available_cash_after
        ));
        lines.push(format!("      {}", paint(&t.reason, "dim")));
    }

    lines
}

fn append_csv_row(log_dir: &str, file_name: &str, header: &str, row: &str) {
    if let Err(e) = std::fs::create_dir_all(log_dir) {
        eprintln!("Failed to create log dir {}: {:?}", log_dir, e);
        return;
    }
    let path = std::path::Path::new(log_dir).join(file_name);
    let file_exists = path.exists();
    match std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .append(true)
        .open(&path)
    {
        Ok(mut file) => {
            use std::io::Write;
            if !file_exists {
                let _ = writeln!(file, "{}", header);
            }
            let _ = writeln!(file, "{}", row);
        }
        Err(e) => eprintln!("Failed to append {}: {:?}", path.display(), e),
    }
}

fn csv_cell(value: &str) -> String {
    let escaped = value.replace('"', "\"\"").replace('\n', " ");
    if escaped.contains(',') || escaped.contains('"') || escaped.contains('\r') {
        format!("\"{}\"", escaped)
    } else {
        escaped
    }
}

fn append_entry_event(
    log_dir: &str,
    window_number: usize,
    slug: &str,
    entry: &EntrySignal,
    llm_forecast: Option<&LlmForecast>,
    current_atr: f64,
    total_budget: f64,
    buy_up_usd: f64,
    buy_down_usd: f64,
) {
    let (entry_mode, entry_side) = match &entry.mode {
        EntryMode::Both => ("both", ""),
        EntryMode::OneSide(side) => ("one_side", side.as_str()),
    };
    append_csv_row(
        log_dir,
        "entry_events.csv",
        "timestamp,window_id,slug,reason,entry_mode,entry_side,llm_side,llm_confidence,llm_strength,llm_reason,current_atr,up_ask,down_ask,budget_multiplier,cheaper_side_ratio,total_budget,buy_up_usd,buy_up_shares,buy_down_usd,buy_down_shares",
        &format!(
            "{},{},{},{},{},{},{},{:.4},{},{},{:.4},{:.4},{:.4},{:.4},{:.4},{:.4},{:.4},{:.8},{:.4},{:.8}",
            get_now_ms(),
            window_number,
            slug,
            csv_cell(&entry.reason),
            entry_mode,
            entry_side,
            llm_forecast
                .map(|forecast| forecast.side.as_str())
                .unwrap_or(""),
            llm_forecast
                .map(|forecast| forecast.confidence)
                .unwrap_or(0.0),
            llm_forecast
                .map(|forecast| forecast.signal_strength.as_str())
                .unwrap_or(""),
            csv_cell(
                llm_forecast
                    .map(|forecast| forecast.reason_short.as_str())
                    .unwrap_or("")
            ),
            current_atr,
            entry.up_ask,
            entry.down_ask,
            entry.budget_multiplier,
            entry.cheaper_side_ratio,
            total_budget,
            buy_up_usd,
            if entry.up_ask > 0.0 {
                buy_up_usd / entry.up_ask
            } else {
                0.0
            },
            buy_down_usd,
            if entry.down_ask > 0.0 {
                buy_down_usd / entry.down_ask
            } else {
                0.0
            }
        ),
    );
}

fn append_llm_forecast_event(
    log_dir: &str,
    window_number: usize,
    slug: &str,
    forecast: Option<&LlmForecast>,
    status: &str,
    current_atr: f64,
    secs_to_start: i64,
    spot_price: Option<f64>,
    prices: &PricesState,
    spot_signal: SpotSignalSnapshot,
    result_winner: Option<&str>,
    result_correct: Option<bool>,
) {
    append_csv_row(
        log_dir,
        "llm_forecasts.csv",
        "timestamp,window_id,slug,status,llm_side,llm_confidence,llm_strength,llm_reason,llm_risk,key_drivers,result_winner,result_correct,current_atr,secs_to_start,spot_price,spot_velocity_usd_per_sec,spot_smoothed_velocity_usd_per_sec,spot_acceleration_usd_per_sec2,up_bid,up_ask,down_bid,down_ask",
        &format!(
            "{},{},{},{},{},{:.4},{},{},{},{},{},{},{:.4},{},{},{},{},{},{:.4},{:.4},{:.4},{:.4}",
            get_now_ms(),
            window_number,
            slug,
            csv_cell(status),
            forecast.map(|forecast| forecast.side.as_str()).unwrap_or(""),
            forecast.map(|forecast| forecast.confidence).unwrap_or(0.0),
            forecast
                .map(|forecast| forecast.signal_strength.as_str())
                .unwrap_or(""),
            csv_cell(
                forecast
                    .map(|forecast| forecast.reason_short.as_str())
                    .unwrap_or("")
            ),
            csv_cell(
                forecast
                    .map(|forecast| forecast.risk_note.as_str())
                    .unwrap_or("")
            ),
            csv_cell(
                &forecast
                    .map(|forecast| forecast.key_drivers.join(" | "))
                    .unwrap_or_default()
            ),
            result_winner.unwrap_or(""),
            result_correct
                .map(|correct| if correct { "true" } else { "false" })
                .unwrap_or(""),
            current_atr,
            secs_to_start,
            spot_price
                .map(|p| format!("{:.4}", p))
                .unwrap_or_else(|| "".to_string()),
            spot_signal
                .raw_velocity_usd_per_sec
                .map(|v| format!("{:.6}", v))
                .unwrap_or_else(|| "".to_string()),
            spot_signal
                .smoothed_velocity_usd_per_sec
                .map(|v| format!("{:.6}", v))
                .unwrap_or_else(|| "".to_string()),
            spot_signal
                .acceleration_usd_per_sec2
                .map(|v| format!("{:.6}", v))
                .unwrap_or_else(|| "".to_string()),
            prices.up.bid,
            prices.up.ask,
            prices.down.bid,
            prices.down.ask,
        ),
    );
}

fn append_signal_event(
    log_dir: &str,
    window_number: usize,
    slug: &str,
    sig: &OrderSignal,
    executed: bool,
    current_atr: f64,
    spot_price: Option<f64>,
    market: &MarketWindow,
    prices: &PricesState,
    win_state: &WindowState,
    secs_to_end: i64,
    spot_signal: SpotSignalSnapshot,
) {
    let duration_ms = match (
        chrono::DateTime::parse_from_rfc3339(&market.start_time),
        chrono::DateTime::parse_from_rfc3339(&market.end_time),
    ) {
        (Ok(s), Ok(e)) => (e.timestamp_millis() - s.timestamp_millis()) as f64,
        _ => 900_000.0,
    };
    let duration_sec = duration_ms / 1000.0;
    let elapsed_sec = (duration_sec - secs_to_end as f64).clamp(0.0, duration_sec);
    let time_pct = (elapsed_sec / duration_sec) * 100.0;
    let (ptb_delta_usd, ptb_delta_pct) = match (spot_price, market.price_to_beat) {
        (Some(spot), Some(ptb)) if ptb > 0.0 => {
            let delta = spot - ptb;
            (Some(delta), Some((delta / ptb) * 100.0))
        }
        _ => (None, None),
    };
    let mtm = win_state.up_shares * prices.up.bid + win_state.down_shares * prices.down.bid;
    let unrealized_pnl = win_state.cash_returned + mtm - win_state.spent;
    let paired_shares = win_state.up_shares.min(win_state.down_shares);
    let terminal_floor = win_state.cash_returned + paired_shares;
    let terminal_floor_gap = terminal_floor - win_state.spent;
    let (signal_amount_kind, signal_shares, signal_usd_value) = if sig.is_buy {
        let shares = if sig.price > 0.0 {
            sig.amount / sig.price
        } else {
            0.0
        };
        ("usd", shares, sig.amount)
    } else {
        ("shares", sig.amount, sig.amount * sig.price)
    };
    let spot_velocity = spot_signal
        .raw_velocity_usd_per_sec
        .map(|v| format!("{:.6}", v))
        .unwrap_or_else(|| "".to_string());
    let spot_smoothed_velocity = spot_signal
        .smoothed_velocity_usd_per_sec
        .map(|v| format!("{:.6}", v))
        .unwrap_or_else(|| "".to_string());
    let spot_acceleration = spot_signal
        .acceleration_usd_per_sec2
        .map(|v| format!("{:.6}", v))
        .unwrap_or_else(|| "".to_string());

    append_csv_row(
        log_dir,
        "strategy_signals.csv",
        "timestamp,window_id,slug,action,side,amount,amount_kind,signal_shares,signal_usd_value,price,reason,executed,current_atr,secs_to_end,time_pct,spot_price,spot_velocity_usd_per_sec,spot_smoothed_velocity_usd_per_sec,spot_acceleration_usd_per_sec2,ptb,ptb_delta_usd,ptb_delta_pct,up_bid,up_ask,down_bid,down_ask,up_shares,down_shares,paired_shares,spent,returned,terminal_floor,terminal_floor_gap,mtm,unrealized_pnl",
        &format!(
            "{},{},{},{},{},{:.8},{},{:.8},{:.4},{:.4},{},{},{:.4},{},{:.2},{},{},{},{},{},{},{},{:.4},{:.4},{:.4},{:.4},{:.8},{:.8},{:.8},{:.4},{:.4},{:.4},{:.4},{:.4},{:.4}",
            get_now_ms(),
            window_number,
            slug,
            if sig.is_buy { "BUY" } else { "SELL" },
            sig.side,
            sig.amount,
            signal_amount_kind,
            signal_shares,
            signal_usd_value,
            sig.price,
            sig.reason,
            executed,
            current_atr,
            secs_to_end,
            time_pct,
            spot_price
                .map(|p| format!("{:.4}", p))
                .unwrap_or_else(|| "".to_string()),
            spot_velocity,
            spot_smoothed_velocity,
            spot_acceleration,
            market
                .price_to_beat
                .map(|p| format!("{:.4}", p))
                .unwrap_or_else(|| "".to_string()),
            ptb_delta_usd
                .map(|p| format!("{:.4}", p))
                .unwrap_or_else(|| "".to_string()),
            ptb_delta_pct
                .map(|p| format!("{:.6}", p))
                .unwrap_or_else(|| "".to_string()),
            prices.up.bid,
            prices.up.ask,
            prices.down.bid,
            prices.down.ask,
            win_state.up_shares,
            win_state.down_shares,
            paired_shares,
            win_state.spent,
            win_state.cash_returned,
            terminal_floor,
            terminal_floor_gap,
            mtm,
            unrealized_pnl
        ),
    );
}

fn append_lifecycle_event(
    log_dir: &str,
    window_number: usize,
    market: &MarketWindow,
    event: &str,
    status_before: &str,
    current_atr: f64,
    spot_price: Option<f64>,
    prices: &PricesState,
) {
    let (ptb_delta_usd, ptb_delta_pct) = match (spot_price, market.price_to_beat) {
        (Some(spot), Some(ptb)) if ptb > 0.0 => {
            let delta = spot - ptb;
            (Some(delta), Some((delta / ptb) * 100.0))
        }
        _ => (None, None),
    };

    append_csv_row(
        log_dir,
        "lifecycle_events.csv",
        "timestamp,window_id,slug,event,status_before,current_atr,spot_price,ptb,ptb_delta_usd,ptb_delta_pct,up_bid,up_ask,down_bid,down_ask",
        &format!(
            "{},{},{},{},{},{:.4},{},{},{},{},{:.4},{:.4},{:.4},{:.4}",
            get_now_ms(),
            window_number,
            market.slug,
            event,
            status_before,
            current_atr,
            spot_price
                .map(|p| format!("{:.4}", p))
                .unwrap_or_else(|| "".to_string()),
            market
                .price_to_beat
                .map(|p| format!("{:.4}", p))
                .unwrap_or_else(|| "".to_string()),
            ptb_delta_usd
                .map(|p| format!("{:.4}", p))
                .unwrap_or_else(|| "".to_string()),
            ptb_delta_pct
                .map(|p| format!("{:.6}", p))
                .unwrap_or_else(|| "".to_string()),
            prices.up.bid,
            prices.up.ask,
            prices.down.bid,
            prices.down.ask
        ),
    );
}

// ─── Formatting Helpers ─────────────────────────────────────────

fn visible_len(s: &str) -> usize {
    let mut len = 0;
    let mut in_esc = false;
    for c in s.chars() {
        if c == '\x1b' {
            in_esc = true;
        } else if in_esc {
            if c == 'm' {
                in_esc = false;
            }
        } else {
            len += 1;
        }
    }
    len
}

fn pad_right(s: &str, width: usize) -> String {
    let vis = visible_len(s);
    if vis >= width {
        s.to_string()
    } else {
        format!("{}{}", s, " ".repeat(width - vis))
    }
}

fn paint(value: &str, tone: &str) -> String {
    let code = match tone {
        "bold" => "\x1b[1;38;5;252m",
        "dim" => "\x1b[38;5;245m",
        "green" => "\x1b[38;5;114m",
        "red" => "\x1b[38;5;174m",
        "yellow" => "\x1b[38;5;179m",
        "cyan" => "\x1b[38;5;81m",
        "magenta" => "\x1b[38;5;198m",
        _ => "",
    };
    format!("{code}{value}\x1b[0m")
}

fn format_runtime(ms: i64) -> String {
    let total_sec = ms / 1000;
    let h = total_sec / 3600;
    let m = (total_sec % 3600) / 60;
    let s = total_sec % 60;
    format!("{:02}:{:02}:{:02}", h, m, s)
}

fn format_countdown(secs: i64) -> String {
    if secs < 0 {
        return "00:00".to_string();
    }
    let m = secs / 60;
    let s = secs % 60;
    format!("{:02}:{:02}", m, s)
}

fn format_utc(ms: i64) -> String {
    let s = ms / 1000;
    let ns = (ms % 1000) * 1_000_000;
    if let Some(dt) = chrono::DateTime::from_timestamp(s, ns as u32) {
        dt.to_rfc3339()
            .chars()
            .take(19)
            .collect::<String>()
            .replace("T", " ")
    } else {
        "N/A".to_string()
    }
}

fn format_timestamp(ms: i64) -> String {
    let s = ms / 1000;
    let ns = (ms % 1000) * 1_000_000;
    if let Some(dt) = chrono::DateTime::from_timestamp(s, ns as u32) {
        dt.to_rfc3339().chars().skip(11).take(8).collect::<String>()
    } else {
        "N/A".to_string()
    }
}
