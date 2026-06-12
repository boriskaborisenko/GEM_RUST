use crate::trader::TradeRecord;

#[derive(Debug, Clone, PartialEq, Default)]
pub struct HCloseStats {
    pub market_win: Option<bool>,
    pub salvaged: bool,
    pub salvage_win: bool,
    pub entry_price: Option<f64>,
    pub exit_price: Option<f64>,
}

pub fn derive_h_close_stats(
    trades: &[TradeRecord],
    entry_side: &str,
    winner: &str,
) -> HCloseStats {
    if entry_side.is_empty() {
        return HCloseStats::default();
    }

    let entry_price = trades
        .iter()
        .find(|t| t.trade_type == "BUY" && t.side == entry_side)
        .map(|t| t.price);

    let salvage_sell = trades.iter().rev().find(|t| {
        t.trade_type == "SELL" && t.side == entry_side && t.reason.starts_with("h_salvage")
    });

    let salvaged = salvage_sell.is_some();
    let exit_price = if salvaged {
        salvage_sell.map(|t| t.price)
    } else {
        trades.iter().find(|t| {
            t.trade_type == "REDEEM" && t.side == entry_side && t.shares > 0.0
        }).map(|t| t.price)
    };

    let salvage_win = match (entry_price, exit_price) {
        (Some(entry), Some(exit)) if salvaged => exit > entry,
        _ => false,
    };

    let market_win = if winner.is_empty() {
        None
    } else {
        Some(entry_side == winner)
    };

    HCloseStats {
        market_win,
        salvaged,
        salvage_win,
        entry_price,
        exit_price,
    }
}

pub fn log_h_window_close(
    log_dir: &str,
    window_id: usize,
    slug: &str,
    pnl: f64,
    h: &HCloseStats,
    cum_market_wins: u32,
    cum_market_losses: u32,
    cum_salvage_escapes: u32,
    cum_salvage_wins: u32,
) {
    use std::fs::OpenOptions;
    use std::io::Write;
    let path = std::path::Path::new(log_dir).join("h_session_stats.csv");
    let new_file = !path.exists();
    let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path) else {
        return;
    };
    if new_file {
        let _ = writeln!(
            file,
            "timestamp,window_id,slug,pnl,h_market_win,h_salvaged,h_salvage_win,entry_price,exit_price,cum_market_wins,cum_market_losses,cum_salvage_escapes,cum_salvage_wins"
        );
    }
    let _ = writeln!(
        file,
        "{},{},{},{:.4},{},{},{},{:.4},{:.4},{},{},{},{}",
        crate::client::get_now_ms(),
        window_id,
        slug,
        pnl,
        h.market_win
            .map(|v| v.to_string())
            .unwrap_or_default(),
        h.salvaged,
        h.salvage_win,
        h.entry_price
            .map(|p| format!("{:.4}", p))
            .unwrap_or_default(),
        h.exit_price
            .map(|p| format!("{:.4}", p))
            .unwrap_or_default(),
        cum_market_wins,
        cum_market_losses,
        cum_salvage_escapes,
        cum_salvage_wins,
    );
}

pub fn format_h_session_line(
    market_wins: u32,
    market_losses: u32,
    salvage_escapes: u32,
    salvage_wins: u32,
) -> String {
    let market_total = market_wins + market_losses;
    let market_wr = if market_total > 0 {
        (market_wins as f64 / market_total as f64) * 100.0
    } else {
        0.0
    };
    let salvage_wr = if salvage_escapes > 0 {
        (salvage_wins as f64 / salvage_escapes as f64) * 100.0
    } else {
        0.0
    };
    format!(
        "H Extra: Market {}/{} ({:.0}%) | Salvaged {}/{} profitable exit ({:.0}%)",
        market_wins,
        market_total,
        market_wr,
        salvage_wins,
        salvage_escapes,
        salvage_wr
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buy(side: &str, price: f64) -> TradeRecord {
        TradeRecord {
            timestamp: 0,
            trade_type: "BUY".into(),
            side: side.into(),
            reason: "h_entry".into(),
            price,
            shares: 10.0,
            usd_value: price * 10.0,
            available_cash_after: 0.0,
        }
    }

    fn salvage(side: &str, price: f64) -> TradeRecord {
        TradeRecord {
            timestamp: 1,
            trade_type: "SELL".into(),
            side: side.into(),
            reason: "h_salvage_otm".into(),
            price,
            shares: 10.0,
            usd_value: price * 10.0,
            available_cash_after: 0.0,
        }
    }

    #[test]
    fn salvage_profit_counts_as_salvage_win() {
        let s = derive_h_close_stats(
            &[buy("DOWN", 0.40), salvage("DOWN", 0.41)],
            "DOWN",
            "UP",
        );
        assert_eq!(s.market_win, Some(false));
        assert!(s.salvaged);
        assert!(s.salvage_win);
    }

    #[test]
    fn salvage_loss_not_salvage_win() {
        let s = derive_h_close_stats(
            &[buy("UP", 0.40), salvage("UP", 0.39)],
            "UP",
            "UP",
        );
        assert_eq!(s.market_win, Some(true));
        assert!(s.salvaged);
        assert!(!s.salvage_win);
    }

    #[test]
    fn redeem_hold_no_salvage() {
        let s = derive_h_close_stats(
            &[buy("DOWN", 0.40)],
            "DOWN",
            "DOWN",
        );
        assert!(!s.salvaged);
        assert!(!s.salvage_win);
    }
}
