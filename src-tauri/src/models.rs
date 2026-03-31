use serde::{Deserialize, Serialize};

// ── AI Provider ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[serde(rename_all = "lowercase")]
pub enum AiProvider {
    Claude,
    #[serde(alias = "openai", alias = "gpt4o")]
    OpenAI,
    Gemini,
    Grok,
    /// Local model via Ollama (e.g. qwen2.5vl:7b). No API key required.
    #[serde(alias = "ollama", alias = "local")]
    Local,
}

impl AiProvider {
    pub fn all() -> &'static [AiProvider] {
        &[
            AiProvider::Claude,
            AiProvider::OpenAI,
            AiProvider::Gemini,
            AiProvider::Grok,
            AiProvider::Local,
        ]
    }

    pub fn name(&self) -> &'static str {
        match self {
            AiProvider::Claude => "Claude",
            AiProvider::OpenAI => "OpenAI",
            AiProvider::Gemini => "Gemini",
            AiProvider::Grok => "Grok",
            AiProvider::Local => "Local (Ollama)",
        }
    }

    pub fn key_name(&self) -> &'static str {
        match self {
            AiProvider::Claude => "claude",
            AiProvider::OpenAI => "openai",
            AiProvider::Gemini => "gemini",
            AiProvider::Grok => "grok",
            AiProvider::Local => "local",
        }
    }

    pub fn default_model(&self) -> &'static str {
        match self {
            AiProvider::Claude => "claude-haiku-4-5-20251001",
            AiProvider::OpenAI => "gpt-4o-mini",
            AiProvider::Gemini => "gemini-2.0-flash",
            AiProvider::Grok => "grok-2-vision-latest",
            AiProvider::Local => "qwen2.5vl:7b",
        }
    }

    /// Cost per image in USD. Local = $0 (uses your hardware).
    pub fn cost_per_image(&self) -> f64 {
        match self {
            AiProvider::Claude => 0.0004,
            AiProvider::OpenAI => 0.0003,
            AiProvider::Gemini => 0.0001,
            AiProvider::Grok => 0.0005,
            AiProvider::Local => 0.0,
        }
    }

    /// Requests per minute
    pub fn default_rpm(&self) -> u32 {
        match self {
            AiProvider::Claude => 50,
            AiProvider::OpenAI => 60,
            AiProvider::Gemini => 60,
            AiProvider::Grok => 30,
            // Local speed depends on hardware; conservative default
            AiProvider::Local => 10,
        }
    }

    /// Whether this provider needs an API key to function
    pub fn requires_api_key(&self) -> bool {
        !matches!(self, AiProvider::Local)
    }
}

impl std::fmt::Display for AiProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.key_name())
    }
}

impl std::str::FromStr for AiProvider {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "claude" | "anthropic" => Ok(AiProvider::Claude),
            "openai" | "gpt4o" | "gpt" | "chatgpt" => Ok(AiProvider::OpenAI),
            "gemini" | "google" => Ok(AiProvider::Gemini),
            "grok" | "xai" => Ok(AiProvider::Grok),
            "local" | "ollama" | "qwen" => Ok(AiProvider::Local),
            _ => Err(format!("Unknown provider: {}", s)),
        }
    }
}

// ── Provider status sent to frontend ─────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ProviderStatus {
    pub provider: AiProvider,
    pub name: String,
    pub has_key: bool,
    pub enabled: bool,
    pub model: String,
    pub cost_per_image: f64,
    pub total_tagged: i64,
    pub total_errors: i64,
    pub total_cost_usd: f64,
}

