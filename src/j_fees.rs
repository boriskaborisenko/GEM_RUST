//! Polymarket crypto fee model for J endgame clip math (paper).

pub const DEFAULT_CRYPTO_FEE_RATE_BPS: f64 = 70.0;

/// Fee on notional for a buy/sell leg (simplified: bps × price × shares).
pub fn leg_fee_usd(price: f64, shares: f64, fee_rate_bps: f64) -> f64 {
    if price <= 0.0 || shares <= 0.0 {
        return 0.0;
    }
    price * shares * (fee_rate_bps / 10_000.0)
}

/// Net PnL per $1 notional clip: buy at `entry_price`, redeem at $1/share if win.
pub fn endgame_buy_hold_net(entry_price: f64, clip_usd: f64, won: bool, fee_rate_bps: f64) -> f64 {
    if entry_price <= 0.0 || clip_usd <= 0.0 {
        return 0.0;
    }
    let shares = clip_usd / entry_price;
    let buy_fee = leg_fee_usd(entry_price, shares, fee_rate_bps);
    if !won {
        return -clip_usd - buy_fee;
    }
    let redeem_gross = shares * 1.0;
    let redeem_fee = leg_fee_usd(1.0, shares, fee_rate_bps);
    redeem_gross - clip_usd - buy_fee - redeem_fee
}

/// Round-trip scalp: buy at buy_price, sell at sell_price (same shares).
pub fn endgame_scalp_net(buy_price: f64, sell_price: f64, clip_usd: f64, fee_rate_bps: f64) -> f64 {
    if buy_price <= 0.0 || sell_price <= buy_price || clip_usd <= 0.0 {
        return 0.0;
    }
    let shares = clip_usd / buy_price;
    let buy_fee = leg_fee_usd(buy_price, shares, fee_rate_bps);
    let sell_gross = shares * sell_price;
    let sell_fee = leg_fee_usd(sell_price, shares, fee_rate_bps);
    sell_gross - clip_usd - buy_fee - sell_fee
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EndgameFeeComparison {
    pub buy_98_net: f64,
    pub buy_99_net: f64,
    pub scalp_98_99_net: f64,
}

pub fn compare_endgame_clips(clip_usd: f64, fee_rate_bps: f64) -> EndgameFeeComparison {
    EndgameFeeComparison {
        buy_98_net: endgame_buy_hold_net(0.98, clip_usd, true, fee_rate_bps),
        buy_99_net: endgame_buy_hold_net(0.99, clip_usd, true, fee_rate_bps),
        scalp_98_99_net: endgame_scalp_net(0.98, 0.99, clip_usd, fee_rate_bps),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buy_98_better_than_99_after_fees() {
        let c = compare_endgame_clips(1.0, DEFAULT_CRYPTO_FEE_RATE_BPS);
        assert!(c.buy_98_net > c.buy_99_net);
        assert!(c.buy_98_net > 0.0);
    }

    #[test]
    fn scalp_loses_to_fees() {
        let c = compare_endgame_clips(1.0, DEFAULT_CRYPTO_FEE_RATE_BPS);
        assert!(c.scalp_98_99_net < c.buy_98_net);
        assert!(c.scalp_98_99_net < 0.01);
    }
}
