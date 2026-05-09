use anyhow::{Context, Result};

use crate::models::AiProvider;

/// Error kind — tagger decides what action to take based on this
#[derive(Debug, Clone, PartialEq)]
pub enum ApiErrorKind {
    /// 401/403 — API key invalid, disable this provider
    AuthFailed,
    /// 429 — Rate limit, wait and retry
    RateLimit { retry_after_secs: u64 },
    /// 5xx / timeout — Transient error, retry after a short delay
    Transient,
    /// Other permanent errors
    Permanent,
}

#[derive(Debug)]
pub struct ApiError {
    pub kind: ApiErrorKind,
    pub message: String,
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}
impl std::error::Error for ApiError {}

fn classify_status(status: u16, body: &str) -> ApiErrorKind {
    match status {
        401 | 403 => ApiErrorKind::AuthFailed,
        429 => {
            // Some APIs provide retry duration in Retry-After header or body
            let secs = body.parse::<u64>().unwrap_or(15);
            ApiErrorKind::RateLimit { retry_after_secs: secs.min(120) }
        }
        500..=599 => ApiErrorKind::Transient,
        _ => ApiErrorKind::Permanent,
    }
}

const TAG_PROMPT_EN: &str = "\
Analyze this image and return a JSON object with three fields: \"tags\", \"description\", and \"location\".
- \"tags\": array of 20-40 lowercase English tags covering people, emotions, clothing, objects, food, location, architecture, colors, mood, weather, activity. Be SPECIFIC: say \"woman\" not \"person\", say \"pasta\" not \"food\".
- \"description\": one vivid English sentence (max 30 words) describing what is happening in the photo.
- \"location\": your best guess of where this photo was taken, as an object with \"lat\" (number), \"lon\" (number), and \"name\" (string, e.g. \"Istanbul, Turkey\"). Use visual clues like architecture, signs, vegetation, landmarks. If you truly cannot guess, set location to null.
Return ONLY the raw JSON object, no markdown, no explanation.
Example: {\"tags\":[\"woman\",\"man\",\"couple\",\"selfie\",\"smiling\",\"restaurant\",\"indoor\",\"dinner\",\"wine glass\",\"romantic\"],\"description\":\"A smiling couple taking a selfie at a cozy restaurant during a romantic dinner.\",\"location\":{\"lat\":41.01,\"lon\":28.97,\"name\":\"Istanbul, Turkey\"}}";

/// Turkish-output tag prompt. Uses native Turkish vocabulary so tags read
/// naturally ("köpek" not "dog", "düğün" not "wedding"). We still ask for
/// the location `name` in "Şehir, Ülke" form.
const TAG_PROMPT_TR: &str = "\
Bu fotoğrafı analiz et ve üç alanlı bir JSON nesnesi döndür: \"tags\", \"description\", \"location\".
- \"tags\": 20-40 adet küçük harfli TÜRKÇE etiket dizisi. Kişi, duygu, kıyafet, nesne, yemek, konum, mimari, renk, atmosfer, hava, aktivite kategorilerini kapsasın. SPESİFİK ol: \"insan\" yerine \"kadın\", \"yemek\" yerine \"makarna\" yaz. İngilizce etiket KULLANMA.
- \"description\": Fotoğrafta ne olduğunu anlatan, en fazla 30 kelimelik bir TÜRKÇE cümle.
- \"location\": Fotoğrafın nerede çekildiğine dair en iyi tahminin; {\"lat\":sayı,\"lon\":sayı,\"name\":\"Şehir, Ülke\"} biçiminde. Mimari, tabelalar, bitki örtüsü, önemli yapılar gibi görsel ipuçlarını kullan. Tahmin edemiyorsan null döndür.
SADECE ham JSON nesnesini döndür; markdown, açıklama veya kod bloğu ekleme.
Örnek: {\"tags\":[\"kadın\",\"erkek\",\"çift\",\"selfie\",\"gülümseyen\",\"restoran\",\"iç mekan\",\"akşam yemeği\",\"şarap kadehi\",\"romantik\"],\"description\":\"Bir çift, romantik bir akşam yemeğinde sıcak bir restoranda selfie çekiyor.\",\"location\":{\"lat\":41.01,\"lon\":28.97,\"name\":\"İstanbul, Türkiye\"}}";

/// Detailed prompt for local Ollama models (English)
const OLLAMA_TAG_PROMPT_EN: &str = "\
Analyze this image. Return a JSON object with three fields:
- \"tags\": array of 20-40 lowercase English tags (people, food, location, objects, emotions, clothing, colors, activity)
- \"description\": one English sentence (max 25 words) describing the scene
- \"location\": your best guess where this was taken: {\"lat\":number,\"lon\":number,\"name\":\"City, Country\"}. Use visual clues. If unknown, set to null.
Be SPECIFIC: say \"man\" not \"person\", say \"pasta\" not \"food\".
Output ONLY the raw JSON object, nothing else.
Example: {\"tags\":[\"woman\",\"man\",\"couple\",\"restaurant\",\"indoor\",\"pasta\",\"wine\",\"smiling\",\"romantic\"],\"description\":\"A couple enjoying pasta and wine at a restaurant.\",\"location\":{\"lat\":41.01,\"lon\":28.97,\"name\":\"Istanbul, Turkey\"}}";

const OLLAMA_TAG_PROMPT_TR: &str = "\
Bu fotoğrafı analiz et. Üç alanlı JSON nesnesi döndür:
- \"tags\": 20-40 küçük harf TÜRKÇE etiket dizisi (kişi, yemek, konum, nesne, duygu, kıyafet, renk, aktivite)
- \"description\": Sahneyi anlatan en fazla 25 kelimelik bir TÜRKÇE cümle
- \"location\": Nerede çekildiğine dair tahminin: {\"lat\":sayı,\"lon\":sayı,\"name\":\"Şehir, Ülke\"}. Görsel ipuçları kullan. Bilmiyorsan null.
SPESİFİK ol: \"insan\" yerine \"kadın\", \"yemek\" yerine \"makarna\" kullan.
SADECE ham JSON nesnesi döndür, başka hiçbir şey yazma.
Örnek: {\"tags\":[\"kadın\",\"erkek\",\"çift\",\"restoran\",\"iç mekan\",\"makarna\",\"şarap\",\"gülümseyen\",\"romantik\"],\"description\":\"Bir çift restoranda makarna ve şarabın tadını çıkarıyor.\",\"location\":{\"lat\":41.01,\"lon\":28.97,\"name\":\"İstanbul, Türkiye\"}}";

/// Global language toggle. 0 = English (default), 1 = Turkish. Set via
/// `set_tag_language` command from the frontend. Providers read this at
/// the start of each request.
pub static TAG_LANG: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

pub fn current_tag_prompt() -> &'static str {
    if TAG_LANG.load(std::sync::atomic::Ordering::Relaxed) == 1 { TAG_PROMPT_TR } else { TAG_PROMPT_EN }
}

pub fn current_ollama_tag_prompt() -> &'static str {
    if TAG_LANG.load(std::sync::atomic::Ordering::Relaxed) == 1 { OLLAMA_TAG_PROMPT_TR } else { OLLAMA_TAG_PROMPT_EN }
}

/// Estimated location from AI
pub type EstimatedLocation = (f64, f64, String); // (lat, lon, name)

/// Parse AI response into (tags, description, location)
pub fn extract_tags_and_description(text: &str) -> (Vec<String>, Option<String>, Option<EstimatedLocation>) {
    let text = text.trim();
    let cleaned = if let Some(inner) = text.strip_prefix("```json").or_else(|| text.strip_prefix("```")) {
        inner.trim_end_matches("```").trim()
    } else {
        text
    };

    fn parse_location(obj: &serde_json::Value) -> Option<EstimatedLocation> {
        let loc = obj.get("location")?;
        if loc.is_null() { return None; }
        let lat = loc.get("lat")?.as_f64()?;
        let lon = loc.get("lon")?.as_f64()?;
        let name = loc.get("name")?.as_str().unwrap_or("Unknown").to_string();
        // Sanity check: valid coordinates
        if lat.abs() > 90.0 || lon.abs() > 180.0 { return None; }
        Some((lat, lon, name))
    }

    // Try parsing as {"tags": [...], "description": "...", "location": {...}}
    if let Ok(obj) = serde_json::from_str::<serde_json::Value>(cleaned) {
        if let Some(tags_val) = obj.get("tags") {
            if let Ok(tags) = serde_json::from_value::<Vec<String>>(tags_val.clone()) {
                let desc = obj.get("description")
                    .and_then(|d| d.as_str())
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string());
                let loc = parse_location(&obj);
                return (normalize_tags(tags), desc, loc);
            }
        }
        // Maybe it's just an array
        if let Ok(tags) = serde_json::from_value::<Vec<String>>(obj.clone()) {
            return (normalize_tags(tags), None, None);
        }
    }

    // Find JSON object {...}
    if let (Some(start), Some(end)) = (cleaned.find('{'), cleaned.rfind('}')) {
        let slice = &cleaned[start..=end];
        if let Ok(obj) = serde_json::from_str::<serde_json::Value>(slice) {
            if let Some(tags_val) = obj.get("tags") {
                if let Ok(tags) = serde_json::from_value::<Vec<String>>(tags_val.clone()) {
                    let desc = obj.get("description")
                        .and_then(|d| d.as_str())
                        .filter(|s| !s.is_empty())
                        .map(|s| s.to_string());
                    let loc = parse_location(&obj);
                    return (normalize_tags(tags), desc, loc);
                }
            }
        }
    }

    // Find JSON array [...]
    if let (Some(start), Some(end)) = (cleaned.find('['), cleaned.rfind(']')) {
        let slice = &cleaned[start..=end];
        if let Ok(tags) = serde_json::from_str::<Vec<String>>(slice) {
            return (normalize_tags(tags), None, None);
        }
    }

    // Bullet list fallback
    let bullet_tags: Vec<String> = cleaned
        .lines()
        .filter_map(|l| {
            let l = l.trim().trim_start_matches('-').trim_start_matches('*').trim_start_matches('•').trim();
            if !l.is_empty() && l.len() > 1 && l.len() < 60 && !l.starts_with('{') && !l.starts_with('[') {
                Some(l.to_lowercase())
            } else {
                None
            }
        })
        .collect();
    if bullet_tags.len() >= 3 {
        return (normalize_tags(bullet_tags), None, None);
    }

    (vec![], None, None)
}

/// Legacy: kept for translate_for_clip usage
pub fn extract_tags(text: &str) -> Vec<String> {
    extract_tags_and_description(text).0
}

fn normalize_tags(tags: Vec<String>) -> Vec<String> {
    tags.into_iter()
        .map(|t| t.trim().to_lowercase())
        .filter(|t| !t.is_empty() && t.len() < 60)
        .collect()
}

// ── Claude (Anthropic) ───────────────────────────────────────────────────────

pub async fn call_claude(image_b64: &str, api_key: &str, model: &str) -> Result<(Vec<String>, Option<String>, Option<EstimatedLocation>)> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .context("HTTP client build failed")?;
    let body = serde_json::json!({
        "model": model,
        "max_tokens": 512,
        "messages": [{
            "role": "user",
            "content": [
                {
                    "type": "image",
                    "source": {
                        "type": "base64",
                        "media_type": "image/jpeg",
                        "data": image_b64
                    }
                },
                { "type": "text", "text": current_tag_prompt() }
            ]
        }]
    });

    let resp = client
        .post("https://api.anthropic.com/v1/messages")
        .header("x-api-key", api_key)
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await
        .context("Claude request failed")?;

    let status = resp.status();
    let json: serde_json::Value = resp.json().await.context("Claude response parse")?;

    if !status.is_success() {
        return Err(anyhow::anyhow!(
            "Claude API {} : {}",
            status,
            json["error"]["message"].as_str().unwrap_or("unknown error")
        ));
    }

    let text = json["content"][0]["text"].as_str().unwrap_or("{}");
    Ok(extract_tags_and_description(text))
}

