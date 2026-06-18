use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};

const BYBIT_WS_URL: &str = "wss://stream.bybit.com/v5/public/linear";
const ATR_PERIOD: usize = 14;
/// If no confirmed 1m candle for this long, force REST refresh.
const ATR_STALE_SECS: u64 = 120;
const ATR_STALE_CHECK_SECS: u64 = 90;
type DynError = Box<dyn std::error::Error + Send + Sync>;

#[derive(Debug, Default, Clone)]
struct Bar {
    high: f64,
    low: f64,
    close: f64,
}

/// Maps CLI asset to Bybit/Binance linear spot symbol for 1m ATR.
pub fn atr_symbol_for_asset(asset: &str) -> String {
    match asset.to_uppercase().as_str() {
        "ETH" => "ETHUSDT".to_string(),
        "SOL" => "SOLUSDT".to_string(),
        "XRP" => "XRPUSDT".to_string(),
        "DOGE" => "DOGEUSDT".to_string(),
        _ => "BTCUSDT".to_string(),
    }
}

#[derive(Clone)]
pub struct VolatilityManager {
    symbol: String,
    asset: String,
    current_atr: Arc<Mutex<f64>>,
    bar_history: Arc<Mutex<Vec<Bar>>>,
    last_atr_tick: Arc<Mutex<Option<Instant>>>,
}

#[derive(Deserialize, Debug)]
struct BybitResponse {
    topic: Option<String>,
    data: Option<Vec<BybitBarData>>,
}

#[derive(Deserialize, Debug)]
struct BybitBarData {
    high: String,
    low: String,
    close: String,
    confirm: bool,
}

impl VolatilityManager {
    pub fn new(asset: &str) -> Self {
        let asset = asset.to_uppercase();
        let symbol = atr_symbol_for_asset(&asset);
        Self {
            symbol,
            asset,
            current_atr: Arc::new(Mutex::new(0.0)),
            bar_history: Arc::new(Mutex::new(Vec::new())),
            last_atr_tick: Arc::new(Mutex::new(None)),
        }
    }

    pub fn asset(&self) -> &str {
        &self.asset
    }

    pub fn symbol(&self) -> &str {
        &self.symbol
    }

    /// Current ATR(14) in quote currency (USD) for this process asset.
    pub fn get_current_atr(&self) -> f64 {
        *self.current_atr.lock().unwrap()
    }

    fn mark_atr_fresh(&self) {
        *self.last_atr_tick.lock().unwrap() = Some(Instant::now());
    }

    fn apply_bars(bars: Vec<Bar>) -> (f64, Vec<Bar>) {
        let mut history = Vec::new();
        let mut calculated_atr = 0.0;
        for bar in bars {
            if let Some(new_atr) = Self::calculate_next_atr(&mut history, bar) {
                calculated_atr = new_atr;
            }
        }
        (calculated_atr, history)
    }

    fn commit_atr(&self, calculated_atr: f64, history: Vec<Bar>, label: &str, source: &str) {
        if calculated_atr <= 0.0 {
            return;
        }
        *self.current_atr.lock().unwrap() = calculated_atr;
        *self.bar_history.lock().unwrap() = history;
        self.mark_atr_fresh();
        println!(
            "[ATR {}] {} {} ATR(14): {:.4} USD",
            self.asset, label, source, calculated_atr
        );
    }

    async fn fetch_rest_bars(client: &reqwest::Client, symbol: &str) -> Result<(&'static str, Vec<Bar>), DynError> {
        match Self::fetch_bybit_bars(client, symbol).await {
            Ok(bars) => Ok(("Bybit REST", bars)),
            Err(bybit_err) => {
                eprintln!(
                    "[ATR {}] Bybit REST недоступен: {}. Пробуем Binance REST fallback...",
                    symbol, bybit_err
                );
                match Self::fetch_binance_bars(client, symbol).await {
                    Ok(bars) => Ok(("Binance REST fallback", bars)),
                    Err(binance_err) => Err(format!(
                        "ATR REST failed for {}. Bybit: {} | Binance: {}",
                        symbol, bybit_err, binance_err
                    )
                    .into()),
                }
            }
        }
    }

    /// Reload ATR(14) and bar history from REST (warmup, reconnect, stale watchdog).
    pub async fn refresh_from_rest(&self, label: &str) -> Result<f64, DynError> {
        let client = reqwest::Client::builder()
            .user_agent("GEM_RUST ATR refresh/0.2")
            .build()?;

        let (source, bars) = Self::fetch_rest_bars(&client, &self.symbol).await?;
        let (calculated_atr, history) = Self::apply_bars(bars);
        if calculated_atr <= 0.0 {
            return Err(format!("ATR REST refresh produced zero for {}", self.symbol).into());
        }
        self.commit_atr(calculated_atr, history, label, source);
        Ok(calculated_atr)
    }

