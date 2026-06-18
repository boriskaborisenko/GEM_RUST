use serde_json::Value;

#[derive(Debug, Clone, Copy, Default)]
pub struct BookLevel {
    pub price: f64,
    pub size: f64,
}

#[derive(Debug, Clone, Default)]
pub struct SideBook {
    pub bids: Vec<BookLevel>,
    pub asks: Vec<BookLevel>,
}

impl SideBook {
    pub fn best_bid(&self) -> f64 {
        self.bids.first().map(|l| l.price).unwrap_or(0.0)
    }

    pub fn best_ask(&self) -> f64 {
        self.asks.first().map(|l| l.price).unwrap_or(0.0)
    }
}

#[derive(Debug, Clone, Default)]
pub struct MarketBooks {
    pub up: SideBook,
    pub down: SideBook,
}

pub fn parse_levels(arr: Option<&Vec<Value>>) -> Vec<BookLevel> {
    let mut levels = vec![];
    if let Some(items) = arr {
        for v in items {
            let price = v
                .get("price")
                .and_then(|p| p.as_str())
                .and_then(|p| p.parse().ok())
                .unwrap_or(0.0);
            let size = v
                .get("size")
                .and_then(|s| s.as_str())
                .and_then(|s| s.parse().ok())
                .unwrap_or(0.0);
            if price > 0.0 && size > 0.0 {
                levels.push(BookLevel { price, size });
            }
        }
    }
    levels
}

pub fn sort_bids(mut levels: Vec<BookLevel>) -> Vec<BookLevel> {
    levels.sort_by(|a, b| {
        b.price
            .partial_cmp(&a.price)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    levels
}

pub fn sort_asks(mut levels: Vec<BookLevel>) -> Vec<BookLevel> {
    levels.sort_by(|a, b| {
        a.price
            .partial_cmp(&b.price)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    levels
}

/// Paper fill: limit buy up to `max_usd` when asks are at or below `limit_price`.
pub fn simulate_limit_buy_fill(asks: &[BookLevel], limit_price: f64, max_usd: f64) -> Option<(f64, f64)> {
    if limit_price <= 0.0 || max_usd <= 0.0 {
        return None;
    }
    let mut remaining_usd = max_usd;
    let mut total_shares = 0.0;
    let mut spent = 0.0;

    for level in asks {
        if level.price > limit_price {
            break;
        }
        let max_shares_at_level = remaining_usd / level.price;
        let fill_shares = level.size.min(max_shares_at_level);
        if fill_shares <= 0.0 {
            continue;
        }
        let cost = fill_shares * level.price;
        total_shares += fill_shares;
        spent += cost;
        remaining_usd -= cost;
        if remaining_usd < 0.01 {
            break;
        }
    }

    if total_shares <= 0.0 {
        return None;
    }
    Some((total_shares, spent / total_shares))
}

/// Taker buy at best ask up to `max_pay` (join the green pluses).
pub fn simulate_taker_buy_fill(
    asks: &[BookLevel],
    best_ask: f64,
    max_pay: f64,
    clip_usd: f64,
) -> Option<(f64, f64)> {
    if best_ask <= 0.0 || best_ask > max_pay {
        return None;
    }
    if !asks.is_empty() {
        return simulate_limit_buy_fill(asks, max_pay, clip_usd);
    }
    Some((clip_usd / best_ask, best_ask))
}

pub fn ask_depth_usd(asks: &[BookLevel], max_pay: f64) -> f64 {
    asks.iter()
        .filter(|l| l.price <= max_pay)
        .map(|l| l.price * l.size)
        .sum()
}

/// Paper: remove filled size from ask levels after a simulated buy.
pub fn apply_fill_to_asks(asks: &mut Vec<BookLevel>, max_pay: f64, clip_usd: f64) -> Option<(f64, f64)> {
    let fill = simulate_limit_buy_fill(asks, max_pay, clip_usd)?;
    let mut remaining_shares = fill.0;
    for level in asks.iter_mut() {
        if level.price > max_pay || remaining_shares <= 0.0 {
            break;
        }
        let take = level.size.min(remaining_shares);
        level.size -= take;
        remaining_shares -= take;
    }
    asks.retain(|l| l.size > 1e-9);
    Some(fill)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limit_fill_walks_asks() {
        let asks = vec![
            BookLevel {
                price: 0.97,
                size: 2.0,
            },
            BookLevel {
                price: 0.98,
                size: 5.0,
            },
        ];
        let (shares, avg) = simulate_limit_buy_fill(&asks, 0.98, 1.0).unwrap();
        assert!(shares > 0.0);
        assert!(avg <= 0.98);
    }

    #[test]
    fn no_fill_if_ask_above_limit() {
        let asks = vec![BookLevel {
            price: 0.99,
            size: 10.0,
        }];
        assert!(simulate_limit_buy_fill(&asks, 0.98, 1.0).is_none());
    }
}