// ── OpenAI (GPT-4o) ─────────────────────────────────────────────────────────

pub async fn call_openai(image_b64: &str, api_key: &str, model: &str) -> Result<(Vec<String>, Option<String>, Option<EstimatedLocation>)> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .context("HTTP client build failed")?;
    let body = serde_json::json!({
        "model": model,
        "max_tokens": 512,
        "messages": [{
            "role": "user",
            "content": [
                {
                    "type": "image_url",
                    "image_url": {
                        "url": format!("data:image/jpeg;base64,{}", image_b64),
                        "detail": "low"
                    }
                },
                { "type": "text", "text": current_tag_prompt() }
            ]
        }]
    });

    let resp = client
        .post("https://api.openai.com/v1/chat/completions")
        .header("Authorization", format!("Bearer {}", api_key))
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await
        .context("OpenAI request failed")?;

    let status = resp.status();
    let json: serde_json::Value = resp.json().await.context("OpenAI response parse")?;

    if !status.is_success() {
        return Err(anyhow::anyhow!(
            "OpenAI API {} : {}",
            status,
            json["error"]["message"].as_str().unwrap_or("unknown error")
        ));
    }

    let text = json["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or("{}");
    Ok(extract_tags_and_description(text))
}

// ── Google Gemini ────────────────────────────────────────────────────────────

