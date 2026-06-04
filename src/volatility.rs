use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};

const BYBIT_WS_URL: &str = "wss://stream.bybit.com/v5/public/linear";
const ATR_PERIOD: usize = 14;
type DynError = Box<dyn std::error::Error + Send + Sync>;

#[derive(Debug, Default, Clone)]
struct Bar {
    high: f64,
    low: f64,
    close: f64,
}

// Потокобезопасный менеджер волатильности
#[derive(Clone)]
pub struct VolatilityManager {
    current_atr: Arc<Mutex<f64>>,
}

// Структуры для десериализации JSON от Bybit WS
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
    confirm: bool, // true означает, что минутная свеча закрыта
}

impl VolatilityManager {
    pub fn new() -> Self {
        Self {
            current_atr: Arc::new(Mutex::new(0.0)),
        }
    }

    /// Возвращает текущее значение ATR(14) в долларах BTC.
    /// Если данных еще недостаточно, вернет 0.0.
    pub fn get_current_atr(&self) -> f64 {
        *self.current_atr.lock().unwrap()
    }

    /// Загружает исторические свечи через REST API Bybit для мгновенного прогрева ATR при старте
    pub async fn warmup_from_rest(&self) -> Result<(), DynError> {
        let client = reqwest::Client::builder()
            .user_agent("GEM_RUST ATR warmup/0.1")
            .build()?;

        let (source, temp_history) = match Self::fetch_bybit_bars(&client).await {
            Ok(bars) => ("Bybit REST", bars),
            Err(bybit_err) => {
                eprintln!(
                    "[ATR Warmup] Bybit REST недоступен: {}. Пробуем Binance REST fallback...",
                    bybit_err
                );
                match Self::fetch_binance_bars(&client).await {
                    Ok(bars) => ("Binance REST fallback", bars),
                    Err(binance_err) => {
                        return Err(format!(
                            "ATR REST warmup failed. Bybit: {} | Binance: {}",
                            bybit_err, binance_err
                        )
                        .into());
                    }
                }
            }
        };

        // Вычисляем начальный ATR по истории
        let mut history = vec![];
        let mut calculated_atr = 0.0;

        for bar in temp_history {
            if let Some(new_atr) = Self::calculate_next_atr(&mut history, bar) {
                calculated_atr = new_atr;
            }
        }

        // Записываем прогретый ATR в стейт
        if calculated_atr > 0.0 {
            let mut atr_lock = self.current_atr.lock().unwrap();
            *atr_lock = calculated_atr;
            println!(
                "[ATR Warmup] Успешный мгновенный прогрев через {}! Стартовый ATR: {:.2} USD (на базе {} свечей)",
                source,
                calculated_atr,
                history.len()
            );
        }

        Ok(())
    }

