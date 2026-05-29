use crate::client::{MarketWindow, PricesState, get_now_ms};
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct TradeRecord {
    pub timestamp: i64,
    pub trade_type: String, // "BUY" / "SELL"
    pub side: String,       // "UP" / "DOWN"
    pub reason: String,
    pub price: f64,
    pub shares: f64,
    pub usd_value: f64,
    pub available_cash_after: f64,
}

#[derive(Debug, Clone)]
pub struct WindowState {
    pub window_number: usize,
    pub role: String,
    pub status: String, // "WAITING_ENTRY", "ENTERED_PRE_START", "LIVE", "CLOSED_TARGET", "CLOSED_TIME", "SKIPPED"
    pub market: MarketWindow,
    pub spent: f64,
    pub cash_returned: f64,
    pub up_shares: f64,
    pub down_shares: f64,
    pub initial_up_shares: f64,
    pub initial_down_shares: f64,
    pub trades: Vec<TradeRecord>,
    pub prices: PricesState,
}

#[derive(Debug, Clone)]
pub struct Portfolio {
    pub starting_bank: f64,
    pub available_cash: f64,
    pub overall_realized_pnl: f64,
    pub equity: f64,
    pub entered_windows: u32,
    pub closed_windows: u32,
    pub wins: u32,
    pub losses: u32,
    pub skipped_windows: u32,
    pub windows: HashMap<usize, WindowState>,
}

#[derive(Debug, Clone)]
pub struct PortfolioSnapshot {
    pub starting_bank: f64,
    pub available_cash: f64,
    pub overall_realized_pnl: f64,
    pub equity: f64,
    pub entered_windows: u32,
    pub closed_windows: u32,
    pub wins: u32,
    pub losses: u32,
    pub skipped_windows: u32,
}

impl Portfolio {
    pub fn new(starting_bank: f64) -> Self {
        Self {
            starting_bank,
            available_cash: starting_bank,
            overall_realized_pnl: 0.0,
            equity: starting_bank,
            entered_windows: 0,
            closed_windows: 0,
            wins: 0,
            losses: 0,
            skipped_windows: 0,
            windows: HashMap::new(),
        }
    }

    pub fn get_portfolio_snapshot(&self) -> PortfolioSnapshot {
        PortfolioSnapshot {
            starting_bank: self.starting_bank,
            available_cash: self.available_cash,
            overall_realized_pnl: self.overall_realized_pnl,
            equity: self.equity,
            entered_windows: self.entered_windows,
            closed_windows: self.closed_windows,
            wins: self.wins,
            losses: self.losses,
            skipped_windows: self.skipped_windows,
        }
    }

    pub fn get_or_create_window_state(&mut self, window_number: usize, role: &str, market: &MarketWindow) -> &mut WindowState {
        self.windows.entry(window_number).or_insert_with(|| WindowState {
            window_number,
            role: role.to_string(),
            status: "WAITING_ENTRY".to_string(),
            market: market.clone(),
            spent: 0.0,
            cash_returned: 0.0,
            up_shares: 0.0,
            down_shares: 0.0,
            initial_up_shares: 0.0,
            initial_down_shares: 0.0,
            trades: vec![],
            prices: PricesState {
                up: crate::client::ContractPrices { bid: 0.0, ask: 0.0 },
                down: crate::client::ContractPrices { bid: 0.0, ask: 0.0 },
            },
        });

        let w = self.windows.get_mut(&window_number).unwrap();
        if !role.is_empty() && w.role != role {
            w.role = role.to_string();
        }
        w
    }

    pub fn execute_buy(&mut self, window_number: usize, side: &str, usd_amount: f64, ask_price: f64, reason: &str) -> Option<TradeRecord> {
        if ask_price <= 0.0 || usd_amount <= 0.0 {
            return None;
        }
        if self.available_cash < usd_amount {
            return None;
        }

        self.available_cash -= usd_amount;
        
        // Evaluate and freeze cash_after to avoid borrow conflict later
        let available_cash_after = self.available_cash;

        // Perform mutable operations
        let market = self.windows.get(&window_number)?.market.clone();
        let win = self.get_or_create_window_state(window_number, "", &market);

        let shares = usd_amount / ask_price;
        win.spent += usd_amount;

        if side == "UP" {
            win.up_shares += shares;
            win.initial_up_shares += shares;
        } else {
            win.down_shares += shares;
            win.initial_down_shares += shares;
        }

        if win.status == "WAITING_ENTRY" {
            win.status = "ENTERED_PRE_START".to_string();
        }

        let trade = TradeRecord {
            timestamp: get_now_ms(),
            trade_type: "BUY".to_string(),
            side: side.to_string(),
            reason: reason.to_string(),
            price: ask_price,
            shares,
            usd_value: usd_amount,
            available_cash_after,
        };

        win.trades.push(trade.clone());
        self.recalculate_equity();
        Some(trade)
    }