pub async fn call_gemini(image_b64: &str, api_key: &str, model: &str) -> Result<(Vec<String>, Option<String>, Option<EstimatedLocation>)> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .context("HTTP client build failed")?;
    let url = format!(
        "https://generativelanguage.googleapis.com/v1beta/models/{}:generateContent?key={}",
        model, api_key
    );

    let body = serde_json::json!({
        "contents": [{
            "parts": [
                {
                    "inline_data": {
                        "mime_type": "image/jpeg",
                        "data": image_b64
                    }
                },
                { "text": current_tag_prompt() }
            ]
        }],
        "generationConfig": {
            "maxOutputTokens": 512,
            "temperature": 0.2
        }
    });

    let resp = client
        .post(&url)
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await
        .context("Gemini request failed")?;

    let status = resp.status();
    let json: serde_json::Value = resp.json().await.context("Gemini response parse")?;

    if !status.is_success() {
        let msg = json["error"]["message"]
            .as_str()
            .unwrap_or("unknown error");
        return Err(anyhow::anyhow!("Gemini API {} : {}", status, msg));
    }

    let text = json["candidates"][0]["content"]["parts"][0]["text"]
        .as_str()
        .unwrap_or("{}");
    Ok(extract_tags_and_description(text))
}

// ── xAI Grok ─────────────────────────────────────────────────────────────────

