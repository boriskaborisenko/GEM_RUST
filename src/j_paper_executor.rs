use crate::client::PricesState;
use crate::strategy::{OrderOperation, OrderSignal, OrderType};
use crate::trader::Portfolio;

const J_MIN_NOTIONAL_USD: f64 = 1.0;
const EPS: f64 = 1e-9;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JPaperReject {
    BadSide,
    BadPrice,
    BelowMinNotional,
    InsufficientCash,
    NoShares,
    NoExecutableAsk,
    NoExecutableBid,
}

impl JPaperReject {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::BadSide => "bad_side",
            Self::BadPrice => "bad_price",
            Self::BelowMinNotional => "below_min_notional",
            Self::InsufficientCash => "insufficient_cash",
            Self::NoShares => "no_shares",
            Self::NoExecutableAsk => "no_executable_ask",
            Self::NoExecutableBid => "no_executable_bid",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct JPaperExecution {
    pub executed: bool,
    pub reject: Option<JPaperReject>,
}

impl JPaperExecution {
    fn filled() -> Self {
        Self {
            executed: true,
            reject: None,
        }
    }

    fn rejected(reason: JPaperReject) -> Self {
        Self {
            executed: false,
            reject: Some(reason),
        }
    }
}

pub fn execute_j_paper_signal(
    port: &mut Portfolio,
    window_number: usize,
    prices: &PricesState,
    sig: &OrderSignal,
) -> JPaperExecution {
    match sig.operation() {
        OrderOperation::Buy => execute_j_paper_buy(port, window_number, prices, sig),
        OrderOperation::Sell => execute_j_paper_sell(port, window_number, prices, sig),
    }
}

fn execute_j_paper_buy(
    port: &mut Portfolio,
    window_number: usize,
    prices: &PricesState,
    sig: &OrderSignal,
) -> JPaperExecution {
    if sig.amount + EPS < J_MIN_NOTIONAL_USD {
        return JPaperExecution::rejected(JPaperReject::BelowMinNotional);
    }
    if sig.price <= 0.0 {
        return JPaperExecution::rejected(JPaperReject::BadPrice);
    }
    if port.available_cash + EPS < sig.amount {
        return JPaperExecution::rejected(JPaperReject::InsufficientCash);
    }

    let Some(best_ask) = best_ask(&sig.side, prices) else {
        return JPaperExecution::rejected(JPaperReject::BadSide);
    };
    if best_ask <= 0.0 {
        return JPaperExecution::rejected(JPaperReject::NoExecutableAsk);
    }

    let execution_price = match sig.order_type {
        OrderType::Market => best_ask,
        OrderType::Limit => {
            if best_ask > sig.price + EPS {
                return JPaperExecution::rejected(JPaperReject::NoExecutableAsk);
            }
            best_ask
        }
    };

    if port
        .execute_buy(
            window_number,
            &sig.side,
            sig.amount,
            execution_price,
            &sig.reason,
        )
        .is_some()
    {
        JPaperExecution::filled()
    } else {
        JPaperExecution::rejected(JPaperReject::BadPrice)
    }
}

fn execute_j_paper_sell(
    port: &mut Portfolio,
    window_number: usize,
    prices: &PricesState,
    sig: &OrderSignal,
) -> JPaperExecution {
    if sig.amount <= 0.0 {
        return JPaperExecution::rejected(JPaperReject::NoShares);
    }

    let Some(best_bid) = best_bid(&sig.side, prices) else {
        return JPaperExecution::rejected(JPaperReject::BadSide);
    };
    if best_bid <= 0.0 {
        return JPaperExecution::rejected(JPaperReject::NoExecutableBid);
    }

    let execution_price = match sig.order_type {
        OrderType::Market => best_bid,
        OrderType::Limit => {
            if sig.price <= 0.0 {
                return JPaperExecution::rejected(JPaperReject::BadPrice);
            }
            if best_bid + EPS < sig.price {
                return JPaperExecution::rejected(JPaperReject::NoExecutableBid);
            }
            sig.price
        }
    };
    if sig.amount * execution_price + EPS < J_MIN_NOTIONAL_USD {
        return JPaperExecution::rejected(JPaperReject::BelowMinNotional);
    }

    if port
        .execute_sell(
            window_number,
            &sig.side,
            sig.amount,
            execution_price,
            &sig.reason,
        )
        .is_some()
    {
        JPaperExecution::filled()
    } else {
        JPaperExecution::rejected(JPaperReject::NoShares)
    }
}

fn best_ask(side: &str, prices: &PricesState) -> Option<f64> {
    match side {
        "UP" => Some(book_or_top_ask(prices.up.book.best_ask(), prices.up.ask)),
        "DOWN" => Some(book_or_top_ask(
            prices.down.book.best_ask(),
            prices.down.ask,
        )),
        _ => None,
    }
}

fn best_bid(side: &str, prices: &PricesState) -> Option<f64> {
    match side {
        "UP" => Some(book_or_top_bid(prices.up.book.best_bid(), prices.up.bid)),
        "DOWN" => Some(book_or_top_bid(
            prices.down.book.best_bid(),
            prices.down.bid,
        )),
        _ => None,
    }
}

fn book_or_top_ask(book_ask: f64, top_ask: f64) -> f64 {
    if book_ask > 0.0 {
        book_ask
    } else {
        top_ask
    }
}

fn book_or_top_bid(book_bid: f64, top_bid: f64) -> f64 {
    if book_bid > 0.0 {
        book_bid
    } else {
        top_bid
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{ContractPrices, MarketWindow, TokenInfo, TokensMap};

    fn sample_market() -> MarketWindow {
        MarketWindow {
            id: "m".into(),
            slug: "m".into(),
            question: "m".into(),
            asset: "BTC".into(),
            interval: "5m".into(),
            start_time: "2026-06-21T00:00:00Z".into(),
            end_time: "2026-06-21T00:05:00Z".into(),
            price_to_beat: Some(60_000.0),
            tokens: TokensMap {
                up: TokenInfo {
                    token_id: "up".into(),
                    outcome_name: "Up".into(),
                },
                down: TokenInfo {
                    token_id: "down".into(),
                    outcome_name: "Down".into(),
                },
            },
        }
    }

    fn prices(up_bid: f64, up_ask: f64, down_bid: f64, down_ask: f64) -> PricesState {
        PricesState {
            up: ContractPrices::top(up_bid, up_ask),
            down: ContractPrices::top(down_bid, down_ask),
        }
    }

    fn portfolio_with_window(bank: f64, prices: PricesState) -> Portfolio {
        let log_dir = std::env::temp_dir()
            .join(format!(
                "gem_rust_j_paper_executor_test_{}",
                std::process::id()
            ))
            .to_string_lossy()
            .to_string();
        let mut port = Portfolio::new_with_log_dir(bank, log_dir);
        port.get_or_create_window_state(1, "CURRENT", &sample_market())
            .prices = prices;
        port
    }

    #[test]
    fn j_limit_buy_rejects_when_best_ask_is_above_limit() {
        let p = prices(0.54, 0.56, 0.43, 0.44);
        let mut port = portfolio_with_window(10.0, p.clone());
        let sig = OrderSignal::buy("UP", OrderType::Limit, 2.0, 0.55, "j_test_limit_buy");

        let res = execute_j_paper_signal(&mut port, 1, &p, &sig);

        assert!(!res.executed);
        assert_eq!(res.reject, Some(JPaperReject::NoExecutableAsk));
        assert_eq!(port.windows.get(&1).unwrap().up_shares, 0.0);
    }

    #[test]
    fn j_limit_buy_fills_when_best_ask_is_at_limit() {
        let p = prices(0.54, 0.55, 0.43, 0.44);
        let mut port = portfolio_with_window(10.0, p.clone());
        let sig = OrderSignal::buy("UP", OrderType::Limit, 2.0, 0.55, "j_test_limit_buy");

        let res = execute_j_paper_signal(&mut port, 1, &p, &sig);

        assert!(res.executed);
        assert!(port.windows.get(&1).unwrap().up_shares > 3.63);
        assert!((port.available_cash - 8.0).abs() < 1e-9);
    }

    #[test]
    fn j_limit_buy_fills_at_best_ask_below_limit() {
        let p = prices(0.49, 0.50, 0.49, 0.50);
        let mut port = portfolio_with_window(10.0, p.clone());
        let sig = OrderSignal::buy("UP", OrderType::Limit, 2.0, 0.88, "j_test_limit_buy");

        let res = execute_j_paper_signal(&mut port, 1, &p, &sig);

        assert!(res.executed);
        assert!((port.windows.get(&1).unwrap().up_shares - 4.0).abs() < 1e-9);
        assert!((port.available_cash - 8.0).abs() < 1e-9);
    }

    #[test]
    fn j_market_buy_uses_current_ask_not_signal_price() {
        let p = prices(0.61, 0.62, 0.37, 0.38);
        let mut port = portfolio_with_window(10.0, p.clone());
        let sig = OrderSignal::buy("UP", OrderType::Market, 2.0, 0.88, "j_test_market_buy");

        let res = execute_j_paper_signal(&mut port, 1, &p, &sig);

        assert!(res.executed);
        assert!((port.windows.get(&1).unwrap().up_shares - (2.0 / 0.62)).abs() < 1e-9);
        assert!((port.available_cash - 8.0).abs() < 1e-9);
    }

    #[test]
    fn j_market_sell_uses_current_bid() {
        let p = prices(0.61, 0.62, 0.38, 0.39);
        let mut port = portfolio_with_window(10.0, p.clone());
        assert!(port.execute_buy(1, "UP", 2.0, 0.50, "seed").is_some());
        let sig = OrderSignal::sell("UP", OrderType::Market, 2.0, 0.50, "j_test_market_sell");

        let res = execute_j_paper_signal(&mut port, 1, &p, &sig);

        assert!(res.executed);
        assert!((port.windows.get(&1).unwrap().cash_returned - 1.22).abs() < 1e-9);
    }

    #[test]
    fn j_limit_sell_rejects_when_best_bid_is_below_limit() {
        let p = prices(0.59, 0.62, 0.38, 0.39);
        let mut port = portfolio_with_window(10.0, p.clone());
        assert!(port.execute_buy(1, "UP", 2.0, 0.50, "seed").is_some());
        let sig = OrderSignal::sell("UP", OrderType::Limit, 2.0, 0.60, "j_test_limit_sell");

        let res = execute_j_paper_signal(&mut port, 1, &p, &sig);

        assert!(!res.executed);
        assert_eq!(res.reject, Some(JPaperReject::NoExecutableBid));
        assert_eq!(port.windows.get(&1).unwrap().up_shares, 4.0);
    }
}
