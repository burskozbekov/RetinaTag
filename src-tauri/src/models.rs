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
    /// Local model via Ollama (e.g. gemma3:4b). No API key required.
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
            AiProvider::Local => "gemma3:4b",
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
    pub description: Option<String>,
    pub estimated_location: Option<String>,
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
    pub media_type: String,
    pub date_taken: Option<String>,
    pub duration_secs: Option<i32>,
    pub rating: i32,
    pub favorite: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TimelineGroup {
    pub date: String,
    pub photos: Vec<PhotoSummary>,
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

#[derive(Debug, Default, Serialize, Deserialize, Clone)]
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
    pub source: String,           // "gps" or "ai"
    pub location_name: Option<String>, // AI-estimated location name
}

// ── Duplicate Detection ─────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct DuplicateGroup {
    pub hash: String,
    pub photos: Vec<PhotoSummary>,
}

/// Cleanup view — rich per-photo info with everything the UI needs to
/// visualize a duplicate group or a blurry-photo row.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct CleanupPhoto {
    pub id: i64,
    pub path: String,
    pub filename: String,
    pub folder: String,
    pub width: i32,
    pub height: i32,
    pub size_bytes: i64,
    pub rating: i32,
    pub favorite: bool,
    pub blur_score: Option<f32>,
    pub date_taken: Option<String>,
    /// Number of user-assigned tags (protect signal).
    pub tag_count: i64,
    /// Number of assigned named persons (protect signal).
    pub person_count: i64,
    /// Number of collections this photo belongs to (protect signal).
    pub collection_count: i64,
    /// True if the photo's status indicates XMP metadata has been written.
    pub has_xmp: bool,
    /// True if ANY investment signal is present — UI shows a lock badge and
    /// auto-select skips these rows.
    pub is_invested: bool,
    /// Computed keeper-score — highest in the group = auto-picked keeper.
    pub keeper_score: f64,
    /// Set by the backend for duplicate groups: true for the auto-picked
    /// keeper, false for every other member (= deletion candidate).
    pub is_keeper: bool,
    /// Short human-readable reasons for the keeper score — shown as tooltip
    /// in the UI (e.g. "24 MP", "sharp (1523)", "❤ favorite").
    pub keeper_reasons: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct CleanupDuplicateGroup {
    pub hash: String,
    pub photos: Vec<CleanupPhoto>,
    /// Total bytes that would be freed if every non-keeper photo is deleted.
    pub bytes_reclaimable: i64,
    /// 0.0–1.0 confidence that these really are duplicates worth acting on.
    /// Currently: 1.0 for exact pHash match (what we support today). Lower
    /// values are reserved for future near-duplicate groups via Hamming
    /// distance so the UI can badge "likely" vs "definite" duplicates.
    pub confidence: f32,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct CleanupSummary {
    pub duplicate_groups: i64,
    pub duplicate_photos: i64,
    pub duplicate_bytes_reclaimable: i64,
    pub blurry_photos: i64,
    pub blurry_bytes: i64,
    pub photos_without_phash: i64,
    pub photos_without_blur_score: i64,
    pub total_photos: i64,
    /// Recommended blur threshold based on the library's own distribution
    /// (10th percentile of existing blur_scores). None if there aren't
    /// enough scored photos yet — UI falls back to 100.
    pub suggested_blur_threshold: Option<f32>,
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
    /// All face IDs in this cluster (for batch skip/assign)
    #[serde(default)]
    pub cluster_face_ids: Vec<i64>,
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

// ── Local Model Presets ─────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct LocalModelPreset {
    pub id: String,
    pub name: String,
    pub ollama_tag: String,
    pub vram_gb: f32,
    pub size_gb: f32,
    pub description: String,
    pub recommended_for: String,
}

pub fn local_model_presets() -> Vec<LocalModelPreset> {
    vec![
        LocalModelPreset {
            id: "moondream".into(),
            name: "Moondream 2".into(),
            ollama_tag: "moondream".into(),
            vram_gb: 1.5,
            size_gb: 1.0,
            description: "Ultra-light vision model, good for basic tagging".into(),
            recommended_for: "CPU only / Intel UHD / GT 1030 (2-4 GB)".into(),
        },
        LocalModelPreset {
            id: "gemma3_4b".into(),
            name: "Gemma 3 4B".into(),
            ollama_tag: "gemma3:4b".into(),
            vram_gb: 3.5,
            size_gb: 3.3,
            description: "Solid quality/VRAM ratio. Free, 128K context, multilingual".into(),
            recommended_for: "GTX 1060 / GTX 1650 / RTX 2060 (4-6 GB)".into(),
        },
        LocalModelPreset {
            id: "qwen25vl_7b".into(),
            name: "Qwen2.5-VL 7B".into(),
            ollama_tag: "qwen2.5vl:7b".into(),
            vram_gb: 5.5,
            size_gb: 4.7,
            description: "Strong all-round accuracy, excellent OCR and detail".into(),
            recommended_for: "RTX 3060 8GB / RTX 2070 (8 GB)".into(),
        },
        LocalModelPreset {
            id: "gemma4_12b".into(),
            name: "Gemma 4 12B".into(),
            ollama_tag: "gemma4:12b".into(),
            vram_gb: 9.0,
            size_gb: 8.1,
            description: "Latest Gemma generation — better vision understanding than Gemma 3".into(),
            recommended_for: "RTX 3060 12GB / RTX 3070 / RTX 4060 (12 GB)".into(),
        },
        LocalModelPreset {
            id: "qwen25vl_32b".into(),
            name: "Qwen2.5-VL 32B ⭐ Best".into(),
            ollama_tag: "qwen2.5vl:32b".into(),
            vram_gb: 20.0,
            size_gb: 19.0,
            description: "Top-tier vision model. Exceptional detail, OCR, scene understanding — best for photo tagging".into(),
            recommended_for: "RTX 3090 / RTX 4090 / RTX 4080 (24 GB) ← senin kartın".into(),
        },
        LocalModelPreset {
            id: "gemma4_27b".into(),
            name: "Gemma 4 27B".into(),
            ollama_tag: "gemma4:27b".into(),
            vram_gb: 18.0,
            size_gb: 17.0,
            description: "Latest Gemma, near cloud-API quality. Great all-rounder".into(),
            recommended_for: "RTX 3090 / RTX 4090 (24 GB)".into(),
        },
    ]
}

// ── Similar Photo Result ───────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SimilarResult {
    pub photo: PhotoSummary,
    pub similarity: f32,
}