pub async fn call_grok(image_b64: &str, api_key: &str, model: &str) -> Result<(Vec<String>, Option<String>, Option<EstimatedLocation>)> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .context("HTTP client build failed")?;
    let body = serde_json::json!({
        "model": model,
        "max_tokens": 512,
        "messages": [{
            "role": "user",
            "content": [
                {
                    "type": "image_url",
                    "image_url": {
                        "url": format!("data:image/jpeg;base64,{}", image_b64),
                        "detail": "low"
                    }
                },
                { "type": "text", "text": current_tag_prompt() }
            ]
        }]
    });

    let resp = client
        .post("https://api.x.ai/v1/chat/completions")
        .header("Authorization", format!("Bearer {}", api_key))
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await
        .context("Grok request failed")?;

    let status = resp.status();
    let json: serde_json::Value = resp.json().await.context("Grok response parse")?;

    if !status.is_success() {
        return Err(anyhow::anyhow!(
            "Grok API {} : {}",
            status,
            json["error"]["message"].as_str().unwrap_or("unknown error")
        ));
    }

    let text = json["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or("{}");
    Ok(extract_tags_and_description(text))
}

// ── Ollama (local) ────────────────────────────────────────────────────────────

/// Default Ollama endpoint — configurable via `ollama_endpoint` setting.
pub const DEFAULT_OLLAMA_URL: &str = "http://localhost:11434";

