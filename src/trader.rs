use crate::client::{get_now_ms, MarketWindow, PricesState};
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
    pub log_dir: String,
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
        Self::new_with_log_dir(starting_bank, "logs".to_string())
    }

    pub fn new_with_log_dir(starting_bank: f64, log_dir: String) -> Self {
        if let Err(e) = std::fs::create_dir_all(&log_dir) {
            eprintln!("Failed to create portfolio log dir {}: {:?}", log_dir, e);
        }
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
            log_dir,
        }
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

    fn append_trade_event(log_dir: &str, window_number: usize, slug: &str, trade: &TradeRecord) {
        Self::append_csv_row(
            log_dir,
            "trade_events.csv",
            "timestamp,window_id,slug,type,side,reason,price,shares,usd_value,cash_after",
            &format!(
                "{},{},{},{},{},{},{:.4},{:.8},{:.4},{:.4}",
                trade.timestamp,
                window_number,
                slug,
                trade.trade_type,
                trade.side,
                trade.reason,
                trade.price,
                trade.shares,
                trade.usd_value,
                trade.available_cash_after
            ),
        );
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

    pub fn get_or_create_window_state(
        &mut self,
        window_number: usize,
        role: &str,
        market: &MarketWindow,
    ) -> &mut WindowState {
        self.windows
            .entry(window_number)
            .or_insert_with(|| WindowState {
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

    pub fn execute_buy(
        &mut self,
        window_number: usize,
        side: &str,
        usd_amount: f64,
        ask_price: f64,
        reason: &str,
    ) -> Option<TradeRecord> {
        if ask_price <= 0.0 || usd_amount <= 0.0 {
            return None;
        }
        if self.available_cash < usd_amount {
            return None;
        }

        self.available_cash -= usd_amount;

        // Evaluate and freeze cash_after to avoid borrow conflict later
        let available_cash_after = self.available_cash;

        let market = self.windows.get(&window_number)?.market.clone();
        let (trade, slug) = {
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
            (trade, win.market.slug.clone())
        };
        Self::append_trade_event(&self.log_dir, window_number, &slug, &trade);
        self.recalculate_equity();
        Some(trade)
    }

    pub fn execute_sell(
        &mut self,
        window_number: usize,
        side: &str,
        shares_amount: f64,
        bid_price: f64,
        reason: &str,
    ) -> Option<TradeRecord> {
        if bid_price <= 0.0 || shares_amount <= 0.0 {
            return None;
        }

        // Query available shares first, cleanly avoiding borrow conflicts
        let available = if let Some(w) = self.windows.get(&window_number) {
            if side == "UP" {
                w.up_shares
            } else {
                w.down_shares
            }
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

        let market = self.windows.get(&window_number)?.market.clone();
        let (trade, slug) = {
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
            (trade, win.market.slug.clone())
        };
        Self::append_trade_event(&self.log_dir, window_number, &slug, &trade);
        self.recalculate_equity();
        Some(trade)
    }

    pub fn sell_all(&mut self, window_number: usize, reason: &str) -> Vec<TradeRecord> {
        let mut trades = vec![];
        if let Some(win) = self.windows.get(&window_number).cloned() {
            let up_bid = win.prices.up.bid;
            let dn_bid = win.prices.down.bid;

            if win.up_shares > 0.0 && up_bid > 0.0 {
                if let Some(t) =
                    self.execute_sell(window_number, "UP", win.up_shares, up_bid, reason)
                {
                    trades.push(t);
                }
            }
            if win.down_shares > 0.0 && dn_bid > 0.0 {
                if let Some(t) =
                    self.execute_sell(window_number, "DOWN", win.down_shares, dn_bid, reason)
                {
                    trades.push(t);
                }
            }
        }
        trades
    }

    pub fn close_window(&mut self, window_number: usize, status: &str, spot_price: Option<f64>) {
        let mut redeemed = false;

        let mut win_market = None;
        if let Some(w) = self.windows.get(&window_number) {
            win_market = Some((w.market.clone(), w.up_shares, w.down_shares));
        }

        if let Some((market, up_shares, down_shares)) = win_market {
            if let (Some(spot), Some(ptb)) = (spot_price, market.price_to_beat) {
                if ptb > 0.0 {
                    let up_won = spot > ptb;

                    // Редемп выигрышной стороны строго по 1.00$
                    if up_won {
                        if up_shares > 0.0 {
                            let val = up_shares * 1.00;
                            self.available_cash += val;
                            let cash_after = self.available_cash;

                            if let Some(win) = self.windows.get_mut(&window_number) {
                                win.cash_returned += val;
                                win.up_shares = 0.0;
                                win.trades.push(TradeRecord {
                                    timestamp: get_now_ms(),
                                    trade_type: "REDEEM".to_string(),
                                    side: "UP".to_string(),
                                    reason: "option_expired_itm_win_1.00".to_string(),
                                    price: 1.00,
                                    shares: up_shares,
                                    usd_value: val,
                                    available_cash_after: cash_after,
                                });
                            }
                        }
                        if down_shares > 0.0 {
                            let cash_after = self.available_cash;
                            if let Some(win) = self.windows.get_mut(&window_number) {
                                win.down_shares = 0.0;
                                win.trades.push(TradeRecord {
                                    timestamp: get_now_ms(),
                                    trade_type: "EXPIRED".to_string(),
                                    side: "DOWN".to_string(),
                                    reason: "option_expired_otm_loss_0.00".to_string(),
                                    price: 0.00,
                                    shares: down_shares,
                                    usd_value: 0.00,
                                    available_cash_after: cash_after,
                                });
                            }
                        }
                    } else {
                        if down_shares > 0.0 {
                            let val = down_shares * 1.00;
                            self.available_cash += val;
                            let cash_after = self.available_cash;

                            if let Some(win) = self.windows.get_mut(&window_number) {
                                win.cash_returned += val;
                                win.down_shares = 0.0;
                                win.trades.push(TradeRecord {
                                    timestamp: get_now_ms(),
                                    trade_type: "REDEEM".to_string(),
                                    side: "DOWN".to_string(),
                                    reason: "option_expired_itm_win_1.00".to_string(),
                                    price: 1.00,
                                    shares: down_shares,
                                    usd_value: val,
                                    available_cash_after: cash_after,
                                });
                            }
                        }
                        if up_shares > 0.0 {
                            let cash_after = self.available_cash;
                            if let Some(win) = self.windows.get_mut(&window_number) {
                                win.up_shares = 0.0;
                                win.trades.push(TradeRecord {
                                    timestamp: get_now_ms(),
                                    trade_type: "EXPIRED".to_string(),
                                    side: "UP".to_string(),
                                    reason: "option_expired_otm_loss_0.00".to_string(),
                                    price: 0.00,
                                    shares: up_shares,
                                    usd_value: 0.00,
                                    available_cash_after: cash_after,
                                });
                            }
                        }
                    }
                    redeemed = true;
                }
            }
        }

        // Если не удалось точно определить победителя (нет спота или страйка) - делаем обычный экстренный сброс по Bid
        if !redeemed {
            self.sell_all(window_number, "time_stop_emergency");
        }

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

                Self::append_csv_row(
                    &self.log_dir,
                    "window_summary.csv",
                    "timestamp,window_id,slug,status,spent,returned,pnl,close_spot,ptb,winner",
                    &format!(
                        "{},{},{},{},{:.4},{:.4},{:.4},{},{},{}",
                        get_now_ms(),
                        win.window_number,
                        win.market.slug,
                        status,
                        win.spent,
                        win.cash_returned,
                        realized,
                        spot_price
                            .map(|p| format!("{:.4}", p))
                            .unwrap_or_else(|| "".to_string()),
                        win.market
                            .price_to_beat
                            .map(|p| format!("{:.4}", p))
                            .unwrap_or_else(|| "".to_string()),
                        match (spot_price, win.market.price_to_beat) {
                            (Some(spot), Some(ptb)) if ptb > 0.0 && spot > ptb => "UP",
                            (Some(_), Some(ptb)) if ptb > 0.0 => "DOWN",
                            _ => "",
                        }
                    ),
                );
            }
            win.status = status.to_string();
            win.role = "PAST".to_string();
        }
        self.recalculate_equity();
    }

    pub fn recalculate_equity(&mut self) {
        let mut equity = self.available_cash;

        for win in self.windows.values() {
            if win.status != "CLOSED_TARGET"
                && win.status != "CLOSED_TIME"
                && win.status != "SKIPPED"
            {
                let up_val = win.up_shares * win.prices.up.bid;
                let dn_val = win.down_shares * win.prices.down.bid;
                equity += up_val + dn_val;
            }
        }

        self.equity = equity;
    }
}
