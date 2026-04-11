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

const TAG_PROMPT: &str = "\
Analyze this image and return ONLY a JSON array of descriptive English tags. \
No explanation, no markdown, just the raw JSON array. \
Include 10-20 tags covering: subjects, objects, colors, mood, setting, \
activities, visual style, lighting, composition, and any text visible. \
Use lowercase. Example: [\"outdoor\",\"sunset\",\"mountain\",\"orange sky\",\"silhouette\",\"hiking\"]";

/// Simpler prompt for local Ollama models — more reliable JSON output
const OLLAMA_TAG_PROMPT: &str = "\
Look at this image. List 10-15 descriptive tags as a JSON array of strings.
Tags should describe: people, objects, colors, setting, mood, activities.
Use lowercase English. Output ONLY the JSON array, nothing else.
Example output: [\"person\",\"outdoor\",\"sunset\",\"smiling\",\"casual\"]";

/// Parse AI response into tag list, handling various response formats
pub fn extract_tags(text: &str) -> Vec<String> {
    let text = text.trim();

    // Strip markdown code blocks: ```json ... ``` or ``` ... ```
    let cleaned = if let Some(inner) = text.strip_prefix("```json").or_else(|| text.strip_prefix("```")) {
        inner.trim_end_matches("```").trim()
    } else {
        text
    };

    // Try direct JSON parse
    if let Ok(tags) = serde_json::from_str::<Vec<String>>(cleaned) {
        return normalize_tags(tags);
    }

    // Find first JSON array in response
    if let (Some(start), Some(end)) = (cleaned.find('['), cleaned.rfind(']')) {
        let slice = &cleaned[start..=end];
        if let Ok(tags) = serde_json::from_str::<Vec<String>>(slice) {
            return normalize_tags(tags);
        }
    }

    // Handle "- tag" or "* tag" bullet list format
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
        return normalize_tags(bullet_tags);
    }

    // Last resort: split by comma
    let tags: Vec<String> = cleaned
        .lines()
        .flat_map(|l| l.split(','))
        .map(|s| {
            s.trim_matches(|c: char| !c.is_alphanumeric() && c != ' ' && c != '-')
                .trim()
                .to_lowercase()
        })
        .filter(|s| !s.is_empty() && s.len() < 60 && s.len() > 1)
        .collect();
    tags
}

fn normalize_tags(tags: Vec<String>) -> Vec<String> {
    tags.into_iter()
        .map(|t| t.trim().to_lowercase())
        .filter(|t| !t.is_empty() && t.len() < 60)
        .collect()
}

// ── Claude (Anthropic) ───────────────────────────────────────────────────────

pub async fn call_claude(image_b64: &str, api_key: &str, model: &str) -> Result<Vec<String>> {
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
                { "type": "text", "text": TAG_PROMPT }
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

    let text = json["content"][0]["text"].as_str().unwrap_or("[]");
    Ok(extract_tags(text))
}

// ── OpenAI (GPT-4o) ─────────────────────────────────────────────────────────

pub async fn call_openai(image_b64: &str, api_key: &str, model: &str) -> Result<Vec<String>> {
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
                { "type": "text", "text": TAG_PROMPT }
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
        .unwrap_or("[]");
    Ok(extract_tags(text))
}

// ── Google Gemini ────────────────────────────────────────────────────────────

pub async fn call_gemini(image_b64: &str, api_key: &str, model: &str) -> Result<Vec<String>> {
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
                { "text": TAG_PROMPT }
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
        .unwrap_or("[]");
    Ok(extract_tags(text))
}

// ── xAI Grok ─────────────────────────────────────────────────────────────────

pub async fn call_grok(image_b64: &str, api_key: &str, model: &str) -> Result<Vec<String>> {
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
                { "type": "text", "text": TAG_PROMPT }
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
        .unwrap_or("[]");
    Ok(extract_tags(text))
}

// ── Ollama (local) ────────────────────────────────────────────────────────────

/// Default Ollama endpoint — configurable via `ollama_endpoint` setting.
pub const DEFAULT_OLLAMA_URL: &str = "http://localhost:11434";

/// Call a local Ollama model with vision support (e.g. gemma3:4b, qwen2.5vl:7b).
/// Ollama uses the `/api/chat` endpoint with `images` field for base64 data.
pub async fn call_ollama(image_b64: &str, model: &str, endpoint: &str) -> Result<Vec<String>> {
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
            "content": OLLAMA_TAG_PROMPT,
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

    let tags = extract_tags(text);

    if tags.is_empty() {
        // Log the raw response to help debug
        let preview = &text[..text.len().min(200)];
        return Err(anyhow::anyhow!(
            "Ollama response could not be parsed into tags. Raw: {}",
            preview
        ));
    }

    Ok(tags)
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

/// Call the appropriate provider API
pub async fn call_provider(
    provider: AiProvider,
    image_b64: &str,
    api_key: &str,  // For Local provider: this holds the Ollama endpoint URL
    model: &str,
) -> Result<Vec<String>> {
    match provider {
        AiProvider::Claude => call_claude(image_b64, api_key, model).await,
        AiProvider::OpenAI => call_openai(image_b64, api_key, model).await,
        AiProvider::Gemini => call_gemini(image_b64, api_key, model).await,
        AiProvider::Grok => call_grok(image_b64, api_key, model).await,
        AiProvider::Local => {
            let endpoint = if api_key.is_empty() { DEFAULT_OLLAMA_URL } else { api_key };
            call_ollama(image_b64, model, endpoint).await
        }
    }
}

// ── Translation via cheapest available text API ──────────────────────────────

const TRANSLATE_PROMPT: &str = "\
Translate the following search query to English tags for image search. \
Return ONLY a JSON array of English search terms. No explanation. \
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
            // Use Ollama for translation too — text-only, no image
            let endpoint = if api_key.is_empty() { DEFAULT_OLLAMA_URL } else { api_key };
            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(60))
                .build()
                .unwrap();
            let url = format!("{}/api/chat", endpoint.trim_end_matches('/'));
            let body = serde_json::json!({
                "model": "qwen2.5:7b",  // text-only model for translation
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