    async fn fetch_bybit_bars(client: &reqwest::Client) -> Result<Vec<Bar>, DynError> {
        // Загружаем последние 50 минутных свечей BTCUSDT
        let url = "https://api.bybit.com/v5/market/kline?category=linear&symbol=BTCUSDT&interval=1&limit=50";

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
        // Bybit возвращает свечи от новых к старым (descending), обходим в обратном порядке (ascending)
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

    async fn fetch_binance_bars(client: &reqwest::Client) -> Result<Vec<Bar>, DynError> {
        let url = "https://api.binance.com/api/v3/klines?symbol=BTCUSDT&interval=1m&limit=50";

        let resp = client.get(url).send().await?;
        if !resp.status().is_success() {
            return Err(format!("Binance REST error: {}", resp.status()).into());
        }

        let list: serde_json::Value = resp.json().await?;
        let arr = list
            .as_array()
            .ok_or("Binance kline response is not an array")?;

        let mut bars = vec![];
        // Binance возвращает свечи от старых к новым (ascending)
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

    /// Запускает асинхронный фоновый поток, который бесконечно слушает биржу
    /// и пересчитывает ATR при закрытии каждой минутной свечи.
    pub fn start_tracking(&self) {
        let current_atr_clone = self.current_atr.clone();

        tokio::spawn(async move {
            let mut history: Vec<Bar> = Vec::new();

            loop {
                println!("[ATR] Подключение к WebSocket Bybit...");
                let (ws_stream, _) = match connect_async(BYBIT_WS_URL).await {
                    Ok(val) => val,
                    Err(e) => {
                        eprintln!("[ATR] Ошибка подключения: {}. Повтор через 5с...", e);
                        tokio::time::sleep(Duration::from_secs(5)).await;
                        continue;
                    }
                };

                let (mut write, mut read) = ws_stream.split();

                // Подписываемся на 1-минутные свечи BTCUSDT
                let subscribe_msg = r#"{"op": "subscribe", "args": ["kline.1.BTCUSDT"]}"#;
                if let Err(e) = write.send(Message::Text(subscribe_msg.into())).await {
                    eprintln!("[ATR] Ошибка отправки подписки: {}", e);
                    continue;
                }

                println!("[ATR] Успешно подписались на поток kline.1.BTCUSDT");

                while let Some(message) = read.next().await {
                    match message {
                        Ok(Message::Text(text)) => {
                            if let Ok(response) = serde_json::from_str::<BybitResponse>(&text) {
                                if let (Some(topic), Some(data_list)) =
                                    (response.topic, response.data)
                                {
                                    if topic == "kline.1.BTCUSDT" && !data_list.is_empty() {
                                        let candle = &data_list[0];

                                        // Нас интересуют только полностью закрытые свечи
                                        if candle.confirm {
                                            let high: f64 = candle.high.parse().unwrap_or(0.0);
                                            let low: f64 = candle.low.parse().unwrap_or(0.0);
                                            let close: f64 = candle.close.parse().unwrap_or(0.0);

                                            let new_bar = Bar { high, low, close };

                                            // Рассчитываем и обновляем ATR
                                            if let Some(new_atr) =
                                                Self::calculate_next_atr(&mut history, new_bar)
                                            {
                                                let mut atr_lock =
                                                    current_atr_clone.lock().unwrap();
                                                *atr_lock = new_atr;
                                                println!("[ATR Update] Новое значение ATR(14): {:.2} USD", new_atr);
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
                            eprintln!("[ATR] Сбой сессии WebSocket: {}", e);
                            break; // Выходим из внутреннего цикла для переподключения
                        }
                        _ => {}
                    }
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });
    }

    /// Математический расчет True Range и скользящего ATR
    fn calculate_next_atr(history: &mut Vec<Bar>, new_bar: Bar) -> Option<f64> {
        if history.is_empty() {
            // Для самой первой свечи True Range — это просто её High - Low
            let tr = new_bar.high - new_bar.low;
            history.push(new_bar);
            // Если это вообще первая свеча, ATR пока рассчитать нельзя, собираем историю
            return None;
        }

        let prev_bar = history.last().unwrap();

        // Классическая формула True Range: Max(H-L, |H-C_prev|, |L-C_prev|)
        let tr1 = new_bar.high - new_bar.low;
        let tr2 = (new_bar.high - prev_bar.close).abs();
        let tr3 = (new_bar.low - prev_bar.close).abs();
        let current_tr = tr1.max(tr2).max(tr3);

        history.push(new_bar);

        // Усекаем историю, чтобы не текла память, храним чуть больше периода
        if history.len() > ATR_PERIOD + 1 {
            history.remove(0);
        }

        if history.len() <= ATR_PERIOD {
            // Данных для полноценного усреднения еще мало
            return None;
        }

        // Если это первый расчет после накопления 14 свечей — считаем простое среднее
        // В последующие разы используем классическое сглаживание Уайлдера (RMA)
        let mut sum_tr = current_tr;

        // Считаем TR для предыдущих элементов истории
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