// ── Calendar View ──────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
pub struct CalendarDay {
    pub day: u32,
    pub count: i64,
    pub first_photo_id: Option<i64>,
}

// ── Library Analytics ──────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
pub struct LibraryAnalytics {
    pub photos_by_month: Vec<(String, i64)>,
    pub top_tags: Vec<(String, i64)>,
    pub camera_stats: Vec<(String, i64)>,
    pub media_type_breakdown: Vec<(String, i64)>,
    pub rating_distribution: Vec<(i32, i64)>,
    pub top_locations: Vec<(String, i64)>,
    pub storage_by_folder: Vec<(String, i64)>,
    pub total_photos: i64,
    pub total_size_bytes: i64,
    // v1.5.123 — true distinct counts. The dashboard used to display
    // `top_tags.length` ("30") and `top_locations.length` ("20") as
    // "Unique Tags" and "Locations" headline numbers, which silently
    // showed the SQL LIMITs instead of the real library size.
    #[serde(default)]
    pub total_unique_tags: i64,
    #[serde(default)]
    pub total_unique_locations: i64,
}

// ── Health Check ───────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
pub struct HealthReport {
    pub orphaned_entries: Vec<(i64, String)>,
    pub missing_thumbnails: i64,
    pub total_checked: i64,
}

// ── Smart Rename ───────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
pub struct RenamePreview {
    pub photo_id: i64,
    pub old_name: String,
    pub new_name: String,
    pub old_path: String,
    pub new_path: String,
}
