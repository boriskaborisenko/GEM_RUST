#![allow(
    dead_code,
    unused_imports,
    non_snake_case,
    unused_variables,
    unused_mut
)]

mod analytics;
mod asset_price;
mod cex_micro;
mod client;
mod config;
mod h_stats;
mod j_controller;
mod j_fees;
mod llm;
mod mid_cross_tracker;
mod orderbook;
mod redeem_hold;
mod strategy;
mod trade_tape;
mod trader;
mod volatility;
mod window_stats;

use asset_price::{format_asset_price, format_atr, ptb_implausible};
use cex_micro::CexMicroManager;
use client::{get_now_ms, MarketEvent, MarketWindow, PricesState};
use config::Config;
use llm::{LlmForecastRequest, LlmForecaster, LlmRecentWindowContext, LlmRecentWindowRow};
use mid_cross_tracker::{LeadSide, MidCrossEvent, MidCrossSnapshot, MidCrossTracker};
use redeem_hold::{
    evaluate_redeem_hold, itm_gap_z, side_is_itm, RedeemHoldInput, REDEEM_HOLD_MIN_VALID_ATR,
};
use strategy::{
    CexMicroSnapshot, EntryMode, EntrySignal, LlmForecast, OrderSignal, SpotSignalSnapshot,
    StrategyEngine, TradeTapeSnapshot, LEGACY_CHEAPER_SIDE_RATIO,
};
use trade_tape::TradeTapeTracker;
use trader::{Portfolio, TradeRecord, WindowCloseMeta, WindowState};
use volatility::VolatilityManager;
use window_stats::{WindowCloseRecord, WindowStatsAggregator};

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
    volatility_mgr: Arc<VolatilityManager>,
    shutdown_pending: bool,
    run_log_dir: String,
    spot_series: SpotSeries,
    llm_forecaster: Option<Arc<LlmForecaster>>,
    llm_forecasts: HashMap<usize, LlmForecast>,
    llm_forecast_attempted: HashSet<usize>,
    llm_forecast_scored: HashSet<usize>,
    llm_correct: u32,
    llm_wrong: u32,
    mid_cross_tracker: MidCrossTracker,
    cex_micro_mgr: CexMicroManager,
    trade_tape: TradeTapeTracker,
    window_stats: WindowStatsAggregator,
    ptb_locked_windows: HashSet<usize>,
    maintenance: MaintenanceStatus,
    maintenance_checked_window: Option<usize>,
    /// Throttle background NEXT-market discovery when the feed is missing.
    last_next_find_attempt_ms: i64,
}

/// Polymarket platform health, polled once per window start from the public
/// status API. When the platform is degraded/under maintenance we pause trading
/// for that window and surface it boldly on the dashboard.
#[derive(Clone, Debug)]
struct MaintenanceStatus {
    /// True when platform is fully operational (safe to trade).
    ok: bool,
    /// Whether we have a confirmed result from the status API yet.
    checked: bool,
    /// Short human label, e.g. "OK", "UNDER MAINTENANCE!", "INCIDENT!".
    label: String,
}

impl Default for MaintenanceStatus {
    fn default() -> Self {
        Self {
            ok: true,
            checked: false,
            label: "OK".to_string(),
        }
    }
}

impl MaintenanceStatus {
    /// Status check could not be completed (network error / bad payload). We do
    /// NOT block trading on our own connectivity hiccup, but flag it visibly.
    fn unknown() -> Self {
        Self {
            ok: true,
            checked: false,
            label: "CHECK FAILED".to_string(),
        }
    }

    /// Only block trading on a confirmed bad platform status.
    fn blocks_trading(&self) -> bool {
        self.checked && !self.ok
    }
}