    pub async fn warmup_from_rest(&self) -> Result<(), DynError> {
        self.refresh_from_rest("Warmup").await?;
        Ok(())
    }

    async fn fetch_bybit_bars(client: &reqwest::Client, symbol: &str) -> Result<Vec<Bar>, DynError> {
        let url = format!(
            "https://api.bybit.com/v5/market/kline?category=linear&symbol={}&interval=1&limit=50",
            symbol
        );

        let resp = client.get(url).send().await?;
        if !resp.status().is_success() {
            return Err(format!("Bybit REST error: {}", resp.status()).into());
        }

        let json: serde_json::Value = resp.json().await?;
        if json["retCode"].as_i64() != Some(0) {
            return Err("Bybit REST returned non-zero retCode".into());
        }

        let list = json["result"]["list"]
            .as_array()
            .ok_or("result.list not found in Bybit response")?;

        let mut bars = vec![];
        for item in list.iter().rev() {
            let item_arr = item.as_array().ok_or("Bybit kline item is not an array")?;
            if item_arr.len() < 5 {
                continue;
            }
            bars.push(Bar {
                high: item_arr[2].as_str().unwrap_or("0.0").parse()?,
                low: item_arr[3].as_str().unwrap_or("0.0").parse()?,
                close: item_arr[4].as_str().unwrap_or("0.0").parse()?,
            });
        }

        if bars.len() <= ATR_PERIOD {
            return Err(format!("Bybit returned too few candles: {}", bars.len()).into());
        }

        Ok(bars)
    }

    async fn fetch_binance_bars(client: &reqwest::Client, symbol: &str) -> Result<Vec<Bar>, DynError> {
        let url = format!(
            "https://api.binance.com/api/v3/klines?symbol={}&interval=1m&limit=50",
            symbol
        );

        let resp = client.get(url).send().await?;
        if !resp.status().is_success() {
            return Err(format!("Binance REST error: {}", resp.status()).into());
        }

        let list: serde_json::Value = resp.json().await?;
        let arr = list
            .as_array()
            .ok_or("Binance kline response is not an array")?;

        let mut bars = vec![];
        for item in arr {
            let item_arr = item
                .as_array()
                .ok_or("Binance kline item is not an array")?;
            if item_arr.len() < 5 {
                continue;
            }
            bars.push(Bar {
                high: item_arr[2].as_str().unwrap_or("0.0").parse()?,
                low: item_arr[3].as_str().unwrap_or("0.0").parse()?,
                close: item_arr[4].as_str().unwrap_or("0.0").parse()?,
            });
        }

        if bars.len() <= ATR_PERIOD {
            return Err(format!("Binance returned too few candles: {}", bars.len()).into());
        }

        Ok(bars)
    }

