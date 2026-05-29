#![allow(dead_code, unused_imports, non_snake_case, unused_variables, unused_mut)]

mod config;
mod client;
mod trader;
mod strategy;
mod volatility;
mod analytics;

use config::Config;
use client::{MarketEvent, MarketWindow, PricesState, get_now_ms};
use trader::{Portfolio, WindowState};
use strategy::StrategyEngine;
use volatility::VolatilityManager;

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

    let asset = args.get(1).cloned().unwrap_or_else(|| "BTC".to_string()).to_uppercase();
    let interval = args.get(2).cloned().unwrap_or_else(|| "5m".to_string()).to_lowercase();

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
            println!("Warning: Clock sync failed: {}. Using local system clock.", e);
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

    let portfolio = Arc::new(Mutex::new(Portfolio::new(config.session.starting_bank)));
    let strategy_engine = Arc::new(Mutex::new(StrategyEngine::new(&config.strategy)));

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
    };

    app_state.system_logs.push(format!("GEM System Initialized for {} {}", asset, interval));
    app_state.system_logs.push(format!("System clock synchronized. Offset updated."));

    // Spawn Chainlink Spot WS Feed
    let tx_spot = event_tx.clone();
    client::subscribe_chainlink(asset.clone(), tx_spot);

    // Initial Market Discovery
    discover_initial_markets(&mut app_state, &event_tx).await;

    // ─── 3. Event Loop & Tickers ───────────────────────────────────────────
    let mut render_interval = tokio::time::interval(Duration::from_millis(250));
    let mut monitor_interval = tokio::time::interval(Duration::from_millis(1000));

    loop {
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
async fn discover_initial_markets(app: &mut AppState, event_tx: &mpsc::UnboundedSender<MarketEvent>) {
    app.system_logs.push("Searching for active and upcoming windows on Polymarket...".to_string());

    // A. Detect CURRENT Active Window
    if let Some(active) = client::find_active_market(&app.asset, &app.interval).await {
        app.system_logs.push(format!("FOUND ACTIVE CURRENT WINDOW: {}", active.slug));
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
        app.system_logs.push("No active window found on Polymarket right now.".to_string());
        app.next_window_number = 1;
    }

    // B. Detect NEXT Upcoming Window
    find_and_subscribe_next(app, event_tx).await;
}

/**
 * Find and subscribe to the NEXT upcoming window.
 */
async fn find_and_subscribe_next(app: &mut AppState, event_tx: &mpsc::UnboundedSender<MarketEvent>) {
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

    app.system_logs.push(format!("Searching for NEXT WINDOW #{}...", app.next_window_number));

    if let Some(next_m) = client::find_next_market(&app.asset, &app.interval, after_time, &app.exclude_slugs).await {
        app.system_logs.push(format!("FOUND NEXT WINDOW #{}: {}", app.next_window_number, next_m.slug));
        app.exclude_slugs.push(next_m.slug.clone());

        let mut port = app.portfolio.lock().unwrap();
        let win_state = port.get_or_create_window_state(app.next_window_number, "NEXT", &next_m);
        app.next_window = Some(win_state.clone());

        // Subscribe prices
        let handle = client::subscribe_prices(app.next_window_number, "NEXT".to_string(), next_m, event_tx.clone());
        app.next_sub = Some(handle);
    } else {
        app.system_logs.push("No upcoming NEXT window found. Retrying in 10s...".to_string());
        let tx = event_tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(10)).await;
            tx.send(MarketEvent::Log("Triggering NEXT window retry...".to_string())).unwrap_or_default();
        });
    }
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

    if let Some(next) = &app.next_window {
        if next.status == "WAITING_ENTRY" {
            if let Ok(start) = chrono::DateTime::parse_from_rfc3339(&next.market.start_time) {
                let secs_to_start = (start.timestamp_millis() - now) / 1000;
                
                if secs_to_start <= 0 {
                    promote_next_to_current(app, event_tx).await;
                    return;
                }

                // Проверяем, укладываемся ли в коридор покупки (например, [120с - 5с])
                let is_within_time = secs_to_start >= app.config.pre_start_entry.min_seconds_before_start
                                  && secs_to_start <= app.config.pre_start_entry.max_seconds_before_start;
                if is_within_time {
                    let current_atr = app.volatility_mgr.get_current_atr();
                    
                    // Логируем причину пропуска по волатильности без спама
                    if current_atr < app.config.min_btc_atr {
                        let log_msg = format!("[STRATEGY] Skipping Window #{}: Volatility too low (ATR: ${:.2} < Min: ${:.2})", next.window_number, current_atr, app.config.min_btc_atr);
                        if !app.system_logs.contains(&log_msg) {
                            app.system_logs.push(log_msg);
                        }
                    } else {
                        let prices = next.prices.clone();
                        let mut strat = app.strategy.lock().unwrap();
                        if let Some((up_ask, dn_ask)) = strat.check_pre_start_entry(&app.config, &prices, next.window_number, secs_to_start, current_atr) {
                            trigger_buy = true;
                            up_ask_val = up_ask;
                            dn_ask_val = dn_ask;
                            window_num = next.window_number;
                            next_market_opt = Some(next.market.clone());
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
            let total_cost = app.config.session.buy_up_usd + app.config.session.buy_down_usd;
            if port.available_cash >= total_cost {
                app.system_logs.push(format!("[STRATEGY] Pre-start entry triggered for Window #{}. UP Ask: ${:.2} | DOWN Ask: ${:.2}", window_num, up_ask_val, dn_ask_val));
                
                port.execute_buy(window_num, "UP", app.config.session.buy_up_usd, up_ask_val, "pre_start_entry_50_51");
                port.execute_buy(window_num, "DOWN", app.config.session.buy_down_usd, dn_ask_val, "pre_start_entry_50_51");
                
                let updated = port.get_or_create_window_state(window_num, "", &next_market);
                app.next_window = Some(updated.clone());
            } else {
                app.system_logs.push(format!("[STRATEGY] REJECTED entry for Window #{}: Insufficient cash (${:.2} needed, ${:.2} available)", window_num, total_cost, port.available_cash));
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
                    app.system_logs.push(format!("[SAFETY CLOSE] Window #{} past end time ({}s). Force closing.", current.window_number, secs_to_end));
                    let mut port = app.portfolio.lock().unwrap();
                    port.close_window(current.window_number, "CLOSED_TIME");
                    
                    let updated = port.get_or_create_window_state(current.window_number, "", &current.market);
                    app.current_window = Some(updated.clone());
                }
            }
        }
    }
}

/**
 * Promote NEXT window to CURRENT (LIVE) window.
 */
async fn promote_next_to_current(app: &mut AppState, event_tx: &mpsc::UnboundedSender<MarketEvent>) {
    let next_win = match &app.next_window {
        Some(w) => w.clone(),
        None => return,
    };

    // Close old CURRENT window if still open
    if let Some(curr) = &app.current_window {
        if curr.status == "LIVE" || curr.status == "SKIPPED" {
            app.system_logs.push(format!("[Lifecycle] Force closing overlapping CURRENT Window #{}", curr.window_number));
            let mut port = app.portfolio.lock().unwrap();
            port.close_window(curr.window_number, "CLOSED_TIME");
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
        app.system_logs.push(format!("[Lifecycle] PROMOTE NEXT WINDOW #{} ({}) TO CURRENT (LIVE)", next_win.window_number, next_win.market.slug));
    } else {
        port.skipped_windows += 1;
        app.system_logs.push(format!("[Lifecycle] PROMOTE NEXT WINDOW #{} ({}) TO CURRENT (SKIPPED)", next_win.window_number, next_win.market.slug));
    }

    let promoted = port.get_or_create_window_state(next_win.window_number, "CURRENT", &next_win.market);
    if is_entered {
        promoted.status = "LIVE".to_string();
    } else {
        promoted.status = "SKIPPED".to_string();
    }

    app.current_window = Some(promoted.clone());
    app.next_window = None;
    app.next_window_number += 1;

    // Re-subscribe prices under role 'CURRENT'
    let handle = client::subscribe_prices(next_win.window_number, "CURRENT".to_string(), next_win.market.clone(), event_tx.clone());
    app.current_sub = Some(handle);

    // Search for new upcoming NEXT window
    drop(port);
    find_and_subscribe_next(app, event_tx).await;
}

/**
 * Handle incoming channel events from CLOB and Spot price WebSockets.
 */
async fn process_event(app: &mut AppState, event: MarketEvent, _event_tx: &mpsc::UnboundedSender<MarketEvent>) {
    match event {
        MarketEvent::Log(msg) => {
            app.system_logs.push(msg);
            if app.system_logs.len() > 30 {
                app.system_logs.remove(0);
            }
        }
        MarketEvent::SpotTick { asset: _, price, timestamp } => {
            app.spot_price = Some(price);

            // 1. Exact open price (PTB) capture at startTime for CURRENT window if missing
            if let Some(curr) = &mut app.current_window {
                if curr.market.price_to_beat.is_none() {
                    if let Ok(start) = chrono::DateTime::parse_from_rfc3339(&curr.market.start_time) {
                        if timestamp >= start.timestamp_millis() {
                            app.system_logs.push(format!("[CAPTURE PTB] Captured exact open price from Chainlink WS: ${:.2}", price));
                            curr.market.price_to_beat = Some(price);
                            
                            // Write back to paper trader!
                            let mut port = app.portfolio.lock().unwrap();
                            let win = port.get_or_create_window_state(curr.window_number, "", &curr.market);
                            win.market.price_to_beat = Some(price);
                        }
                    }
                }
            }
        }
        MarketEvent::MarketTick { window_number, role, market, prices, timestamp } => {
            // Update prices inside portfolio
            let mut port = app.portfolio.lock().unwrap();
            let win_state = port.get_or_create_window_state(window_number, "", &market).clone();
            port.get_or_create_window_state(window_number, "", &market).prices = prices.clone();

            // Run Strategy Engine
            let mut strat = app.strategy.lock().unwrap();

            if role == "CURRENT" {
                if let Ok(end) = chrono::DateTime::parse_from_rfc3339(&market.end_time) {
                    let secs_to_end = (end.timestamp_millis() - timestamp) / 1000;
                    
                    let signals = strat.process_live_tick(&app.config, &prices, app.spot_price, &win_state.market, &win_state, secs_to_end);
                    
                    for sig in signals {
                        if sig.is_buy {
                            port.execute_buy(window_number, &sig.side, sig.amount, sig.price, &sig.reason);
                        } else {
                            port.execute_sell(window_number, &sig.side, sig.amount, sig.price, &sig.reason);
                        }
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

    println!("{}", paint("=================================================================================", "dim"));
    let strategy_title = format!("STRATEGY: {}", app.config.strategy.to_uppercase().replace("_", " "));
    println!("  {}     {}     {}", paint(&strategy_title, "bold"), paint(&format!("Asset: {}", app.asset), "cyan"), paint(&format!("Interval: {}", app.interval), "cyan"));
    println!("{}", paint("=================================================================================", "dim"));

    let runtime = format_runtime(get_now_ms() - app.started_at);
    let win_pct = if p.entered_windows > 0 { (p.wins as f64 / p.entered_windows as f64) * 100.0 } else { 0.0 };
    let loss_pct = if p.entered_windows > 0 { (p.losses as f64 / p.entered_windows as f64) * 100.0 } else { 0.0 };

    let atr = app.volatility_mgr.get_current_atr();
    let atr_str = if atr > 0.0 { format!("${:.2}", atr) } else { "Warming up...".to_string() };

    println!("  Started: {} | Runtime: {} | BTC ATR(1m): {}", 
             paint(&format_utc(app.started_at), "cyan"), 
             paint(&runtime, "bold"),
             paint(&atr_str, "yellow"));

    let total_windows = p.entered_windows + p.skipped_windows;
    println!("  Total Windows: {} | Entered: {} | Skipped: {}", 
             paint(&total_windows.to_string(), "bold"),
             paint(&p.entered_windows.to_string(), "cyan"),
             paint(&p.skipped_windows.to_string(), "yellow"));
    println!("  Wins: {} ({:.1}%) | Losses: {} ({:.1}%)", 
             paint(&p.wins.to_string(), "green"), win_pct, 
             paint(&p.losses.to_string(), "red"), loss_pct);
             
    let pnl_sign = if p.overall_realized_pnl >= 0.0 { "+" } else { "" };
    let pnl_color = if p.overall_realized_pnl >= 0.0 { "green" } else { "red" };
    println!("  Starting bank: ${:.2} | Cash: ${:.2} | Equity: ${:.2} | Realized PnL: {}", 
             p.starting_bank, p.available_cash, p.equity, 
             paint(&format!("{}{:.2}", pnl_sign, p.overall_realized_pnl), pnl_color));
    println!("{}", paint("=================================================================================", "dim"));

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

    let left_lines = render_window_block(&current_window, "CURRENT", app.spot_price, app.strategy.clone());
    let right_lines = render_window_block(&next_window, "NEXT", app.spot_price, app.strategy.clone());

    // Render blocks vertically
    for line in left_lines {
        println!("  {}", line);
    }
    println!("{}", paint("─────────────────────────────────────────────────────────────────────────────────", "dim"));
    for line in right_lines {
        println!("  {}", line);
    }

    println!("{}", paint("=================================================================================", "dim"));
    println!("  {}", paint("SYSTEM EVENT LOG:", "cyan"));
    let max_logs = 6;
    let start_idx = app.system_logs.len().saturating_sub(max_logs);
    for log in &app.system_logs[start_idx..] {
        println!("  • {}", paint(log, "dim"));
    }
    println!("{}", paint("=================================================================================", "dim"));
}

fn render_window_block(
    win_opt: &Option<WindowState>,
    label: &str,
    spot_price: Option<f64>,
    strategy: Arc<Mutex<StrategyEngine>>,
) -> Vec<String> {
    let mut lines = vec![];
    
    let label_colored = if label == "CURRENT" { paint(label, "green") } else { paint(label, "yellow") };

    let Some(win) = win_opt else {
        lines.push(format!("--- {} WINDOW ---", label_colored));
        lines.push(paint("Waiting for market stream...", "dim"));
        return lines;
    };

    let m = &win.market;
    lines.push(format!("--- {} WINDOW #{} ---", label_colored, paint(&win.window_number.to_string(), "bold")));
    lines.push(format!("Slug: {}", paint(&m.slug, "dim")));
    
    let start_time = m.start_time.chars().take(19).collect::<String>().replace("T", " ");
    let end_time = m.end_time.chars().take(19).collect::<String>().replace("T", " ");
    lines.push(format!("Start: {}", paint(&start_time, "dim")));
    lines.push(format!("End:   {}", paint(&end_time, "dim")));

    let now = get_now_ms();
    let start_dt = chrono::DateTime::parse_from_rfc3339(&m.start_time).unwrap().timestamp_millis();
    let end_dt = chrono::DateTime::parse_from_rfc3339(&m.end_time).unwrap().timestamp_millis();

    if now < start_dt {
        let secs = (start_dt - now) / 1000;
        lines.push(format!("Status: {} | Starts In: {}", paint("WAITING", "yellow"), paint(&format_countdown(secs), "bold")));
    } else if now < end_dt {
        let secs = (end_dt - now) / 1000;
        lines.push(format!("Status: {} | Time Left: {}", paint("LIVE", "green"), paint(&format_countdown(secs), "bold")));
    } else {
        lines.push(format!("Status: {}", paint("EXPIRED", "red")));
    }

    let strike_str = m.price_to_beat.map(|p| format!("${:.2}", p)).unwrap_or_else(|| "N/A".to_string());
    lines.push(format!("Price to Beat (Strike): {}", paint(&strike_str, "magenta")));

    let spot_str = spot_price.map(|p| format!("${:.2}", p)).unwrap_or_else(|| "N/A".to_string());
    let distance_str = match (spot_price, m.price_to_beat) {
        (Some(s), Some(p)) => {
            let delta = s - p;
            let (tone, formatted) = if delta >= 0.0 {
                ("green", format!("+${:.2}", delta))
            } else {
                ("red", format!("-${:.2}", delta.abs()))
            };
            paint(&formatted, tone)
        }
        _ => paint("N/A", "dim"),
    };
    lines.push(format!("Live Spot Price: {} | Dist: {}", paint(&spot_str, "cyan"), distance_str));
    lines.push(paint("--------------------------------------", "dim"));

    let UP = &win.prices.up;
    let DN = &win.prices.down;
    let up_price = if UP.ask > 0.0 { UP.ask } else { UP.bid };
    let dn_price = if DN.ask > 0.0 { DN.ask } else { DN.bid };
    let up_chance = (up_price * 100.0).clamp(0.0, 100.0);
    let dn_chance = (dn_price * 100.0).clamp(0.0, 100.0);

    lines.push(format!("UP   YES Bid/Ask: {:.2} / {}  [{}]", UP.bid, paint(&format!("{:.2}", UP.ask), "green"), paint(&format!("{:.1}%", up_chance), "green")));
    lines.push(format!("DOWN YES Bid/Ask: {:.2} / {}  [{}]", DN.bid, paint(&format!("{:.2}", DN.ask), "red"), paint(&format!("{:.1}%", dn_chance), "red")));
    lines.push(format!("Combined Ask:    {}", paint(&format!("{:.2}", UP.ask + DN.ask), "bold")));
    lines.push(paint("--------------------------------------", "dim"));

    // Display Account position
    let spent = win.spent;
    let returned = win.cash_returned;
    let mtm = win.up_shares * UP.bid + win.down_shares * DN.bid;
    let pnl = (returned + mtm) - spent;
    lines.push(format!("Spent: ${:.2} | Returned: ${:.2}", spent, returned));
    
    let pnl_sign = if pnl >= 0.0 { "+" } else { "" };
    let pnl_tone = if pnl >= 0.0 { "green" } else { "red" };
    lines.push(format!("Est. Val: ${:.2} | PnL: {}", returned + mtm, paint(&format!("{}{:.2}", pnl_sign, pnl), pnl_tone)));
    lines.push(format!("UP shares: {} | DOWN shares: {}", paint(&format!("{:.4}", win.up_shares), "green"), paint(&format!("{:.4}", win.down_shares), "red")));
    lines.push(paint("--------------------------------------", "dim"));

    // Strategy status
    let strat_engine = strategy.lock().unwrap();
    if let Some(strat) = strat_engine.get_strategy_state(win.window_number) {
        if strat.first_sold_side.is_none() {
            lines.push(format!("Exit Trigger: {} (Active both)", paint(">= 0.65", "yellow")));
        } else {
            let second = if strat.first_sold_side.as_deref() == Some("UP") { "DOWN" } else { "UP" };
            let crossed = if strat.ptb_crossed { paint("PTB Crossed! Active!", "green") } else { paint("Waiting PTB cross...", "yellow") };
            lines.push(format!("First Sold: {}", paint(strat.first_sold_side.as_ref().unwrap(), "green")));
            lines.push(format!("Second Target ({}): {}", paint(second, "yellow"), paint("0.65", "bold")));
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
        let trade_tone = if t.trade_type == "BUY" { "green" } else { "yellow" };
        let side_tone = if t.side == "UP" { "green" } else { "red" };
        lines.push(format!("[{}] {} {} @${:.2}", paint(&ts_str, "dim"), paint(&t.trade_type, trade_tone), paint(&t.side, side_tone), t.price));
    }

    lines
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
        dt.to_rfc3339().chars().take(19).collect::<String>().replace("T", " ")
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