/// One-shot poll of Polymarket's public status summary.
/// https://status.polymarket.com/public-api -> /v3/summary.json
async fn fetch_polymarket_maintenance() -> MaintenanceStatus {
    const URL: &str = "https://status.polymarket.com/v3/summary.json";
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build()
    {
        Ok(c) => c,
        Err(_) => return MaintenanceStatus::unknown(),
    };
    let text = match client.get(URL).send().await {
        Ok(resp) => match resp.text().await {
            Ok(t) => t,
            Err(_) => return MaintenanceStatus::unknown(),
        },
        Err(_) => return MaintenanceStatus::unknown(),
    };
    let v: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(_) => return MaintenanceStatus::unknown(),
    };

    let page_status = v
        .get("page")
        .and_then(|p| p.get("status"))
        .and_then(|s| s.as_str())
        .unwrap_or("");
    let incidents = v
        .get("activeIncidents")
        .and_then(|a| a.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    let maint_in_progress = v
        .get("activeMaintenances")
        .and_then(|a| a.as_array())
        .map(|a| {
            a.iter().any(|m| {
                m.get("status")
                    .and_then(|s| s.as_str())
                    .map(|s| s.eq_ignore_ascii_case("INPROGRESS"))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false);

    let status_up = page_status.eq_ignore_ascii_case("UP");
    let ok = status_up && incidents == 0 && !maint_in_progress;
    let label = if ok {
        "OK".to_string()
    } else if maint_in_progress {
        "UNDER MAINTENANCE!".to_string()
    } else if incidents > 0 {
        "INCIDENT!".to_string()
    } else {
        format!("DEGRADED ({page_status})!")
    };
    MaintenanceStatus {
        ok,
        checked: true,
        label,
    }
}

/// Bold, colored maintenance badge for the dashboard status line.
fn paint_maintenance(m: &MaintenanceStatus) -> String {
    let code = if !m.checked {
        "\x1b[1;38;5;179m" // bold yellow (unknown / not yet checked)
    } else if m.ok {
        "\x1b[1;38;5;114m" // bold green
    } else {
        "\x1b[1;38;5;174m" // bold red
    };
    format!("{code}Maintenance: {}\x1b[0m", m.label)
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
        println!("Examples:\n  cargo run -- BTC 5m\n  cargo run -- ETH 5m\n  cargo run -- SOL 5m\n  cargo run -- XRP 5m\n  cargo run -- DOGE 5m");
        return Ok(());
    }

    let asset = args
        .get(1)
        .cloned()
        .unwrap_or_else(|| "BTC".to_string())
        .to_uppercase();
    if !asset_price::is_supported(&asset) {
        eprintln!(
            "Unsupported asset: {}. Supported: BTC, ETH, SOL, XRP, DOGE",
            asset
        );
        std::process::exit(1);
    }
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

    let volatility_mgr = Arc::new(VolatilityManager::new(&asset));

    println!(
        "Инициализация ATR для {} ({})...",
        asset,
        volatility_mgr.symbol()
    );
    if let Err(e) = volatility_mgr.warmup_from_rest().await {
        println!("[ATR Warmup] Предупреждение: не удалось выполнить быстрый прогрев: {:?}. Начинаем стандартное накопление...", e);
    }

    // Запускаем фоновое отслеживание живых тиков (+ REST refresh on reconnect/stale)
    volatility_mgr.start_tracking();

    let cex_micro_mgr = CexMicroManager::new();
    cex_micro_mgr.start_tracking();

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
    let (llm_forecaster, llm_startup_log) = if config.llm.enabled {
        match LlmForecaster::new(
            "llm.json",
            config.llm.model.clone(),
            config.llm.location.clone(),
        ) {
            Ok(forecaster) => {
                match tokio::time::timeout(Duration::from_secs(8), forecaster.health_check()).await
                {
                    Ok(Ok(())) => {
                        let msg = format!(
                            "[LLM] OK | Vertex forecast enabled via llm.json | model: {} | location: {}",
                            config.llm.model, config.llm.location
                        );
                        println!("{}", msg);
                        (Some(Arc::new(forecaster)), msg)
                    }
                    Ok(Err(e)) => {
                        let msg = format!("[LLM] Disabled | startup check failed: {}", e);
                        println!("{}", msg);
                        (None, msg)
                    }
                    Err(_) => {
                        let msg = "[LLM] Disabled | startup check timed out".to_string();
                        println!("{}", msg);
                        (None, msg)
                    }
                }
            }
            Err(e) => {
                let msg = format!("[LLM] Disabled | {}", e);
                println!("{}", msg);
                (None, msg)
            }
        }
    } else {
        let msg = "[LLM] Disabled | config llm.enabled=false".to_string();
        println!("{}", msg);
        (None, msg)
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
        system_logs: vec![llm_startup_log],
        started_at: get_now_ms(),
        spot_price: None,
        volatility_mgr: Arc::clone(&volatility_mgr),
        shutdown_pending: false,
        run_log_dir,
        spot_series: SpotSeries::new(180, 12.0),
        llm_forecaster,
        llm_forecasts: HashMap::new(),
        llm_forecast_attempted: HashSet::new(),
        llm_forecast_scored: HashSet::new(),
        llm_correct: 0,
        llm_wrong: 0,
        mid_cross_tracker: MidCrossTracker::new(),
        cex_micro_mgr: cex_micro_mgr.clone(),
        trade_tape: TradeTapeTracker::new(),
        window_stats: WindowStatsAggregator::new(),
        ptb_locked_windows: HashSet::new(),
        maintenance: MaintenanceStatus::default(),
        maintenance_checked_window: None,
        last_next_find_attempt_ms: 0,
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
    if config.strategy == "j_endgame" {
        let fee_bps = config
            .j_endgame
            .fee_rate_bps
            .unwrap_or(j_fees::DEFAULT_CRYPTO_FEE_RATE_BPS);
        let c = j_fees::compare_endgame_clips(1.0, fee_bps);
        app_state.system_logs.push(format!(
            "[J] Fee model $1 clip ({:.0}bps): buy@98 net ${:.4} | buy@99 net ${:.4} | scalp 98/99 net ${:.4}",
            fee_bps, c.buy_98_net, c.buy_99_net, c.scalp_98_99_net
        ));
        app_state.system_logs.push(
            "[J] IMPULSE mid-window + CHEAP endgame + LATE final 50s ($8+$10+$5)".to_string(),
        );
    }

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
                app_state.window_stats.flush_to_csv(&app_state.run_log_dir);
                if app_state.config.strategy == "cheap_hold_h" {
                    let p = app_state.portfolio.lock().unwrap();
                    h_stats::log_h_window_close(
                        &app_state.run_log_dir,
                        0,
                        "SESSION_END",
                        p.overall_realized_pnl,
                        "",
                        "",
                        &h_stats::HCloseStats::default(),
                        p.h_market_wins,
                        p.h_market_losses,
                        p.h_salvage_escapes,
                        p.h_salvage_wins,
                    );
                }
                println!(
                    "  {}",
                    app_state
                        .window_stats
                        .session_summary_line(&app_state.config.strategy)
                );
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
        let warmup_window = app.config.strategy == "dynamic_grid_e"
            || app.config.strategy == "cheap_hold_h"
            || app.config.strategy == "j_endgame";
        let promoted = {
            let win_state = port.get_or_create_window_state(0, "CURRENT", &active);
            if warmup_window {
                win_state.status = "SKIPPED".to_string();
            } else {
                win_state.status = "LIVE".to_string(); // Live since startup
            }
            win_state.clone()
        };
        if warmup_window {
            port.skipped_windows += 1;
            app.system_logs.push(
                "[Strategy E] Window #0 is warmup only — trading starts at window #1".to_string(),
            );
        }
        app.current_window = Some(promoted);
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
                "Scheduled NEXT window retry (monitor will re-search)".to_string(),
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
        recent_context: build_llm_recent_context(app, 10),
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

fn run_j_endgame_live_tick(
    app: &mut AppState,
    window_number: usize,
    market: &MarketWindow,
    prices: &PricesState,
    timestamp: i64,
) {
    if app.config.strategy != "j_endgame" || window_number == 0 {
        return;
    }
    // Platform health gate: do not trade while Polymarket is confirmed
    // degraded / under maintenance for this window.
    if app.maintenance.blocks_trading() {
        return;
    }
    let Ok(end) = chrono::DateTime::parse_from_rfc3339(&market.end_time) else {
        return;
    };
    let secs_to_end = (end.timestamp_millis() - timestamp) / 1000;
    if secs_to_end <= 0 {
        return;
    }

    let mut port = app.portfolio.lock().unwrap();
    let win_state = port
        .get_or_create_window_state(window_number, "CURRENT", market)
        .clone();
    let current_atr = app.volatility_mgr.get_current_atr();
    let spot_signal = app.spot_series.snapshot();
    let mid_cross_snap = app.mid_cross_tracker.snapshot(window_number);
    let cex_micro_snap = app.cex_micro_mgr.snapshot(app.spot_price);
    let tape_snap = app.trade_tape.snapshot(
        window_number,
        timestamp,
        app.config.j_endgame.tape_window_ms,
    );
    let cash = port.available_cash;
    let mut strat = app.strategy.lock().unwrap();
    strat.set_runtime_cash(cash);
    let signals = strat.process_live_tick(
        &app.config,
        prices,
        app.spot_price,
        market,
        &win_state,
        secs_to_end,
        current_atr,
        spot_signal,
        &mid_cross_snap,
        &cex_micro_snap,
        &tape_snap,
    );
    drop(strat);

    execute_strategy_signals(
        &app.run_log_dir,
        &app.strategy,
        &mut port,
        window_number,
        market,
        prices,
        &win_state,
        signals,
        current_atr,
        app.spot_price,
        secs_to_end,
        spot_signal,
    );
}

fn execute_strategy_signals(
    log_dir: &str,
    strategy: &Arc<Mutex<StrategyEngine>>,
    port: &mut Portfolio,
    window_number: usize,
    market: &MarketWindow,
    prices: &PricesState,
    win_state: &WindowState,
    signals: Vec<OrderSignal>,
    current_atr: f64,
    spot_price: Option<f64>,
    secs_to_end: i64,
    spot_signal: SpotSignalSnapshot,
) {
    for sig in signals {
        let executed = if sig.is_buy {
            port.execute_buy(window_number, &sig.side, sig.amount, sig.price, &sig.reason)
                .is_some()
        } else {
            port.execute_sell(window_number, &sig.side, sig.amount, sig.price, &sig.reason)
                .is_some()
        };
        append_signal_event(
            log_dir,
            window_number,
            &market.slug,
            &sig,
            executed,
            current_atr,
            spot_price,
            market,
            prices,
            win_state,
            secs_to_end,
            spot_signal,
        );
        if executed {
            strategy
                .lock()
                .unwrap()
                .notify_order_executed(window_number, &sig);
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

fn build_llm_recent_context(app: &AppState, limit: usize) -> LlmRecentWindowContext {
    let port = app.portfolio.lock().unwrap();
    let mut closed = port
        .windows
        .values()
        .filter(|win| win.spent > 0.0 && win.status.starts_with("CLOSED"))
        .collect::<Vec<_>>();
    closed.sort_by_key(|win| win.window_number);
    let start = closed.len().saturating_sub(limit);
    let recent = &closed[start..];

    let mut rows = Vec::with_capacity(recent.len());
    let mut pnls = Vec::with_capacity(recent.len());
    let mut entry_hits = 0usize;
    let mut entry_known = 0usize;
    let mut llm_hits = 0usize;
    let mut llm_known = 0usize;
    let mut runner_redeems = 0usize;
    let mut up_winners = 0usize;
    let mut down_winners = 0usize;
    let mut hedge_cost_total = 0.0;
    let mut hedge_window_pnl = 0.0;
    let mut tail_liquidation_value = 0.0;
    let mut buy_shares = 0.0;
    let mut sell_shares = 0.0;

    for win in recent {
        let pnl = win.cash_returned - win.spent;
        pnls.push(pnl);

        let entry_side = first_buy(win)
            .map(|trade| trade.side.clone())
            .unwrap_or_default();
        let entry_source = first_buy(win)
            .map(|trade| extract_entry_source(&trade.reason))
            .unwrap_or_else(|| "none".to_string());
        let winner = redeemed_winner(win).unwrap_or_default();
        if winner == "UP" {
            up_winners += 1;
        } else if winner == "DOWN" {
            down_winners += 1;
        }

        if !entry_side.is_empty() && !winner.is_empty() {
            entry_known += 1;
            if entry_side == winner {
                entry_hits += 1;
            }
        }

        let llm_side = app
            .llm_forecasts
            .get(&win.window_number)
            .map(|forecast| forecast.side.clone())
            .unwrap_or_default();
        if !llm_side.is_empty() && !winner.is_empty() {
            llm_known += 1;
            if llm_side == winner {
                llm_hits += 1;
            }
        }

        let runner_redeemed = !entry_side.is_empty()
            && win.trades.iter().any(|trade| {
                trade.trade_type == "REDEEM" && trade.side == entry_side && trade.shares > 0.0
            });
        if runner_redeemed {
            runner_redeems += 1;
        }

        let hedge_cost = opposite_buy_cost_after_first(win, &entry_side);
        if hedge_cost > 0.0 {
            hedge_window_pnl += pnl;
        }
        hedge_cost_total += hedge_cost;

        let tail_value = tail_liquidation_usd(win);
        tail_liquidation_value += tail_value;

        buy_shares += win
            .trades
            .iter()
            .filter(|trade| trade.trade_type == "BUY")
            .map(|trade| trade.shares)
            .sum::<f64>();
        sell_shares += win
            .trades
            .iter()
            .filter(|trade| trade.trade_type == "SELL")
            .map(|trade| trade.shares)
            .sum::<f64>();

        rows.push(LlmRecentWindowRow {
            window_id: win.window_number,
            entry_side,
            llm_side,
            winner,
            pnl,
            entry_source,
            runner_redeemed,
            hedge_cost,
            tail_value,
        });
    }

    let total_pnl = pnls.iter().sum::<f64>();
    let sample_size = pnls.len();
    let adverse_slippage_01_pnl = total_pnl - 0.01 * (buy_shares + sell_shares);
    let adverse_slippage_02_pnl = total_pnl - 0.02 * (buy_shares + sell_shares);

    LlmRecentWindowContext {
        sample_size,
        avg_pnl_per_window: if sample_size > 0 {
            total_pnl / sample_size as f64
        } else {
            0.0
        },
        median_pnl: median_f64(pnls.clone()),
        max_drawdown: max_drawdown(&pnls),
        entry_side_accuracy: ratio(entry_hits, entry_known),
        llm_accuracy: ratio(llm_hits, llm_known),
        runner_redeem_rate: ratio(runner_redeems, sample_size),
        hedge_cost: hedge_cost_total,
        hedge_window_pnl,
        tail_liquidation_value,
        adverse_slippage_01_pnl,
        adverse_slippage_02_pnl,
        up_winners,
        down_winners,
        rows,
    }
}

fn first_buy(win: &WindowState) -> Option<&TradeRecord> {
    win.trades.iter().find(|trade| trade.trade_type == "BUY")
}

fn redeemed_winner(win: &WindowState) -> Option<String> {
    win.trades
        .iter()
        .find(|trade| trade.trade_type == "REDEEM" && trade.shares > 0.0)
        .map(|trade| trade.side.clone())
}

fn opposite_buy_cost_after_first(win: &WindowState, first_side: &str) -> f64 {
    let mut seen_first_buy = false;
    win.trades
        .iter()
        .filter(|trade| {
            if trade.trade_type != "BUY" {
                return false;
            }
            if !seen_first_buy {
                seen_first_buy = true;
                return false;
            }
            !first_side.is_empty() && trade.side != first_side
        })
        .map(|trade| trade.usd_value)
        .sum()
}

fn tail_liquidation_usd(win: &WindowState) -> f64 {
    win.trades
        .iter()
        .filter(|trade| {
            trade.trade_type == "SELL"
                && (trade.reason.contains("weak_salvage")
                    || trade.reason.contains("emergency")
                    || trade.reason.contains("tail")
                    || trade.reason.contains("otm_surplus"))
        })
        .map(|trade| trade.usd_value)
        .sum()
}

fn extract_entry_source(reason: &str) -> String {
    let marker = "_source_";
    let Some(start) = reason.find(marker).map(|idx| idx + marker.len()) else {
        return "unknown".to_string();
    };
    let rest = &reason[start..];
    let end = rest.find("_conf_").unwrap_or(rest.len());
    rest[..end].to_string()
}

fn ratio(num: usize, den: usize) -> f64 {
    if den > 0 {
        num as f64 / den as f64
    } else {
        0.0
    }
}

fn median_f64(mut values: Vec<f64>) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mid = values.len() / 2;
    if values.len() % 2 == 0 {
        (values[mid - 1] + values[mid]) / 2.0
    } else {
        values[mid]
    }
}

fn max_drawdown(pnls: &[f64]) -> f64 {
    let mut cumulative = 0.0;
    let mut peak = 0.0;
    let mut max_dd = 0.0;
    for pnl in pnls {
        cumulative += pnl;
        if cumulative > peak {
            peak = cumulative;
        }
        let dd = peak - cumulative;
        if dd > max_dd {
            max_dd = dd;
        }
    }
    max_dd
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
                    if current_atr < app.config.min_atr_for(&app.asset) {
                        let log_msg = format!(
                            "[STRATEGY] Skipping Window #{}: Volatility too low (ATR: ${:.4} < Min: ${:.4})",
                            next.window_number,
                            current_atr,
                            app.config.min_atr_for(&app.asset)
                        );
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
                        let cex_micro_snap = app.cex_micro_mgr.snapshot(app.spot_price);
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
                            &cex_micro_snap,
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
                    let updated = close_window_tracked(app, &current, "CLOSED_TIME", secs_to_end);
                    app.current_window = Some(updated);
                }
            }
        }
    }

    // Keep hunting for NEXT when the feed dropped or discovery failed earlier.
    if app.next_window.is_none() {
        let now = get_now_ms();
        if now.saturating_sub(app.last_next_find_attempt_ms) >= 10_000 {
            app.last_next_find_attempt_ms = now;
            find_and_subscribe_next(app, event_tx).await;
        }
    }
}

/**
 * Lock PTB from Chainlink at window open. Replaces implausible values parsed from question text.
 */
fn evaluate_ptb_capture(
    already_locked: bool,
    market: &MarketWindow,
    spot: f64,
    timestamp_ms: i64,
) -> Option<(f64, String)> {
    if spot <= 0.0 || already_locked {
        return None;
    }
    let start_ms = chrono::DateTime::parse_from_rfc3339(&market.start_time)
        .ok()?
        .timestamp_millis();
    if timestamp_ms < start_ms {
        return None;
    }
    let should_capture = match market.price_to_beat {
        None => true,
        Some(ptb) if ptb_implausible(&market.asset, ptb, spot) => true,
        Some(_) => false,
    };
    if !should_capture {
        return None;
    }
    let px_str = format_asset_price(&market.asset, spot);
    let msg = if market.price_to_beat.is_some() {
        format!(
            "[CAPTURE PTB] Replaced implausible PTB with Chainlink open: {}",
            px_str
        )
    } else {
        format!(
            "[CAPTURE PTB] Captured exact open price from Chainlink WS: {}",
            px_str
        )
    };
    Some((spot, msg))
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
            let secs_to_end = chrono::DateTime::parse_from_rfc3339(&curr.market.end_time)
                .map(|end| (end.timestamp_millis() - get_now_ms()) / 1000)
                .unwrap_or(0);
            let _updated = close_window_tracked(app, &curr, "CLOSED_TIME", secs_to_end);
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

    // j_endgame trades in-window (endgame), not via pre-start entry — promote as LIVE.
    let is_entered = next_win.status == "ENTERED_PRE_START" || app.config.strategy == "j_endgame";
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

    // Window may already be live when promoted — capture PTB immediately if spot is available.
    if let Some(spot) = app.spot_price {
        if let Some(curr) = app.current_window.as_ref() {
            let wn = curr.window_number;
            let locked = app.ptb_locked_windows.contains(&wn);
            if let Some((ptb, msg)) = evaluate_ptb_capture(locked, &curr.market, spot, get_now_ms())
            {
                app.system_logs.push(msg);
                app.ptb_locked_windows.insert(wn);
                if let Some(curr) = app.current_window.as_mut() {
                    curr.market.price_to_beat = Some(ptb);
                }
                if let Some(win) = port.windows.get_mut(&wn) {
                    win.market.price_to_beat = Some(ptb);
                }
            }
        }
    }

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

            if let Some(curr) = app.current_window.as_ref() {
                let wn = curr.window_number;
                let locked = app.ptb_locked_windows.contains(&wn);
                if let Some((ptb, msg)) =
                    evaluate_ptb_capture(locked, &curr.market, price, timestamp_ms)
                {
                    app.system_logs.push(msg);
                    app.ptb_locked_windows.insert(wn);
                    if let Some(curr) = app.current_window.as_mut() {
                        curr.market.price_to_beat = Some(ptb);
                    }
                    let mut port = app.portfolio.lock().unwrap();
                    if let Some(win) = port.windows.get_mut(&wn) {
                        win.market.price_to_beat = Some(ptb);
                    }
                }
            }

            // Once per window start: poll Polymarket platform status. If the
            // platform is degraded/under maintenance, trading is paused for the
            // window (enforced in run_j_endgame_live_tick) and shown on the dash.
            if let Some(curr) = app.current_window.as_ref() {
                let wn = curr.window_number;
                if wn != 0 && app.maintenance_checked_window != Some(wn) {
                    app.maintenance_checked_window = Some(wn);
                    let status = fetch_polymarket_maintenance().await;
                    if status.blocks_trading() {
                        app.system_logs.push(format!(
                            "[STATUS] Polymarket {} — trading paused for window #{}",
                            status.label, wn
                        ));
                    } else if !status.checked {
                        app.system_logs.push(format!(
                            "[STATUS] Polymarket status check failed for window #{} — trading allowed",
                            wn
                        ));
                    }
                    app.maintenance = status;
                }
            }

            if let Some(curr) = app.current_window.clone() {
                run_j_endgame_live_tick(
                    app,
                    curr.window_number,
                    &curr.market,
                    &curr.prices,
                    timestamp_ms,
                );
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

            if role == "CURRENT" {
                if let Ok(end) = chrono::DateTime::parse_from_rfc3339(&market.end_time) {
                    let secs_to_end = (end.timestamp_millis() - timestamp) / 1000;
                    let current_atr = app.volatility_mgr.get_current_atr();
                    let spot_signal = app.spot_series.snapshot();

                    if let Some(mid_event) = app.mid_cross_tracker.observe_tick(
                        window_number,
                        &market,
                        &prices,
                        secs_to_end,
                        current_atr,
                        app.spot_price,
                        spot_signal,
                        timestamp,
                    ) {
                        append_mid_cross_event(
                            &app.run_log_dir,
                            window_number,
                            &market.slug,
                            &mid_event,
                            app.spot_price,
                            &win_state.market,
                            spot_signal,
                        );
                        if mid_event.event == "mid_cross" {
                            let sig_label = if mid_event.is_significant {
                                "sig=yes"
                            } else {
                                "sig=no"
                            };
                            app.system_logs.push(format!(
                                "[MID CROSS] #{} {}→{} {} ATR={:.1} @{:.1}%",
                                mid_event.cross_count,
                                mid_event.from_side.map(|s| s.as_str()).unwrap_or("?"),
                                mid_event.to_side.as_str(),
                                sig_label,
                                mid_event.current_atr,
                                mid_event.time_pct
                            ));
                            if app.system_logs.len() > 30 {
                                app.system_logs.remove(0);
                            }
                        }
                    }

                    let mid_cross_snap = app.mid_cross_tracker.snapshot(window_number);
                    let cex_micro_snap = app.cex_micro_mgr.snapshot(app.spot_price);
                    let tape_snap = app.trade_tape.snapshot(
                        window_number,
                        timestamp,
                        app.config.j_endgame.tape_window_ms,
                    );

                    let cash = port.available_cash;
                    let signals = {
                        let mut strat = app.strategy.lock().unwrap();
                        strat.set_runtime_cash(cash);
                        strat.process_live_tick(
                            &app.config,
                            &prices,
                            app.spot_price,
                            &win_state.market,
                            &win_state,
                            secs_to_end,
                            current_atr,
                            spot_signal,
                            &mid_cross_snap,
                            &cex_micro_snap,
                            &tape_snap,
                        )
                    };

                    if !((app.config.strategy == "dynamic_grid_e"
                        || app.config.strategy == "cheap_hold_h"
                        || app.config.strategy == "j_endgame")
                        && window_number == 0)
                    {
                        execute_strategy_signals(
                            &app.run_log_dir,
                            &app.strategy,
                            &mut port,
                            window_number,
                            &win_state.market,
                            &prices,
                            &win_state,
                            signals,
                            current_atr,
                            app.spot_price,
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
        MarketEvent::TradePrint {
            window_number,
            role,
            side,
            usd,
            is_buy,
            timestamp,
            ..
        } => {
            if !is_buy {
                return;
            }
            app.trade_tape.record_buy(
                window_number,
                &side,
                usd,
                timestamp,
                app.config.j_endgame.tape_window_ms,
            );
            if role != "CURRENT" || app.config.strategy != "j_endgame" || window_number == 0 {
                return;
            }
            let Some(curr) = app.current_window.clone() else {
                return;
            };
            let market = curr.market.clone();
            let prices = curr.prices.clone();
            let mut port = app.portfolio.lock().unwrap();
            let win_state = port
                .get_or_create_window_state(window_number, "CURRENT", &market)
                .clone();

            if let Ok(end) = chrono::DateTime::parse_from_rfc3339(&market.end_time) {
                let secs_to_end = (end.timestamp_millis() - timestamp) / 1000;
                let current_atr = app.volatility_mgr.get_current_atr();
                let spot_signal = app.spot_series.snapshot();
                let mid_cross_snap = app.mid_cross_tracker.snapshot(window_number);
                let cex_micro_snap = app.cex_micro_mgr.snapshot(app.spot_price);
                let tape_snap = app.trade_tape.snapshot(
                    window_number,
                    timestamp,
                    app.config.j_endgame.tape_window_ms,
                );
                let cash = port.available_cash;
                let signals = {
                    let mut strat = app.strategy.lock().unwrap();
                    strat.set_runtime_cash(cash);
                    strat.process_live_tick(
                        &app.config,
                        &prices,
                        app.spot_price,
                        &market,
                        &win_state,
                        secs_to_end,
                        current_atr,
                        spot_signal,
                        &mid_cross_snap,
                        &cex_micro_snap,
                        &tape_snap,
                    )
                };
                execute_strategy_signals(
                    &app.run_log_dir,
                    &app.strategy,
                    &mut port,
                    window_number,
                    &market,
                    &prices,
                    &win_state,
                    signals,
                    current_atr,
                    app.spot_price,
                    secs_to_end,
                    spot_signal,
                );
                let updated = port.get_or_create_window_state(window_number, "CURRENT", &market);
                app.current_window = Some(updated.clone());
            }
        }
    }
}

/**
 * Merge the live AppState window with portfolio (prices, PTB, status).
 * After close the portfolio role is PAST but AppState still holds the window.
 */
fn resolve_display_window(
    port: &Portfolio,
    live: Option<&WindowState>,
    role: &str,
) -> Option<WindowState> {
    let mut chosen = live
        .cloned()
        .or_else(|| port.windows.values().find(|w| w.role == role).cloned())?;
    if let Some(pw) = port.windows.get(&chosen.window_number) {
        chosen.prices = pw.prices.clone();
        chosen.market = pw.market.clone();
        chosen.status = pw.status.clone();
        chosen.spent = pw.spent;
        chosen.up_shares = pw.up_shares;
        chosen.down_shares = pw.down_shares;
    }
    Some(chosen)
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
        "  LLM-forecast: {} | Model: {} | Location: {} | Right {} | Wrong {} | Acc {:.1}%",
        paint(
            if llm_enabled { "enabled" } else { "disabled" },
            if llm_enabled { "green" } else { "dim" }
        ),
        paint(&app.config.llm.model, "cyan"),
        paint(&app.config.llm.location, "cyan"),
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
        format_atr(&app.asset, atr)
    } else {
        "Warming up...".to_string()
    };

    println!(
        "  Started: {} | Runtime: {} | {} ATR(1m): {}",
        paint(&format_utc(app.started_at), "cyan"),
        paint(&runtime, "bold"),
        paint(&app.asset, "cyan"),
        paint(&atr_str, "yellow")
    );

    let spot_header = match app.spot_price {
        Some(px) if px > 0.0 => paint(
            &format!("Chainlink Spot: {}", format_asset_price(&app.asset, px)),
            "cyan",
        ),
        _ => paint("Chainlink Spot: NO DATA (WS reconnecting...)", "red"),
    };
    println!("  {}", spot_header);

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
    if app.config.strategy == "cheap_hold_h" {
        println!(
            "  {}",
            paint(
                &h_stats::format_h_session_line(
                    p.h_market_wins,
                    p.h_market_losses,
                    p.h_salvage_escapes,
                    p.h_salvage_wins,
                ),
                "cyan",
            )
        );
    }

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

    // Live window state lives in AppState; portfolio `role` becomes PAST after
    // close — reading only port.windows by role leaves the dashboard blank.
    let (current_window, next_window) = {
        let port = app.portfolio.lock().unwrap();
        (
            resolve_display_window(&port, app.current_window.as_ref(), "CURRENT"),
            resolve_display_window(&port, app.next_window.as_ref(), "NEXT"),
        )
    };

    let current_llm = current_window
        .as_ref()
        .and_then(|win| app.llm_forecasts.get(&win.window_number));
    let next_llm = next_window
        .as_ref()
        .and_then(|win| app.llm_forecasts.get(&win.window_number));

    let current_mid_cross = current_window
        .as_ref()
        .map(|win| app.mid_cross_tracker.snapshot(win.window_number));
    let cex_micro_snap = app.cex_micro_mgr.snapshot(app.spot_price);
    let now_ms = get_now_ms();
    let current_tape = current_window.as_ref().map(|win| {
        app.trade_tape.snapshot(
            win.window_number,
            now_ms,
            app.config.j_endgame.tape_window_ms,
        )
    });
    let left_lines = render_window_block(
        &current_window,
        "CURRENT",
        app.spot_price,
        app.volatility_mgr.get_current_atr(),
        app.strategy.clone(),
        current_llm,
        current_mid_cross.as_ref(),
        Some(&cex_micro_snap),
        &app.config,
        current_tape.as_ref(),
        p.available_cash,
        &app.maintenance,
    );
    println!(
        "  {}",
        app.window_stats.session_summary_line(&app.config.strategy)
    );

    let right_lines = render_window_block(
        &next_window,
        "NEXT",
        app.spot_price,
        app.volatility_mgr.get_current_atr(),
        app.strategy.clone(),
        next_llm,
        None,
        None,
        &app.config,
        None,
        p.available_cash,
        &app.maintenance,
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
    current_atr: f64,
    strategy: Arc<Mutex<StrategyEngine>>,
    llm_forecast: Option<&LlmForecast>,
    mid_cross: Option<&MidCrossSnapshot>,
    cex_micro: Option<&CexMicroSnapshot>,
    config: &config::Config,
    tape: Option<&TradeTapeSnapshot>,
    available_cash: f64,
    maintenance: &MaintenanceStatus,
) -> Vec<String> {
    let strategy_name = config.strategy.as_str();
    let j_endgame = &config.j_endgame;
    let mut lines = vec![];

    let label_colored = if label == "CURRENT" {
        paint(label, "green")
    } else {
        paint(label, "yellow")
    };

    let Some(win) = win_opt else {
        lines.push(format!("--- {} WINDOW ---", label_colored));
        lines.push(paint("Waiting for market stream...", "dim"));
        lines.push(paint(
            "  (searching Polymarket — check SYSTEM EVENT LOG below)",
            "dim",
        ));
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
        .map(|dt| dt.timestamp_millis())
        .unwrap_or(now);
    let end_dt = chrono::DateTime::parse_from_rfc3339(&m.end_time)
        .map(|dt| dt.timestamp_millis())
        .unwrap_or(now);

    if now < start_dt {
        let secs = (start_dt - now) / 1000;
        lines.push(format!(
            "Status: {} | Starts In: {} | {}",
            paint("WAITING", "yellow"),
            paint(&format_countdown(secs), "bold"),
            paint_maintenance(maintenance)
        ));
    } else if now < end_dt {
        let secs = (end_dt - now) / 1000;
        lines.push(format!(
            "Status: {} | Time Left: {} | {}",
            paint("LIVE", "green"),
            paint(&format_countdown(secs), "bold"),
            paint_maintenance(maintenance)
        ));
    } else {
        lines.push(format!("Status: {}", paint("EXPIRED", "red")));
    }

    let strike_str = m
        .price_to_beat
        .map(|p| format_asset_price(&m.asset, p))
        .unwrap_or_else(|| "N/A (Chainlink @ open)".to_string());
    lines.push(format!(
        "Price to Beat (Strike): {}",
        paint(&strike_str, "magenta")
    ));

    let spot_str = spot_price
        .map(|p| format_asset_price(&m.asset, p))
        .unwrap_or_else(|| "N/A".to_string());
    let distance_str = match m.get_ptb_deviation(spot_price) {
        Some((delta, pct)) => {
            let (tone, formatted) = if delta >= 0.0 {
                (
                    "green",
                    format!("+{} (+{:.4}%)", format_asset_price(&m.asset, delta), pct),
                )
            } else {
                (
                    "red",
                    format!(
                        "-{} ({:.4}%)",
                        format_asset_price(&m.asset, delta.abs()),
                        pct
                    ),
                )
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
    if UP.ask <= 0.0 && DN.ask <= 0.0 && UP.bid <= 0.0 && DN.bid <= 0.0 {
        lines.push(paint(
            "⚠ CLOB prices: NO DATA — waiting for orderbook WS / REST snapshot",
            "yellow",
        ));
    }
    if label == "CURRENT" {
        if let Some(mc) = mid_cross {
            if mc.armed {
                let leader = mc.current_side.map(|s| s.as_str()).unwrap_or("TIE");
                let leader_tone = match leader {
                    "UP" => "green",
                    "DOWN" => "red",
                    _ => "yellow",
                };
                lines.push(format!(
                    "Mid Lead: {} | gap {:.2} | crosses {} (sig {}) | armed @8%",
                    paint(leader, leader_tone),
                    mc.lead_gap,
                    mc.cross_count,
                    mc.significant_cross_count
                ));
                if let (Some(from), Some(to), Some(tpct)) =
                    (mc.last_cross_from, mc.last_cross_to, mc.last_cross_time_pct)
                {
                    let sig = if mc.last_cross_is_significant {
                        paint("sig", "green")
                    } else {
                        paint("noise", "dim")
                    };
                    lines.push(format!(
                        "Last Mid Cross: {}→{} @ {:.1}% | {} | ATR {:.1}",
                        paint(from.as_str(), "yellow"),
                        paint(to.as_str(), leader_tone),
                        tpct,
                        sig,
                        mc.last_cross_atr
                    ));
                }
            } else {
                lines.push(format!(
                    "Mid Lead: {}",
                    paint("waiting for 8% window", "dim")
                ));
            }
        }

        if let (Some(spot), Some(ptb)) = (spot_price, m.price_to_beat) {
            let now = get_now_ms();
            let (secs_to_end, time_pct) = match chrono::DateTime::parse_from_rfc3339(&m.end_time) {
                Ok(end) => {
                    let secs = (end.timestamp_millis() - now) / 1000;
                    let duration_sec = window_duration_sec(m);
                    let elapsed = (duration_sec - secs as f64).clamp(0.0, duration_sec);
                    let pct = if duration_sec > 0.0 {
                        (elapsed / duration_sec) * 100.0
                    } else {
                        0.0
                    };
                    (secs, pct)
                }
                Err(_) => (0, 0.0),
            };
            let ptb_crossed_terminal = strategy
                .lock()
                .unwrap()
                .get_strategy_state(win.window_number)
                .map(|s| s.ptb_crossed)
                .unwrap_or(false);
            let mut hold_parts = Vec::new();
            for (side, bid) in [("UP", UP.bid), ("DOWN", DN.bid)] {
                let shares = if side == "UP" {
                    win.up_shares
                } else {
                    win.down_shares
                };
                if shares <= 0.0 || !side_is_itm(side, spot, ptb) {
                    continue;
                }
                let gap_z = itm_gap_z(side, spot, ptb, current_atr, secs_to_end);
                let fair_prob = if side == "UP" {
                    0.5 + (gap_z * 0.08).min(0.45)
                } else {
                    0.5 - (gap_z * 0.08).min(0.45)
                };
                let cex_against = cex_micro
                    .map(|c| cex_micro::cex_velocity_against_side(side, c))
                    .unwrap_or(false);
                let decision = evaluate_redeem_hold(&RedeemHoldInput {
                    side,
                    spot,
                    ptb,
                    secs_to_end,
                    time_pct,
                    current_atr,
                    bid,
                    fair_prob: fair_prob.clamp(0.05, 0.95),
                    ptb_crossed: ptb_crossed_terminal,
                    counter_velocity_against: false,
                    cex_velocity_against: cex_against,
                });
                if decision.should_hold {
                    hold_parts.push(format!("{} ITM z={:.2} ({})", side, gap_z, decision.reason));
                }
            }
            if hold_parts.is_empty() {
                lines.push(format!("Redeem Hold: {}", paint("inactive", "dim")));
            } else {
                lines.push(format!(
                    "Redeem Hold: {}",
                    paint(&hold_parts.join(" | "), "green")
                ));
            }
        }

        if let Some(cex) = cex_micro {
            let vel3 = cex
                .trade_velocity_3s
                .map(|v| format!("{:.0}", v))
                .unwrap_or_else(|| "n/a".to_string());
            let lead = cex
                .lead_vs_chainlink_bps
                .map(|b| format!("{:+.1}bps", b))
                .unwrap_or_else(|| "n/a".to_string());
            lines.push(format!(
                "CEX Micro: v3s {} USD/s | imb {:.2} | lead {}",
                vel3, cex.buy_sell_imbalance_3s, lead
            ));
        }
    }
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
    if strategy_name == "dynamic_grid_e" {
        if win.window_number == 0 {
            lines.push(paint(
                "E: window #0 warmup — no trading until window #1",
                "dim",
            ));
        } else if let Some(strat) = strat_engine.get_strategy_state(win.window_number) {
            let conviction = strat.e_conviction_side.as_deref().unwrap_or("N/A");
            let conviction_tone = match conviction {
                "UP" => "green",
                "DOWN" => "red",
                _ => "dim",
            };
            lines.push(format!(
                "E Conviction: {} | Tranches: {}/3 | Grid sells: {} (3 base + extend)",
                paint(conviction, conviction_tone),
                strat.e_tranches_done,
                strat.e_grid_steps_done,
            ));
            let baseline = strat.ptb_baseline.as_deref().unwrap_or("N/A");
            let crossed = if strat.ptb_crossed {
                paint("YES", "green")
            } else {
                paint("NO", "yellow")
            };
            lines.push(format!(
                "PTB baseline: {} | PTB crossed: {}",
                paint(baseline, "cyan"),
                crossed
            ));
            if label == "CURRENT" {
                if let Some(mc) = mid_cross {
                    if mc.armed {
                        let now = get_now_ms();
                        let time_pct = match chrono::DateTime::parse_from_rfc3339(&m.end_time) {
                            Ok(end) => {
                                let secs = (end.timestamp_millis() - now) / 1000;
                                let duration_sec = window_duration_sec(m);
                                let elapsed = (duration_sec - secs as f64).clamp(0.0, duration_sec);
                                if duration_sec > 0.0 {
                                    (elapsed / duration_sec) * 100.0
                                } else {
                                    0.0
                                }
                            }
                            Err(_) => 0.0,
                        };
                        let up_ask = win.prices.up.ask;
                        let dn_ask = win.prices.down.ask;
                        let cross_window = mc
                            .last_cross_time_pct
                            .map(|cp| time_pct >= cp && time_pct <= cp + 10.0)
                            .unwrap_or(false);
                        let entry_gate = if mc.cross_count >= 5 && time_pct < 40.0 {
                            paint(&format!("BLOCKED chop ({} crosses)", mc.cross_count), "red")
                        } else if cross_window
                            && mc.lead_gap >= 0.14
                            && ((mc.current_side == Some(LeadSide::Up) && up_ask <= 0.58)
                                || (mc.current_side == Some(LeadSide::Down) && dn_ask <= 0.58))
                        {
                            paint(
                                &format!(
                                    "READY cross lead {:.2} (UP {:.2} / DN {:.2})",
                                    mc.lead_gap, up_ask, dn_ask
                                ),
                                "green",
                            )
                        } else if up_ask <= 0.50 || dn_ask <= 0.50 {
                            paint(
                                &format!("READY value (UP {:.2} / DN {:.2})", up_ask, dn_ask),
                                "green",
                            )
                        } else {
                            paint(
                                &format!("WAIT no entry (UP {:.2} / DN {:.2})", up_ask, dn_ask),
                                "yellow",
                            )
                        };
                        lines.push(format!("E Entry gate: {}", entry_gate));
                    }
                }
            }
        } else if label == "NEXT" {
            lines.push(paint("E: monitoring NEXT (live-only, no pre-entry)", "dim"));
        } else {
            lines.push(paint("E: live-only — waiting for conviction entry", "dim"));
        }
    } else if strategy_name == "j_endgame" {
        if win.window_number == 0 {
            lines.push(paint(
                "J: window #0 warmup — no trading until window #1",
                "dim",
            ));
        } else if let Some(strat) = strat_engine.get_strategy_state(win.window_number) {
            let clips = strat.e_tranches_done;
            lines.push(format!(
                "J endgame clips: {}/{} | entry: {}",
                clips,
                if j_endgame.max_clips_per_window == 0 {
                    "∞".to_string()
                } else {
                    j_endgame.max_clips_per_window.to_string()
                },
                if strat.h_entry_done {
                    paint("active/done", "green")
                } else {
                    paint("waiting", "yellow")
                },
            ));
            if label == "CURRENT" {
                if let (Some(spot), Some(ptb)) = (spot_price, m.price_to_beat) {
                    let secs_to_end = match chrono::DateTime::parse_from_rfc3339(&m.end_time) {
                        Ok(end) => ((end.timestamp_millis() - now) / 1000).max(0),
                        Err(_) => 600,
                    };
                    let winner = if spot > ptb {
                        "UP"
                    } else if spot < ptb {
                        "DOWN"
                    } else {
                        "TIE"
                    };
                    let winner_ask = if winner == "UP" {
                        win.prices.up.ask
                    } else if winner == "DOWN" {
                        win.prices.down.ask
                    } else {
                        0.0
                    };
                    let expected = redeem_hold::expected_move_usd(
                        current_atr.max(REDEEM_HOLD_MIN_VALID_ATR),
                        secs_to_end.max(1),
                    );
                    let gz = if expected > 0.0 {
                        (spot - ptb) / expected
                    } else {
                        0.0
                    };
                    let elapsed = strategy::strategy_j::window_elapsed_pct(m, secs_to_end);
                    let phase = j_controller::detect_phase(
                        elapsed,
                        secs_to_end,
                        j_endgame,
                        mid_cross.unwrap_or(&MidCrossSnapshot::default()),
                    );
                    let fee_bps = j_endgame
                        .fee_rate_bps
                        .unwrap_or(j_fees::DEFAULT_CRYPTO_FEE_RATE_BPS);
                    let proj_pnl = j_controller::projected_redeem_pnl(win, winner, fee_bps);
                    lines.push(format!(
                        "J phase: {} | target +${:.2} | redeem PnL proj {:+.2}",
                        paint(phase.label(), "cyan"),
                        j_endgame.target_profit_usd,
                        proj_pnl,
                    ));
                    let plan = mid_cross
                        .map(|mc| {
                            let chop_blocked =
                                strat_engine.j_directional_blocked(win.window_number);
                            let allow = strategy::strategy_j::directional_entry_allowed_external(
                                j_endgame,
                                chop_blocked,
                                0.0,
                                current_atr.max(1.0),
                                spot,
                                ptb,
                            );
                            let confidence = if winner == "TIE" {
                                0.0
                            } else {
                                j_controller::endgame_confidence(
                                    j_endgame,
                                    winner,
                                    gz,
                                    &SpotSignalSnapshot::default(),
                                    mc,
                                    cex_micro.unwrap_or(&CexMicroSnapshot::default()),
                                    tape.unwrap_or(&TradeTapeSnapshot::default()),
                                )
                            };
                            j_controller::plan_j_window(
                                config,
                                &strategy::strategy_j::JWindowState::default(),
                                win,
                                &win.prices,
                                spot,
                                ptb,
                                secs_to_end,
                                elapsed,
                                current_atr.max(1.0),
                                0.0,
                                mc,
                                allow,
                                confidence,
                                available_cash,
                            )
                        })
                        .flatten();
                    let flip_armed = mid_cross
                        .map(|mc| {
                            strategy::strategy_j::flip_hedge_armed_display(
                                j_endgame,
                                strat.h_entry_side.as_deref(),
                                winner,
                                spot,
                                ptb,
                                gz,
                                mc,
                            )
                        })
                        .unwrap_or(false);
                    lines.push(format!(
                        "J winner {} ask {:.2} | {}s left | gap_z {:+.2}",
                        winner, winner_ask, secs_to_end, gz
                    ));
                    lines.push(format!(
                        "J tier: {}{}",
                        if flip_armed {
                            paint("FLIP HEDGE armed | ", "red")
                        } else {
                            String::new()
                        },
                        match plan {
                            Some(p) if p.tier == strategy::strategy_j::EndgameTier::Insurance => {
                                paint(
                                    &format!(
                                        "INSURANCE ≤{:.0}¢ | ${:.0} clip",
                                        j_endgame.insurance_max_ask * 100.0,
                                        j_endgame.insurance_clip_usd
                                    ),
                                    "cyan",
                                )
                            }
                            Some(p) if p.tier == strategy::strategy_j::EndgameTier::Rescue => {
                                paint("RESCUE solve → +$target", "red")
                            }
                            Some(p) if p.tier == strategy::strategy_j::EndgameTier::FlipHedge => {
                                paint("FLIP HEDGE", "red")
                            }
                            Some(p) if p.tier == strategy::strategy_j::EndgameTier::Impulse => {
                                paint(
                                    &format!(
                                        "IMPULSE ≤{:.0}¢ + tape",
                                        j_endgame.impulse_max_ask * 100.0
                                    ),
                                    "green",
                                )
                            }
                            Some(p) if p.tier == strategy::strategy_j::EndgameTier::Cheap => {
                                paint(
                                    &format!(
                                        "VALUE ≤{:.0}¢ gap≥{:.1} | 2nd half max {} clips",
                                        j_endgame.cheap_max_ask * 100.0,
                                        j_endgame.cheap_min_gap_z,
                                        j_endgame.cheap_max_clips
                                    ),
                                    "green",
                                )
                            }
                            Some(p) if p.tier == strategy::strategy_j::EndgameTier::Late => {
                                paint(
                                    &format!(
                                        "LATE ≤{:.0}¢ last {}s (heavy ≤{}s)",
                                        j_endgame.taker_max_ask * 100.0,
                                        j_endgame.late_max_secs,
                                        j_endgame.late_heavy_secs
                                    ),
                                    "yellow",
                                )
                            }
                            _ => paint("waiting", "dim"),
                        }
                    ));
                    if let Some(tape) = tape {
                        let (tape_usd, tape_n) =
                            trade_tape::TradeTapeTracker::winner_stats(tape, winner);
                        let tape_ok = strategy::strategy_j::tape_hot(tape, winner, j_endgame);
                        lines.push(format!(
                            "J tape {}: ${:.0}/{} buys (5s) | need ${:.0}/{}",
                            if tape_ok {
                                paint("HOT", "green")
                            } else {
                                paint("cold", "dim")
                            },
                            tape_usd,
                            tape_n,
                            j_endgame.min_tape_usd,
                            j_endgame.min_tape_buys,
                        ));
                        let depth = crate::orderbook::ask_depth_usd(
                            &if winner == "UP" {
                                &win.prices.up.book.asks
                            } else {
                                &win.prices.down.book.asks
                            },
                            j_endgame.taker_max_ask,
                        );
                        lines.push(format!(
                            "J ask depth (≤{:.0}¢): ${:.2}",
                            j_endgame.taker_max_ask * 100.0,
                            depth
                        ));
                    }
                }
            }
        } else if label == "NEXT" {
            lines.push(paint("J: monitoring NEXT (timeline controller)", "dim"));
        } else {
            lines.push(paint("J: hold-to-redeem | target +$1/window", "dim"));
        }
    } else if strategy_name == "cheap_hold_h" {
        if win.window_number == 0 {
            lines.push(paint(
                "H: window #0 warmup — no trading until window #1",
                "dim",
            ));
        } else if let Some(strat) = strat_engine.get_strategy_state(win.window_number) {
            let side = strat.h_entry_side.as_deref().unwrap_or("N/A");
            let side_tone = match side {
                "UP" => "green",
                "DOWN" => "red",
                _ => "dim",
            };
            let time_pct = match chrono::DateTime::parse_from_rfc3339(&m.end_time) {
                Ok(end) => {
                    let secs = (end.timestamp_millis() - now) / 1000;
                    let duration_sec = window_duration_sec(m);
                    let elapsed = (duration_sec - secs as f64).clamp(0.0, duration_sec);
                    if duration_sec > 0.0 {
                        (elapsed / duration_sec) * 100.0
                    } else {
                        0.0
                    }
                }
                Err(_) => 0.0,
            };
            let phase = strategy::strategy_h::phase_label(
                time_pct,
                strat.h_entry_done,
                strat.h_salvage_done,
            );
            let phase_tone = match phase {
                "entry" => "green",
                "salvage" => "yellow",
                "hold" => "cyan",
                "flat" => "dim",
                _ => "red",
            };
            lines.push(format!(
                "H Side: {} | Entry: {} | Phase: {}",
                paint(side, side_tone),
                if strat.h_entry_done {
                    paint("done", "green")
                } else {
                    paint("waiting", "yellow")
                },
                paint(phase, phase_tone),
            ));
            if label == "CURRENT" {
                let up_ask = win.prices.up.ask;
                let dn_ask = win.prices.down.ask;
                if let (Some(spot), Some(ptb)) = (spot_price, m.price_to_beat) {
                    let secs_to_end = match chrono::DateTime::parse_from_rfc3339(&m.end_time) {
                        Ok(end) => ((end.timestamp_millis() - now) / 1000).max(1),
                        Err(_) => 600,
                    };
                    let expected = redeem_hold::expected_move_usd(
                        current_atr.max(REDEEM_HOLD_MIN_VALID_ATR),
                        secs_to_end,
                    );
                    let gap_z = if expected > 0.0 {
                        (spot - ptb) / expected
                    } else {
                        0.0
                    };
                    lines.push(format!(
                        "H gap_z: {:+.2} | UP ask {:.2} | DN ask {:.2}",
                        gap_z, up_ask, dn_ask
                    ));
                    let entry_gate = if !strat.h_entry_done && time_pct <= 33.0 {
                        let cheap_ok =
                            strategy::strategy_h::pick_cheap_entry_side(&win.prices).is_some();
                        let gap_ok = strategy::strategy_h::gap_z_allows_entry(gap_z);
                        if cheap_ok && gap_ok {
                            paint("READY cheap ~0.38 near PTB", "green")
                        } else if !gap_ok {
                            paint("BLOCKED PTB gap too large", "red")
                        } else {
                            paint(
                                &format!("WAIT ask band (UP {:.2} / DN {:.2})", up_ask, dn_ask),
                                "yellow",
                            )
                        }
                    } else {
                        paint("n/a", "dim")
                    };
                    lines.push(format!("H Entry gate: {}", entry_gate));
                }
            }
            if win.status.starts_with("CLOSED") && win.spent > 0.0 {
                let entry_side = strat
                    .h_entry_side
                    .as_deref()
                    .or_else(|| first_buy(win).map(|t| t.side.as_str()))
                    .unwrap_or("N/A");
                let winner = match (spot_price, m.price_to_beat) {
                    (Some(spot), Some(ptb)) if ptb > 0.0 && spot > ptb => "UP",
                    (Some(_), Some(ptb)) if ptb > 0.0 => "DOWN",
                    _ => "",
                };
                let h = h_stats::derive_h_close_stats(&win.trades, entry_side, winner);
                let realized = win.cash_returned - win.spent;
                let market = match h.market_win {
                    Some(true) => paint("WIN", "green"),
                    Some(false) => paint("LOSE", "red"),
                    None => paint("n/a", "dim"),
                };
                lines.push(format!(
                    "H Close: entry={} real_winner={} | market={} salvaged={} salvage_win={} | fin PnL {:+.2}",
                    entry_side,
                    if winner.is_empty() { "n/a" } else { winner },
                    market,
                    h.salvaged,
                    h.salvage_win,
                    realized
                ));
            }
        } else if label == "NEXT" {
            lines.push(paint("H: monitoring NEXT (live-only, no pre-entry)", "dim"));
        } else {
            lines.push(paint("H: live-only — waiting for cheap entry", "dim"));
        }
    } else if let Some(strat) = strat_engine.get_strategy_state(win.window_number) {
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
        "timestamp,window_id,slug,action,order_type,side,amount,amount_kind,signal_shares,signal_usd_value,price,reason,executed,current_atr,secs_to_end,time_pct,spot_price,spot_velocity_usd_per_sec,spot_smoothed_velocity_usd_per_sec,spot_acceleration_usd_per_sec2,ptb,ptb_delta_usd,ptb_delta_pct,up_bid,up_ask,down_bid,down_ask,up_shares,down_shares,paired_shares,spent,returned,terminal_floor,terminal_floor_gap,mtm,unrealized_pnl",
        &format!(
            "{},{},{},{},{},{},{:.8},{},{:.8},{:.4},{:.4},{},{},{:.4},{},{:.2},{},{},{},{},{},{},{},{:.4},{:.4},{:.4},{:.4},{:.8},{:.8},{:.8},{:.4},{:.4},{:.4},{:.4},{:.4},{:.4}",
            get_now_ms(),
            window_number,
            slug,
            if sig.is_buy { "BUY" } else { "SELL" },
            sig.order_type.as_str(),
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

fn window_duration_sec(market: &MarketWindow) -> f64 {
    match (
        chrono::DateTime::parse_from_rfc3339(&market.start_time),
        chrono::DateTime::parse_from_rfc3339(&market.end_time),
    ) {
        (Ok(s), Ok(e)) => ((e.timestamp_millis() - s.timestamp_millis()) as f64 / 1000.0).max(1.0),
        _ => 900.0,
    }
}

fn build_window_close_meta(app: &AppState, win: &WindowState, secs_to_end: i64) -> WindowCloseMeta {
    let duration_sec = window_duration_sec(&win.market);
    let elapsed_sec = (duration_sec - secs_to_end as f64).clamp(0.0, duration_sec);
    let time_pct_at_close = (elapsed_sec / duration_sec) * 100.0;
    let final_atr = app.volatility_mgr.get_current_atr();
    let mid_snap = app.mid_cross_tracker.snapshot(win.window_number);

    let final_gap_z = match (app.spot_price, win.market.price_to_beat) {
        (Some(spot), Some(ptb)) if ptb > 0.0 => {
            let expected = redeem_hold::expected_move_usd(
                final_atr.max(REDEEM_HOLD_MIN_VALID_ATR),
                secs_to_end,
            );
            if expected > 0.0 {
                Some((spot - ptb) / expected)
            } else {
                None
            }
        }
        _ => None,
    };

    let (entry_side, entry_reason) = win
        .trades
        .iter()
        .find(|t| t.trade_type == "BUY")
        .map(|t| (t.side.clone(), t.reason.clone()))
        .unwrap_or_default();

    let mut would_redeem_hold = false;
    if let (Some(spot), Some(ptb)) = (app.spot_price, win.market.price_to_beat) {
        for side in ["UP", "DOWN"] {
            let shares = if side == "UP" {
                win.up_shares
            } else {
                win.down_shares
            };
            if shares <= 0.0 {
                continue;
            }
            let bid = if side == "UP" {
                win.prices.up.bid
            } else {
                win.prices.down.bid
            };
            let gap_z = itm_gap_z(side, spot, ptb, final_atr, secs_to_end);
            let fair_prob = if side == "UP" {
                0.5 + (gap_z * 0.08).min(0.45)
            } else {
                0.5 - (gap_z * 0.08).min(0.45)
            };
            if evaluate_redeem_hold(&RedeemHoldInput {
                side,
                spot,
                ptb,
                secs_to_end,
                time_pct: time_pct_at_close,
                current_atr: final_atr,
                bid,
                fair_prob: fair_prob.clamp(0.05, 0.95),
                ptb_crossed: false,
                counter_velocity_against: false,
                cex_velocity_against: false,
            })
            .should_hold
            {
                would_redeem_hold = true;
                break;
            }
        }
    }

    let utc_hour = chrono::DateTime::parse_from_rfc3339(&win.market.start_time)
        .ok()
        .and_then(|dt| dt.format("%H").to_string().parse().ok())
        .unwrap_or(0);

    WindowCloseMeta {
        strategy_name: app.config.strategy.clone(),
        utc_hour,
        time_pct_at_close,
        final_gap_z,
        final_atr,
        mid_cross_count: mid_snap.cross_count,
        significant_mid_cross_count: mid_snap.significant_cross_count,
        entry_side,
        entry_reason,
        would_redeem_hold,
    }
}

fn close_window_tracked(
    app: &mut AppState,
    win: &WindowState,
    status: &str,
    secs_to_end: i64,
) -> WindowState {
    let meta = build_window_close_meta(app, win, secs_to_end);
    let winner = match (app.spot_price, win.market.price_to_beat) {
        (Some(spot), Some(ptb)) if ptb > 0.0 && spot > ptb => "UP".to_string(),
        (Some(_), Some(ptb)) if ptb > 0.0 => "DOWN".to_string(),
        _ => String::new(),
    };
    let pnl;
    {
        let mut port = app.portfolio.lock().unwrap();
        port.close_window(
            win.window_number,
            status,
            app.spot_price,
            Some(meta.clone()),
        );
        pnl = port
            .windows
            .get(&win.window_number)
            .map(|w| w.cash_returned - w.spent)
            .unwrap_or(0.0);
    }

    if meta.strategy_name == "cheap_hold_h" && win.spent > 0.0 {
        let closed = app.portfolio.lock().unwrap();
        if let Some(w) = closed.windows.get(&win.window_number) {
            let h = h_stats::derive_h_close_stats(&w.trades, &meta.entry_side, &winner);
            app.system_logs.push(format!(
                "[H] #{} entry={} real_winner={} market={:?} salvaged={} salvage_win={} pnl={:+.2}",
                win.window_number,
                meta.entry_side,
                if winner.is_empty() { "n/a" } else { &winner },
                h.market_win,
                h.salvaged,
                h.salvage_win,
                pnl
            ));
            if app.system_logs.len() > 30 {
                app.system_logs.remove(0);
            }
        }
    }

    app.window_stats.record_close(&WindowCloseRecord {
        window_number: win.window_number,
        slug: win.market.slug.clone(),
        strategy_name: meta.strategy_name.clone(),
        pnl,
        spent: win.spent,
        final_atr: meta.final_atr,
        time_pct_at_close: meta.time_pct_at_close,
        final_gap_z: meta.final_gap_z,
        mid_cross_count: meta.mid_cross_count,
        significant_mid_cross_count: meta.significant_mid_cross_count,
        entry_side: meta.entry_side.clone(),
        entry_reason: meta.entry_reason.clone(),
        would_redeem_hold: meta.would_redeem_hold,
        winner,
        utc_hour: meta.utc_hour,
    });

    record_llm_result(
        app,
        win.window_number,
        &win.market,
        app.spot_price,
        &win.prices,
    );
    finalize_mid_cross_for_window(app, win.window_number, &win.market.slug);

    app.portfolio
        .lock()
        .unwrap()
        .get_or_create_window_state(win.window_number, "", &win.market)
        .clone()
}

fn finalize_mid_cross_for_window(app: &mut AppState, window_number: usize, slug: &str) {
    let summary = app.mid_cross_tracker.finalize_window(window_number);
    if summary.cross_count > 0 || summary.final_side.is_some() {
        append_mid_cross_window_summary(&app.run_log_dir, window_number, slug, &summary);
    }
    app.mid_cross_tracker.remove_window(window_number);
}

fn append_mid_cross_event(
    log_dir: &str,
    window_number: usize,
    slug: &str,
    event: &MidCrossEvent,
    spot_price: Option<f64>,
    market: &MarketWindow,
    spot_signal: SpotSignalSnapshot,
) {
    let (ptb_delta_usd, _) = match (spot_price, market.price_to_beat) {
        (Some(spot), Some(ptb)) if ptb > 0.0 => {
            let delta = spot - ptb;
            (Some(delta), Some((delta / ptb) * 100.0))
        }
        _ => (None, None),
    };

    append_csv_row(
        log_dir,
        "mid_cross_events.csv",
        "timestamp,window_id,slug,event,from_side,to_side,up_mid,down_mid,lead_gap,peak_prev_gap,is_significant,cross_count,significant_cross_count,time_pct,secs_to_end,current_atr,spot_price,ptb,ptb_delta_usd,spot_velocity,spot_smoothed_velocity,spot_acceleration",
        &format!(
            "{},{},{},{},{},{},{:.4},{:.4},{:.4},{:.4},{},{},{},{:.2},{},{:.4},{},{},{},{},{},{}",
            get_now_ms(),
            window_number,
            slug,
            event.event,
            event.from_side.map(|s| s.as_str()).unwrap_or(""),
            event.to_side.as_str(),
            event.up_mid,
            event.down_mid,
            event.lead_gap,
            event.peak_prev_gap,
            event.is_significant,
            event.cross_count,
            event.significant_cross_count,
            event.time_pct,
            event.secs_to_end,
            event.current_atr,
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
        ),
    );
}

fn append_mid_cross_window_summary(
    log_dir: &str,
    window_number: usize,
    slug: &str,
    summary: &mid_cross_tracker::MidCrossWindowSummary,
) {
    append_csv_row(
        log_dir,
        "mid_cross_window_summary.csv",
        "timestamp,window_id,slug,cross_count,significant_cross_count,final_side,last_cross_atr",
        &format!(
            "{},{},{},{},{},{},{}",
            get_now_ms(),
            window_number,
            slug,
            summary.cross_count,
            summary.significant_cross_count,
            summary.final_side.map(|s| s.as_str()).unwrap_or(""),
            summary.last_cross_atr,
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
