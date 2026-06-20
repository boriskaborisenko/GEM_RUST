use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};

const BYBIT_WS_URL: &str = "wss://stream.bybit.com/v5/public/linear";
const TRADE_BUFFER_MS: i64 = 10_000;
const CEX_VETO_VELOCITY_USD_PER_SEC: f64 = 8.0;

#[derive(Debug, Clone, Copy)]
struct TradeTick {
    timestamp_ms: i64,
    price: f64,
    qty: f64,
    is_buy: bool,
}

#[derive(Debug, Clone, Default)]
pub struct CexMicroSnapshot {
    pub trade_velocity_1s: Option<f64>,
    pub trade_velocity_3s: Option<f64>,
    pub trade_velocity_5s: Option<f64>,
    pub buy_sell_imbalance_3s: f64,
    pub last_trade_price: f64,
    pub lead_vs_chainlink_bps: Option<f64>,
    pub trade_count_3s: u32,
}

#[derive(Clone)]
pub struct CexMicroManager {
    trades: Arc<Mutex<VecDeque<TradeTick>>>,
    last_trade_price: Arc<Mutex<f64>>,
}

#[derive(Deserialize, Debug)]
struct BybitTradeResponse {
    topic: Option<String>,
    data: Option<Vec<BybitTradeData>>,
}

#[derive(Deserialize, Debug)]
struct BybitTradeData {
    #[serde(rename = "T")]
    timestamp_ms: i64,
    p: String,
    v: String,
    #[serde(rename = "S")]
    side: String,
}

impl CexMicroManager {
    pub fn new() -> Self {
        Self {
            trades: Arc::new(Mutex::new(VecDeque::with_capacity(512))),
            last_trade_price: Arc::new(Mutex::new(0.0)),
        }
    }

    pub fn start_tracking(&self) {
        let trades = self.trades.clone();
        let last_trade_price = self.last_trade_price.clone();

        tokio::spawn(async move {
            loop {
                let (ws_stream, _) = match connect_async(BYBIT_WS_URL).await {
                    Ok(val) => val,
                    Err(e) => {
                        eprintln!("[CEX Micro] WS connect error: {}. Retry 5s...", e);
                        tokio::time::sleep(Duration::from_secs(5)).await;
                        continue;
                    }
                };

                let (mut write, mut read) = ws_stream.split();
                let subscribe_msg = r#"{"op": "subscribe", "args": ["publicTrade.BTCUSDT"]}"#;
                if write
                    .send(Message::Text(subscribe_msg.into()))
                    .await
                    .is_err()
                {
                    continue;
                }

                println!("[CEX Micro] Subscribed to publicTrade.BTCUSDT");

                while let Some(message) = read.next().await {
                    match message {
                        Ok(Message::Text(text)) => {
                            if let Ok(response) = serde_json::from_str::<BybitTradeResponse>(&text)
                            {
                                if response.topic.as_deref() == Some("publicTrade.BTCUSDT") {
                                    if let Some(data_list) = response.data {
                                        let mut buf = trades.lock().unwrap();
                                        let mut last_px = last_trade_price.lock().unwrap();
                                        for tick in data_list {
                                            let price: f64 = tick.p.parse().unwrap_or(0.0);
                                            let qty: f64 = tick.v.parse().unwrap_or(0.0);
                                            if price <= 0.0 || qty <= 0.0 {
                                                continue;
                                            }
                                            *last_px = price;
                                            buf.push_back(TradeTick {
                                                timestamp_ms: tick.timestamp_ms,
                                                price,
                                                qty,
                                                is_buy: tick.side.eq_ignore_ascii_case("Buy"),
                                            });
                                        }
                                        let cutoff = get_now_ms() - TRADE_BUFFER_MS;
                                        while buf.front().is_some_and(|t| t.timestamp_ms < cutoff) {
                                            buf.pop_front();
                                        }
                                    }
                                }
                            }
                        }
                        Ok(Message::Ping(ping)) => {
                            let _ = write.send(Message::Pong(ping)).await;
                        }
                        Err(e) => {
                            eprintln!("[CEX Micro] WS error: {}", e);
                            break;
                        }
                        _ => {}
                    }
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });
    }

    pub fn snapshot(&self, chainlink_spot: Option<f64>) -> CexMicroSnapshot {
        let now = get_now_ms();
        let trades: Vec<TradeTick> = self.trades.lock().unwrap().iter().copied().collect();
        let last_trade_price = *self.last_trade_price.lock().unwrap();

        let velocity_1s = trade_velocity_usd_per_sec(&trades, now, 1_000);
        let velocity_3s = trade_velocity_usd_per_sec(&trades, now, 3_000);
        let velocity_5s = trade_velocity_usd_per_sec(&trades, now, 5_000);
        let (imbalance_3s, count_3s) = buy_sell_imbalance(&trades, now, 3_000);

        let lead_vs_chainlink_bps = match (chainlink_spot, last_trade_price) {
            (Some(cl), cex) if cl > 0.0 && cex > 0.0 => Some(((cex - cl) / cl) * 10_000.0),
            _ => None,
        };

        CexMicroSnapshot {
            trade_velocity_1s: velocity_1s,
            trade_velocity_3s: velocity_3s,
            trade_velocity_5s: velocity_5s,
            buy_sell_imbalance_3s: imbalance_3s,
            last_trade_price,
            lead_vs_chainlink_bps,
            trade_count_3s: count_3s,
        }
    }
}

impl Default for CexMicroManager {
    fn default() -> Self {
        Self::new()
    }
}

pub fn cex_velocity_against_side(side: &str, snapshot: &CexMicroSnapshot) -> bool {
    let side_sign = if side == "UP" { 1.0 } else { -1.0 };
    snapshot
        .trade_velocity_3s
        .map(|v| v * side_sign < -CEX_VETO_VELOCITY_USD_PER_SEC)
        .unwrap_or(false)
}

pub fn cex_vetoes_cheap_entry(side: &str, snapshot: &CexMicroSnapshot) -> bool {
    cex_velocity_against_side(side, snapshot)
}

fn trade_velocity_usd_per_sec(trades: &[TradeTick], now_ms: i64, window_ms: i64) -> Option<f64> {
    let cutoff = now_ms - window_ms;
    let mut notional = 0.0;
    let mut earliest = now_ms;
    let mut saw = false;
    for tick in trades.iter().rev() {
        if tick.timestamp_ms < cutoff {
            break;
        }
        saw = true;
        earliest = earliest.min(tick.timestamp_ms);
        let signed = if tick.is_buy { 1.0 } else { -1.0 };
        notional += signed * tick.price * tick.qty;
    }
    if !saw {
        return None;
    }
    let dt = ((now_ms - earliest) as f64 / 1000.0).max(0.001);
    Some(notional / dt)
}

fn buy_sell_imbalance(trades: &[TradeTick], now_ms: i64, window_ms: i64) -> (f64, u32) {
    let cutoff = now_ms - window_ms;
    let mut buy_vol = 0.0;
    let mut sell_vol = 0.0;
    let mut count = 0u32;
    for tick in trades.iter().rev() {
        if tick.timestamp_ms < cutoff {
            break;
        }
        count += 1;
        let usd = tick.price * tick.qty;
        if tick.is_buy {
            buy_vol += usd;
        } else {
            sell_vol += usd;
        }
    }
    let total = buy_vol + sell_vol;
    if total <= 0.0 {
        (0.0, count)
    } else {
        ((buy_vol - sell_vol) / total, count)
    }
}

fn get_now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