/// Call a local Ollama model with vision support (e.g. gemma3:4b, qwen2.5vl:7b).
/// Ollama uses the `/api/chat` endpoint with `images` field for base64 data.
pub async fn call_ollama(image_b64: &str, model: &str, endpoint: &str) -> Result<(Vec<String>, Option<String>, Option<EstimatedLocation>)> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build()
        .context("HTTP client build failed")?;

    let url = format!("{}/api/chat", endpoint.trim_end_matches('/'));

    let body = serde_json::json!({
        "model": model,
        "stream": false,
        "keep_alive": "5m",
        "options": {
            "num_ctx": 4096,
            "temperature": 0.1
        },
        "messages": [{
            "role": "user",
            "content": current_ollama_tag_prompt(),
            "images": [image_b64]
        }]
    });

    let resp = client
        .post(&url)
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await
        .context("Ollama request failed — is Ollama running? (ollama serve)")?;

    let status = resp.status();
    if status == 404 {
        return Err(anyhow::anyhow!(
            "Model '{}' not found in Ollama. Run: ollama pull {}",
            model, model
        ));
    }

    let json: serde_json::Value = resp.json().await.context("Ollama response parse")?;

    if !status.is_success() {
        return Err(anyhow::anyhow!(
            "Ollama error {}: {}",
            status,
            json["error"].as_str().unwrap_or("unknown error")
        ));
    }

    let text = json["message"]["content"].as_str().unwrap_or("");

    if text.is_empty() {
        return Err(anyhow::anyhow!("Ollama returned empty response for model '{}'", model));
    }

    let (tags, desc, loc) = extract_tags_and_description(text);

    if tags.is_empty() {
        let preview = &text[..text.len().min(200)];
        return Err(anyhow::anyhow!(
            "Ollama response could not be parsed into tags. Raw: {}",
            preview
        ));
    }

    Ok((tags, desc, loc))
}

/// Check if Ollama is reachable and the model is available.
/// Returns (is_running, model_available, available_models)
pub async fn check_ollama_status(
    model: &str,
    endpoint: &str,
) -> (bool, bool, Vec<String>) {
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
    {
        Ok(c) => c,
        Err(_) => return (false, false, vec![]),
    };

    let url = format!("{}/api/tags", endpoint.trim_end_matches('/'));
    match client.get(&url).send().await {
        Err(_) => (false, false, vec![]),
        Ok(resp) => {
            if !resp.status().is_success() {
                return (true, false, vec![]);
            }
            let json: serde_json::Value = resp.json().await.unwrap_or_default();
            let models: Vec<String> = json["models"]
                .as_array()
                .unwrap_or(&vec![])
                .iter()
                .filter_map(|m| m["name"].as_str().map(|s| s.to_string()))
                .collect();
            // Normalize: "qwen2.5vl" matches "qwen2.5-vl" and vice versa
            let model_norm = model.replace('-', "").to_lowercase();
            let has_model = models.iter().any(|m| {
                m.to_lowercase() == model.to_lowercase()
                    || m.replace('-', "").to_lowercase().starts_with(&model_norm)
                    || model_norm.starts_with(&m.replace('-', "").split(':').next().unwrap_or("").to_lowercase())
            });
            (true, has_model, models)
        }
    }
}

// ── Dispatch ─────────────────────────────────────────────────────────────────

/// Call the appropriate provider API — returns (tags, description)
pub async fn call_provider(
    provider: AiProvider,
    image_b64: &str,
    api_key: &str,
    model: &str,
) -> Result<(Vec<String>, Option<String>, Option<EstimatedLocation>)> {
    match provider {
        AiProvider::Claude => call_claude(image_b64, api_key, model).await,
        AiProvider::OpenAI => call_openai(image_b64, api_key, model).await,
        AiProvider::Gemini => call_gemini(image_b64, api_key, model).await,
        AiProvider::Grok => call_grok(image_b64, api_key, model).await,
        AiProvider::Local => {
            let endpoint = if api_key.is_empty() {
                DEFAULT_OLLAMA_URL
            } else {
                api_key.split('|').next().unwrap_or(DEFAULT_OLLAMA_URL)
            };
            call_ollama(image_b64, model, endpoint).await
        }
    }
}

