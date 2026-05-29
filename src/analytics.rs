use std::fs;
use std::path::Path;
use gcp_auth::{CustomServiceAccount, TokenProvider};
use serde::Deserialize;
use serde_json::json;

/// Структура ответа от Vertex AI API для безопасного парсинга
#[derive(Deserialize, Debug)]
struct VertexResponse {
    candidates: Vec<Candidate>,
}

#[derive(Deserialize, Debug)]
struct Candidate {
    content: Content,
}

#[derive(Deserialize, Debug)]
struct Content {
    parts: Vec<Part>,
}

#[derive(Deserialize, Debug)]
struct Part {
    text: String,
}

pub struct VertexAnalytics {
    service_account: CustomServiceAccount,
    project_id: String,
}

impl VertexAnalytics {
    /// Инициализация модуля аналитики. 
    /// Метод принимает путь к JSON-файлу вашего сервисного аккаунта,
    /// автоматически выставляет переменную окружения и инициализирует менеджер токенов.
    pub fn new<P: AsRef<Path>>(key_path: P) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let path_ref = key_path.as_ref();
        
        if !path_ref.exists() {
            return Err(format!(
                "Критическая ошибка: Файл креденшелов сервисного аккаунта не найден по пути: {:?}", 
                path_ref
            ).into());
        }

        // Автоматически выставляем переменную окружения для библиотеки gcp_auth
        std::env::set_var("GOOGLE_APPLICATION_CREDENTIALS", path_ref);
        
        // Читаем содержимое JSON-ключа, чтобы автоматически вытащить project_id проекта GCP
        let key_content = fs::read_to_string(path_ref)?;
        let key_json: serde_json::Value = serde_json::from_str(&key_content)?;
        
        let project_id = key_json["project_id"]
            .as_str()
            .ok_or("В предоставленном сервисном JSON-ключе отсутствует обязательное поле 'project_id'")?
            .to_string();

        // Создаем менеджер аутентификации Google на базе CustomServiceAccount (совместимо с gcp_auth 0.12)
        let service_account = CustomServiceAccount::from_file(key_path)?;

        println!("[Vertex AI] Успешная авторизация. Проект инициализации: {}", project_id);
        Ok(Self {
            service_account,
            project_id,
        })
    }

    /// Отправляет накопленные CSV логи торгов робота в Gemini 2.5 Pro на глубокий аудит.
    /// Возвращает текстовый отчет с рекомендациями по изменению параметров стратегии.
    pub async fn analyze_trading_logs(&self, logs_csv: &str) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        // 1. Запрашиваем токен доступа. gcp_auth берет его из кэша памяти,
        // либо запрашивает новый у Google OAuth 2.0, если прошлый стек токена истек (> 1 часа).
        let scopes = &["https://www.googleapis.com/auth/cloud-platform"];
        let token = self.service_account.token(scopes).await?;

        // 2. Строим endpoint к Vertex AI API. 
        // Используем модель gemini-2.5-pro, так как она обладает лучшей математической логикой для анализа таблиц.
        let url = format!(
            "https://us-central1-aiplatform.googleapis.com/v1/projects/{}/locations/us-central1/publishers/google/models/gemini-2.5-pro:generateContent",
            self.project_id
        );

        // 3. Формируем строгий системный промпт (инструкцию) для ИИ-аналитика
        let system_instruction = "Ты — ведущий квантовый аналитик и эксперт по HFT-стратегиям на крипторынке. \
        Твоя задача — провести глубокий математический аудит логов робота GEM_RUST на 15-минутных экспресс-рынках Polymarket. \
        Проанализируй предоставленный тебе CSV-массив данных. \
        Выдели скрытые паттерны убытков и корреляции по следующим направлениям:\n\
        1. ЭФФЕКТИВНОСТЬ ДИАПАЗОНОВ ATR: Определи точные границы минутной волатильности Биткоина (btc_atr), при которых стратегия Dynamic Grid показывает максимальную доходность, а при каких — систематически сливает из-за затяжного флэта.\n\
        2. АУДИТ DYNAMIC BUY: Оцени эффективность модуля докупок слабой стороны по цене 0.15-0.16. Окупает ли себя повышенный объем при последующем отскоке Биткоина, или бот просто увеличивает убыток по временному стопу?\n\
        3. АНАЛИЗ TIME-DECAY И CROSSOVER: Проверь точки фиксации слабой стороны на пересечениях линии старта. На каком проценте времени раунда (time_pct) спред и распад стоимости контракта делают выход невыгодным?\n\
        4. КАЛИБРОВКА СЕТКИ: Предложи математически обоснованные изменения шагов фиксации сильной стороны (текущие шаги: [0.58, 0.66, 0.75]). Стоит ли сузить или расширить шаги сетки?\n\
        Выдавай отчет строго в профессиональном стиле, с приведением цифр, процентов и готовых рекомендаций для внесения в файл конфигурации (config.toml) робота.";

        // 4. Пакуем тело запроса в соответствии с официальной спецификацией Vertex AI API
        let payload = json!({
            "contents": {
                "role": "user",
                "parts": [
                    { "text": format!("Вот файлы логов торговых сессий в формате CSV для анализа:\n\n{}", logs_csv) }
                ]
            },
            "systemInstruction": {
                "parts": [
                    { "text": system_instruction }
                ]
            },
            "generationConfig": {
                "temperature": 0.1, // Минимальная температура для исключения галлюцинаций ИИ
                "maxOutputTokens": 8192
            }
        });

        // 5. Выполняем асинхронный HTTP POST запрос с Bearer OAuth2 токеном нашего сервисного аккаунта
        let client = reqwest::Client::new();
        let response = client.post(&url)
            .bearer_auth(token.as_str())
            .json(&payload)
            .send()
            .await?;

        // Если Google вернул ошибку (например, не включен Vertex API в консоли или лимиты), ловим её
        if !response.status().is_success() {
            let err_text = response.text().await?;
            return Err(format!("Исключение Vertex AI API: {}", err_text).into());
        }

        // 6. Безопасно парсим ответ через типизированную структуру
        let resp_data: VertexResponse = response.json::<VertexResponse>().await?;
        
        let ai_report = resp_data
            .candidates
            .first()
            .and_then(|c| c.content.parts.first())
            .map(|p| p.text.clone())
            .ok_or("Не удалось извлечь текстовый блок аналитики из ответа Vertex AI. Структура JSON изменена.")?;

        Ok(ai_report)
    }
}