// ── Photo ────────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Photo {
    pub id: i64,
    pub path: String,
    pub filename: String,
    pub folder: String,
    pub hash: String,
    pub size: i64,
    pub width: Option<i32>,
    pub height: Option<i32>,
    pub created_at: String,
    pub tagged_at: Option<String>,
    pub thumbnail_path: Option<String>,
    pub status: String,
    pub provider_used: Option<String>,
    pub tags: Vec<TagEntry>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct TagEntry {
    pub tag: String,
    pub confidence: Option<f64>,
    pub source: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct PhotoSummary {
    pub id: i64,
    pub path: String,
    pub filename: String,
    pub status: String,
    pub provider_used: Option<String>,
    pub tags: Vec<String>,
    pub tag_count: i64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PhotosResponse {
    pub photos: Vec<PhotoSummary>,
    pub total: i64,
}

// ── Scan / Tag progress ──────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ScanProgress {
    pub total: usize,
    pub scanned: usize,
    pub new_files: usize,
    pub skipped: usize,
    pub current_file: String,
    pub is_running: bool,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ScanComplete {
    pub new_files: usize,
    pub skipped: usize,
    pub total: usize,
    pub folder: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct TagProgress {
    pub total: usize,
    pub completed: usize,
    pub failed: usize,
    pub current_file: String,
    pub current_provider: String,
    pub is_running: bool,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct TagComplete {
    pub tagged: usize,
    pub failed: usize,
    pub provider_breakdown: Vec<ProviderBreakdown>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ProviderBreakdown {
    pub provider: String,
    pub count: usize,
    pub cost_usd: f64,
}

// ── App stats ────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
pub struct AppStats {
    pub total_photos: i64,
    pub tagged_photos: i64,
    pub pending_photos: i64,
    pub error_photos: i64,
    pub total_tags: i64,
    pub unique_tags: i64,
    pub folders_scanned: i64,
    pub total_cost_usd: f64,
}

// ── Router decisions ─────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct RouteDecision {
    pub provider: AiProvider,
    pub api_key: String,
    pub model: String,
}

// ── Collections / Smart Albums ──────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Collection {
    pub id: i64,
    pub name: String,
    pub collection_type: String, // "manual" or "smart"
    pub rules_json: Option<String>,
    pub photo_count: i64,
    pub created_at: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct CollectionRule {
    pub field: String,       // "tag", "folder", "provider", "status", "date_after", "date_before"
    pub operator: String,    // "contains", "equals", "not_equals"
    pub value: String,
}

// ── Watch Folders ───────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct WatchFolder {
    pub id: i64,
    pub path: String,
    pub enabled: bool,
    pub auto_tag: bool,
    pub last_checked: Option<String>,
    pub photo_count: i64,
}

// ── Export ───────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
pub struct ExportResult {
    pub path: String,
    pub count: usize,
}

// ── EXIF / GPS ──────────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct PhotoExif {
    pub camera: Option<String>,
    pub lens: Option<String>,
    pub focal_length: Option<String>,
    pub aperture: Option<String>,
    pub shutter_speed: Option<String>,
    pub iso: Option<String>,
    pub date_taken: Option<String>,
    pub gps_lat: Option<f64>,
    pub gps_lon: Option<f64>,
    pub gps_alt: Option<f64>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct GpsPhoto {
    pub id: i64,
    pub filename: String,
    pub lat: f64,
    pub lon: f64,
    pub tag_count: i64,
}

// ── Duplicate Detection ─────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct DuplicateGroup {
    pub hash: String,
    pub photos: Vec<PhotoSummary>,
}

// ── Tag Management ──────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct TagInfo {
    pub tag: String,
    pub count: i64,
    pub providers: Vec<String>,
}

// ── Budget ──────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
pub struct BudgetStatus {
    pub monthly_limit_usd: f64,
    pub spent_this_month: f64,
    pub remaining: f64,
    pub is_over: bool,
}

// ── Face Recognition ─────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Person {
    pub id: i64,
    pub name: String,
    pub thumbnail: Option<String>, // base64-encoded JPEG of representative face
    pub face_count: i64,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct FaceRegion {
    pub id: i64,
    pub photo_id: i64,
    pub x1: i32,
    pub y1: i32,
    pub x2: i32,
    pub y2: i32,
    pub score: f32,
    pub person_id: Option<i64>,
    pub person_name: Option<String>,
    pub thumbnail_b64: Option<String>, // base64-encoded 128×128 JPEG crop
}

// ── Cost Dashboard ──────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
pub struct CostDashboard {
    pub total_cost: f64,
    pub total_tagged: i64,
    pub avg_cost_per_image: f64,
    pub estimated_savings: f64,
    pub provider_costs: Vec<ProviderCostInfo>,
    pub daily_costs: Vec<DailyCost>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ProviderCostInfo {
    pub provider: String,
    pub count: i64,
    pub cost: f64,
    pub avg_cost: f64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DailyCost {
    pub date: String,
    pub cost: f64,
    pub count: i64,
}