    fn start_stale_watchdog(self: &Arc<Self>) {
        let mgr = Arc::clone(self);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(ATR_STALE_CHECK_SECS));
            loop {
                interval.tick().await;
                let stale = {
                    let last = mgr.last_atr_tick.lock().unwrap();
                    match *last {
                        None => true,
                        Some(t) => t.elapsed() > Duration::from_secs(ATR_STALE_SECS),
                    }
                };
                if !stale {
                    continue;
                }
                eprintln!(
                    "[ATR {}] stale > {}s — forcing REST refresh",
                    mgr.asset, ATR_STALE_SECS
                );
                if let Err(e) = mgr.refresh_from_rest("StaleWatchdog").await {
                    eprintln!("[ATR {}] stale REST refresh failed: {}", mgr.asset, e);
                }
            }
        });
    }

    pub fn start_tracking(self: &Arc<Self>) {
        self.start_stale_watchdog();

        let mgr = Arc::clone(self);
        let current_atr_clone = self.current_atr.clone();
        let bar_history_clone = self.bar_history.clone();
        let last_atr_tick_clone = self.last_atr_tick.clone();
        let symbol = self.symbol.clone();
        let asset = self.asset.clone();
        let ws_topic = format!("kline.1.{}", symbol);

        tokio::spawn(async move {
            loop {
                if let Err(e) = mgr.refresh_from_rest("Reconnect").await {
                    eprintln!(
                        "[ATR {}] REST refresh before WS connect failed: {} — retry in 5s",
                        asset, e
                    );
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    continue;
                }

                println!("[ATR {}] Подключение к WebSocket Bybit...", symbol);
                let (ws_stream, _) = match connect_async(BYBIT_WS_URL).await {
                    Ok(val) => val,
                    Err(e) => {
                        eprintln!(
                            "[ATR {}] Ошибка подключения: {}. Повтор через 5с...",
                            symbol, e
                        );
                        tokio::time::sleep(Duration::from_secs(5)).await;
                        continue;
                    }
                };

                let (mut write, mut read) = ws_stream.split();

                let subscribe_msg = format!(
                    r#"{{"op": "subscribe", "args": ["{}"]}}"#,
                    ws_topic
                );
                if let Err(e) = write.send(Message::Text(subscribe_msg.into())).await {
                    eprintln!("[ATR {}] Ошибка отправки подписки: {}", symbol, e);
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    continue;
                }

                println!("[ATR {}] Подписка на {}", asset, ws_topic);

                while let Some(message) = read.next().await {
                    match message {
                        Ok(Message::Text(text)) => {
                            if let Ok(response) = serde_json::from_str::<BybitResponse>(&text) {
                                if let (Some(topic), Some(data_list)) =
                                    (response.topic, response.data)
                                {
                                    if topic == ws_topic && !data_list.is_empty() {
                                        let candle = &data_list[0];

                                        if candle.confirm {
                                            let high: f64 = candle.high.parse().unwrap_or(0.0);
                                            let low: f64 = candle.low.parse().unwrap_or(0.0);
                                            let close: f64 = candle.close.parse().unwrap_or(0.0);

                                            let new_bar = Bar { high, low, close };

                                            let new_atr = {
                                                let mut history =
                                                    bar_history_clone.lock().unwrap();
                                                Self::calculate_next_atr(&mut history, new_bar)
                                            };

                                            if let Some(new_atr) = new_atr {
                                                *current_atr_clone.lock().unwrap() = new_atr;
                                                *last_atr_tick_clone.lock().unwrap() =
                                                    Some(Instant::now());
                                                println!(
                                                    "[ATR {}] WS ATR(14): {:.4} USD",
                                                    asset, new_atr
                                                );
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        Ok(Message::Ping(ping)) => {
                            let _ = write.send(Message::Pong(ping)).await;
                        }
                        Err(e) => {
                            eprintln!("[ATR {}] Сбой WebSocket: {} — reconnect", symbol, e);
                            break;
                        }
                        _ => {}
                    }
                }
                eprintln!(
                    "[ATR {}] WebSocket disconnected — REST refresh + reconnect",
                    asset
                );
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });
    }

    fn calculate_next_atr(history: &mut Vec<Bar>, new_bar: Bar) -> Option<f64> {
        if history.is_empty() {
            let _tr = new_bar.high - new_bar.low;
            history.push(new_bar);
            return None;
        }

        let prev_bar = history.last().unwrap();

        let tr1 = new_bar.high - new_bar.low;
        let tr2 = (new_bar.high - prev_bar.close).abs();
        let tr3 = (new_bar.low - prev_bar.close).abs();
        let current_tr = tr1.max(tr2).max(tr3);

        history.push(new_bar);

        if history.len() > ATR_PERIOD + 1 {
            history.remove(0);
        }

        if history.len() <= ATR_PERIOD {
            return None;
        }

        let mut sum_tr = current_tr;

        for i in 1..history.len() - 1 {
            let h = history[i].high;
            let l = history[i].low;
            let pc = history[i - 1].close;
            let tr = (h - l).max((h - pc).abs()).max((l - pc).abs());
            sum_tr += tr;
        }

        let atr = sum_tr / ATR_PERIOD as f64;
        Some(atr)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atr_symbol_per_asset() {
        assert_eq!(atr_symbol_for_asset("BTC"), "BTCUSDT");
        assert_eq!(atr_symbol_for_asset("eth"), "ETHUSDT");
        assert_eq!(atr_symbol_for_asset("SOL"), "SOLUSDT");
        assert_eq!(atr_symbol_for_asset("XRP"), "XRPUSDT");
        assert_eq!(atr_symbol_for_asset("DOGE"), "DOGEUSDT");
    }

    #[test]
    fn apply_bars_builds_history_and_atr() {
        let bars: Vec<Bar> = (0..20)
            .map(|i| {
                let base = 100.0 + i as f64;
                Bar {
                    high: base + 2.0,
                    low: base - 1.0,
                    close: base + 1.0,
                }
            })
            .collect();
        let (atr, history) = VolatilityManager::apply_bars(bars);
        assert!(atr > 0.0);
        assert!(history.len() > ATR_PERIOD);
    }
}