    pub fn execute_sell(&mut self, window_number: usize, side: &str, shares_amount: f64, bid_price: f64, reason: &str) -> Option<TradeRecord> {
        if bid_price <= 0.0 || shares_amount <= 0.0 {
            return None;
        }

        // Query available shares first, cleanly avoiding borrow conflicts
        let available = if let Some(w) = self.windows.get(&window_number) {
            if side == "UP" { w.up_shares } else { w.down_shares }
        } else {
            0.0
        };

        let mut shares = shares_amount;
        if shares > available {
            shares = available;
        }

        if shares <= 0.0 {
            return None;
        }

        let usd_value = shares * bid_price;
        self.available_cash += usd_value;

        // Freeze cash_after to avoid borrow conflicts
        let available_cash_after = self.available_cash;

        // Perform mutable operations
        let market = self.windows.get(&window_number)?.market.clone();
        let win = self.get_or_create_window_state(window_number, "", &market);
        win.cash_returned += usd_value;

        if side == "UP" {
            win.up_shares -= shares;
        } else {
            win.down_shares -= shares;
        }

        let trade = TradeRecord {
            timestamp: get_now_ms(),
            trade_type: "SELL".to_string(),
            side: side.to_string(),
            reason: reason.to_string(),
            price: bid_price,
            shares,
            usd_value,
            available_cash_after,
        };

        win.trades.push(trade.clone());
        self.recalculate_equity();
        Some(trade)
    }

    pub fn sell_all(&mut self, window_number: usize, reason: &str) -> Vec<TradeRecord> {
        let mut trades = vec![];
        if let Some(win) = self.windows.get(&window_number).cloned() {
            let up_bid = win.prices.up.bid;
            let dn_bid = win.prices.down.bid;

            if win.up_shares > 0.0 && up_bid > 0.0 {
                if let Some(t) = self.execute_sell(window_number, "UP", win.up_shares, up_bid, reason) {
                    trades.push(t);
                }
            }
            if win.down_shares > 0.0 && dn_bid > 0.0 {
                if let Some(t) = self.execute_sell(window_number, "DOWN", win.down_shares, dn_bid, reason) {
                    trades.push(t);
                }
            }
        }
        trades
    }

    pub fn close_window(&mut self, window_number: usize, status: &str) {
        // Sell off anything left
        self.sell_all(window_number, "time_stop_emergency");

        if let Some(win) = self.windows.get_mut(&window_number) {
            // FIX Auto-Loss Bug: Only count towards session stats and wins/losses if we actually entered (spent > 0)
            if win.spent > 0.0 {
                self.closed_windows += 1;
                let realized = win.cash_returned - win.spent;
                self.overall_realized_pnl += realized;
                
                if realized > 0.0 {
                    self.wins += 1;
                } else {
                    self.losses += 1;
                }

                // Append closed window metrics to logs/trades_history.csv
                if let Err(e) = std::fs::create_dir_all("logs") {
                    eprintln!("Failed to create logs dir: {:?}", e);
                }
                let file_path = "logs/trades_history.csv";
                let file_exists = std::path::Path::new(file_path).exists();
                match std::fs::OpenOptions::new()
                    .create(true)
                    .write(true)
                    .append(true)
                    .open(file_path)
                {
                    Ok(mut file) => {
                        use std::io::Write;
                        if !file_exists {
                            let _ = writeln!(file, "window_id,slug,spent,returned,pnl,timestamp");
                        }
                        let _ = writeln!(
                            file,
                            "{},{},{:.2},{:.2},{:.2},{}",
                            win.window_number,
                            win.market.slug,
                            win.spent,
                            win.cash_returned,
                            realized,
                            get_now_ms()
                        );
                    }
                    Err(e) => {
                        eprintln!("Failed to append to trades_history.csv: {:?}", e);
                    }
                }
            }
            win.status = status.to_string();
            win.role = "PAST".to_string();
        }
        self.recalculate_equity();
    }

    pub fn recalculate_equity(&mut self) {
        let mut equity = self.available_cash;

        for win in self.windows.values() {
            if win.status != "CLOSED_TARGET" && win.status != "CLOSED_TIME" && win.status != "SKIPPED" {
                let up_val = win.up_shares * win.prices.up.bid;
                let dn_val = win.down_shares * win.prices.down.bid;
                equity += up_val + dn_val;
            }
        }

        self.equity = equity;
    }
}