// ── Translation via cheapest available text API ──────────────────────────────

const TRANSLATE_PROMPT: &str = "\
Translate ONLY this word/phrase to English. Return a JSON array with ONLY the direct translation. \
Do NOT add synonyms, related words, or broader categories. \
Examples: kadın→[\"woman\"], kedi→[\"cat\"], kırmızı araba→[\"red car\"], güneş batımı→[\"sunset\"]. \
If already English, return as-is but still in a JSON array. \
Input: ";

pub async fn translate_query(
    text: &str,
    provider: AiProvider,
    api_key: &str,
) -> Result<Vec<String>> {
    let prompt = format!("{}{}", TRANSLATE_PROMPT, text);

    let resp_text = match provider {
        AiProvider::Gemini => {
            let client = reqwest::Client::new();
            let url = format!(
                "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.0-flash:generateContent?key={}",
                api_key
            );
            let body = serde_json::json!({
                "contents": [{ "parts": [{ "text": prompt }] }],
                "generationConfig": { "maxOutputTokens": 256, "temperature": 0.1 }
            });
            let resp = client.post(&url).json(&body).send().await?;
            let json: serde_json::Value = resp.json().await?;
            json["candidates"][0]["content"]["parts"][0]["text"]
                .as_str()
                .unwrap_or("[]")
                .to_string()
        }
        AiProvider::OpenAI => {
            let client = reqwest::Client::new();
            let body = serde_json::json!({
                "model": "gpt-4o-mini",
                "max_tokens": 256,
                "messages": [{ "role": "user", "content": prompt }]
            });
            let resp = client
                .post("https://api.openai.com/v1/chat/completions")
                .header("Authorization", format!("Bearer {}", api_key))
                .json(&body)
                .send()
                .await?;
            let json: serde_json::Value = resp.json().await?;
            json["choices"][0]["message"]["content"]
                .as_str()
                .unwrap_or("[]")
                .to_string()
        }
        AiProvider::Claude => {
            let client = reqwest::Client::new();
            let body = serde_json::json!({
                "model": "claude-haiku-4-5-20251001",
                "max_tokens": 256,
                "messages": [{ "role": "user", "content": prompt }]
            });
            let resp = client
                .post("https://api.anthropic.com/v1/messages")
                .header("x-api-key", api_key)
                .header("anthropic-version", "2023-06-01")
                .json(&body)
                .send()
                .await?;
            let json: serde_json::Value = resp.json().await?;
            json["content"][0]["text"]
                .as_str()
                .unwrap_or("[]")
                .to_string()
        }
        AiProvider::Grok => {
            let client = reqwest::Client::new();
            let body = serde_json::json!({
                "model": "grok-2-latest",
                "max_tokens": 256,
                "messages": [{ "role": "user", "content": prompt }]
            });
            let resp = client
                .post("https://api.x.ai/v1/chat/completions")
                .header("Authorization", format!("Bearer {}", api_key))
                .json(&body)
                .send()
                .await?;
            let json: serde_json::Value = resp.json().await?;
            json["choices"][0]["message"]["content"]
                .as_str()
                .unwrap_or("[]")
                .to_string()
        }
        AiProvider::Local => {
            // Use Ollama for translation — use whatever model is configured
            let parts: Vec<&str> = api_key.splitn(2, '|').collect();
            let endpoint = if parts[0].is_empty() { DEFAULT_OLLAMA_URL } else { parts[0] };
            let model = if parts.len() > 1 && !parts[1].is_empty() { parts[1] } else { "gemma3:4b" };
            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(60))
                .build()
                .context("HTTP client build failed")?;
            let url = format!("{}/api/chat", endpoint.trim_end_matches('/'));
            let body = serde_json::json!({
                "model": model,
                "stream": false,
                "messages": [{ "role": "user", "content": prompt }]
            });
            let resp = client.post(&url).json(&body).send().await?;
            let json: serde_json::Value = resp.json().await?;
            json["message"]["content"].as_str().unwrap_or("[]").to_string()
        }
    };

    Ok(extract_tags(&resp_text))
}
