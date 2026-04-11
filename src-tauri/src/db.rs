use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};

use crate::models::{AppStats, Photo, PhotoSummary, TagEntry};

pub fn init_db(path: &str) -> Result<Connection> {
    let conn = Connection::open(path).context("open db")?;

    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA foreign_keys = ON;
         PRAGMA synchronous = NORMAL;
         PRAGMA cache_size = -8000;",
    )?;

    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS photos (
            id             INTEGER PRIMARY KEY AUTOINCREMENT,
            path           TEXT    UNIQUE NOT NULL,
            filename       TEXT    NOT NULL,
            folder         TEXT    NOT NULL,
            hash           TEXT    NOT NULL,
            size           INTEGER NOT NULL,
            width          INTEGER,
            height         INTEGER,
            created_at     TEXT    NOT NULL,
            tagged_at      TEXT,
            thumbnail_path TEXT,
            status         TEXT    NOT NULL DEFAULT 'pending',
            provider_used  TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_photos_folder   ON photos(folder);
        CREATE INDEX IF NOT EXISTS idx_photos_status   ON photos(status);
        CREATE INDEX IF NOT EXISTS idx_photos_hash     ON photos(hash);
        CREATE INDEX IF NOT EXISTS idx_photos_provider ON photos(provider_used);

        CREATE TABLE IF NOT EXISTS tags (
            id         INTEGER PRIMARY KEY AUTOINCREMENT,
            photo_id   INTEGER NOT NULL REFERENCES photos(id) ON DELETE CASCADE,
            tag        TEXT    NOT NULL,
            confidence REAL,
            source     TEXT,
            UNIQUE(photo_id, tag)
        );
        CREATE INDEX IF NOT EXISTS idx_tags_photo ON tags(photo_id);
        CREATE INDEX IF NOT EXISTS idx_tags_tag   ON tags(tag);

        -- FTS5 for full-text tag search
        CREATE VIRTUAL TABLE IF NOT EXISTS tags_fts USING fts5(
            tag,
            photo_id UNINDEXED,
            content = 'tags',
            content_rowid = 'id'
        );

        -- Triggers to keep FTS in sync
        CREATE TRIGGER IF NOT EXISTS tags_ai AFTER INSERT ON tags BEGIN
            INSERT INTO tags_fts(rowid, tag, photo_id) VALUES (new.id, new.tag, new.photo_id);
        END;
        CREATE TRIGGER IF NOT EXISTS tags_ad AFTER DELETE ON tags BEGIN
            INSERT INTO tags_fts(tags_fts, rowid, tag, photo_id) VALUES('delete', old.id, old.tag, old.photo_id);
        END;
        CREATE TRIGGER IF NOT EXISTS tags_au AFTER UPDATE ON tags BEGIN
            INSERT INTO tags_fts(tags_fts, rowid, tag, photo_id) VALUES('delete', old.id, old.tag, old.photo_id);
            INSERT INTO tags_fts(rowid, tag, photo_id) VALUES (new.id, new.tag, new.photo_id);
        END;

        -- Key-value settings store
        CREATE TABLE IF NOT EXISTS settings (
            key   TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );

        -- Provider usage tracking
        CREATE TABLE IF NOT EXISTS provider_usage (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            provider    TEXT NOT NULL,
            photo_id    INTEGER REFERENCES photos(id) ON DELETE SET NULL,
            success     INTEGER NOT NULL DEFAULT 1,
            cost_usd    REAL NOT NULL DEFAULT 0,
            created_at  TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_usage_provider ON provider_usage(provider);

        -- Translation cache for multi-language search
        CREATE TABLE IF NOT EXISTS translation_cache (
            original_text TEXT NOT NULL,
            source_lang   TEXT,
            translated    TEXT NOT NULL,
            provider_used TEXT,
            created_at    TEXT NOT NULL,
            PRIMARY KEY (original_text, source_lang)
        );

        -- Collections / Smart Albums
        CREATE TABLE IF NOT EXISTS collections (
            id              INTEGER PRIMARY KEY AUTOINCREMENT,
            name            TEXT    NOT NULL,
            collection_type TEXT    NOT NULL DEFAULT 'manual',
            rules_json      TEXT,
            created_at      TEXT    NOT NULL
        );
        CREATE TABLE IF NOT EXISTS collection_photos (
            collection_id INTEGER NOT NULL REFERENCES collections(id) ON DELETE CASCADE,
            photo_id      INTEGER NOT NULL REFERENCES photos(id) ON DELETE CASCADE,
            PRIMARY KEY (collection_id, photo_id)
        );

        -- Watch Folders
        CREATE TABLE IF NOT EXISTS watch_folders (
            id           INTEGER PRIMARY KEY AUTOINCREMENT,
            path         TEXT    UNIQUE NOT NULL,
            enabled      INTEGER NOT NULL DEFAULT 1,
            auto_tag     INTEGER NOT NULL DEFAULT 0,
            last_checked TEXT,
            created_at   TEXT    NOT NULL
        );

        -- GPS coordinates (extracted from EXIF)
        ALTER TABLE photos ADD COLUMN gps_lat REAL;",
    )
    .ok(); // ok() because ALTER TABLE fails if column already exists

    conn.execute_batch("ALTER TABLE photos ADD COLUMN gps_lon REAL;").ok();
    conn.execute_batch("ALTER TABLE photos ADD COLUMN gps_alt REAL;").ok();

    // Perceptual hash for duplicate detection
    conn.execute_batch("ALTER TABLE photos ADD COLUMN phash TEXT;").ok();

    // Create indexes for GPS and phash
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_photos_gps ON photos(gps_lat, gps_lon);
         CREATE INDEX IF NOT EXISTS idx_photos_phash ON photos(phash);",
    )?;

    // CLIP semantic embedding column (added later, ignore if exists)
    conn.execute_batch("ALTER TABLE photos ADD COLUMN clip_emb BLOB;").ok();
    conn.execute_batch("ALTER TABLE photos ADD COLUMN clip_tier TEXT;").ok();

    // Face recognition tables (idempotent)
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS persons (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            name        TEXT NOT NULL UNIQUE,
            thumbnail   TEXT,
            created_at  TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_persons_name ON persons(name);

        CREATE TABLE IF NOT EXISTS face_regions (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            photo_id    INTEGER NOT NULL REFERENCES photos(id) ON DELETE CASCADE,
            x1          INTEGER NOT NULL,
            y1          INTEGER NOT NULL,
            x2          INTEGER NOT NULL,
            y2          INTEGER NOT NULL,
            score       REAL NOT NULL DEFAULT 0,
            embedding   BLOB,
            person_id   INTEGER REFERENCES persons(id) ON DELETE SET NULL,
            created_at  TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_faces_photo  ON face_regions(photo_id);
        CREATE INDEX IF NOT EXISTS idx_faces_person ON face_regions(person_id);",
    )
    .ok(); // ok() — safe to run repeatedly

    Ok(conn)
}

// ── Settings ─────────────────────────────────────────────────────────────────

pub fn get_setting(conn: &Connection, key: &str) -> Result<Option<String>> {
    match conn.query_row(
        "SELECT value FROM settings WHERE key = ?1",
        params![key],
        |r| r.get(0),
    ) {
        Ok(v) => Ok(Some(v)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub fn set_setting(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO settings (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = ?2",
        params![key, value],
    )?;
    Ok(())
}

pub fn get_all_settings(conn: &Connection) -> Result<Vec<(String, String)>> {
    let mut stmt = conn.prepare("SELECT key, value FROM settings ORDER BY key")?;
    let rows = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows)
}

pub fn delete_setting(conn: &Connection, key: &str) -> Result<()> {
    conn.execute("DELETE FROM settings WHERE key = ?1", params![key])?;
    Ok(())
}

// ── Photo writes ─────────────────────────────────────────────────────────────

pub fn photo_exists_by_hash(conn: &Connection, hash: &str) -> Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM photos WHERE hash = ?1",
        params![hash],
        |r| r.get(0),
    )?;
    Ok(count > 0)
}

pub struct NewPhoto<'a> {
    pub path: &'a str,
    pub filename: &'a str,
    pub folder: &'a str,
    pub hash: &'a str,
    pub size: i64,
    pub width: Option<i32>,
    pub height: Option<i32>,
}

pub fn insert_photo(conn: &Connection, p: &NewPhoto<'_>) -> Result<i64> {
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "INSERT OR IGNORE INTO photos
             (path, filename, folder, hash, size, width, height, created_at, status)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'pending')",
        params![p.path, p.filename, p.folder, p.hash, p.size, p.width, p.height, now],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn update_photo_status(conn: &Connection, id: i64, status: &str) -> Result<()> {
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "UPDATE photos SET status = ?1, tagged_at = ?2 WHERE id = ?3",
        params![status, now, id],
    )?;
    Ok(())
}

pub fn update_photo_provider(conn: &Connection, id: i64, provider: &str) -> Result<()> {
    conn.execute(
        "UPDATE photos SET provider_used = ?1 WHERE id = ?2",
        params![provider, id],
    )?;
    Ok(())
}

pub fn update_thumbnail_path(conn: &Connection, id: i64, thumb_path: &str) -> Result<()> {
    conn.execute(
        "UPDATE photos SET thumbnail_path = ?1 WHERE id = ?2",
        params![thumb_path, id],
    )?;
    Ok(())
}

// ── Tag writes ───────────────────────────────────────────────────────────────

pub fn insert_tags(
    conn: &Connection,
    photo_id: i64,
    tags: &[(String, f64, String)],
) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    {
        let mut stmt = tx.prepare_cached(
            "INSERT OR IGNORE INTO tags (photo_id, tag, confidence, source) VALUES (?1, ?2, ?3, ?4)",
        )?;
        for (tag, conf, source) in tags {
            stmt.execute(params![photo_id, tag, conf, source])?;
        }
    }
    tx.commit()?;
    Ok(())
}

pub fn add_manual_tag(conn: &Connection, photo_id: i64, tag: &str) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO tags (photo_id, tag, confidence, source) VALUES (?1, ?2, 1.0, 'manual')",
        params![photo_id, tag],
    )?;
    Ok(())
}

pub fn delete_tag(conn: &Connection, photo_id: i64, tag: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM tags WHERE photo_id = ?1 AND tag = ?2",
        params![photo_id, tag],
    )?;
    Ok(())
}

// ── Provider usage tracking ──────────────────────────────────────────────────

pub fn record_usage(
    conn: &Connection,
    provider: &str,
    photo_id: i64,
    success: bool,
    cost_usd: f64,
) -> Result<()> {
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO provider_usage (provider, photo_id, success, cost_usd, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![provider, photo_id, success as i32, cost_usd, now],
    )?;
    Ok(())
}

pub fn get_provider_stats(conn: &Connection, provider: &str) -> Result<(i64, i64, f64)> {
    let total_ok: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM provider_usage WHERE provider = ?1 AND success = 1",
            params![provider],
            |r| r.get(0),
        )
        .unwrap_or(0);
    let total_err: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM provider_usage WHERE provider = ?1 AND success = 0",
            params![provider],
            |r| r.get(0),
        )
        .unwrap_or(0);
    let total_cost: f64 = conn
        .query_row(
            "SELECT COALESCE(SUM(cost_usd), 0) FROM provider_usage WHERE provider = ?1",
            params![provider],
            |r| r.get(0),
        )
        .unwrap_or(0.0);
    Ok((total_ok, total_err, total_cost))
}

// ── Translation cache ────────────────────────────────────────────────────────

pub fn get_cached_translation(conn: &Connection, text: &str) -> Result<Option<String>> {
    match conn.query_row(
        "SELECT translated FROM translation_cache WHERE original_text = ?1",
        params![text],
        |r| r.get(0),
    ) {
        Ok(v) => Ok(Some(v)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub fn cache_translation(
    conn: &Connection,
    original: &str,
    source_lang: Option<&str>,
    translated: &str,
    provider: &str,
) -> Result<()> {
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "INSERT OR REPLACE INTO translation_cache
             (original_text, source_lang, translated, provider_used, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![original, source_lang.unwrap_or("auto"), translated, provider, now],
    )?;
    Ok(())
}

// ── Photo reads ──────────────────────────────────────────────────────────────

pub fn get_photos(
    conn: &Connection,
    offset: i64,
    limit: i64,
    folder: Option<&str>,
    tag_filter: Option<&str>,
    status_filter: Option<&str>,
) -> Result<(Vec<PhotoSummary>, i64)> {
    let mut conditions: Vec<String> = vec![];
    let mut args: Vec<Box<dyn rusqlite::ToSql>> = vec![];

    if let Some(f) = folder {
        args.push(Box::new(f.to_string()));
        conditions.push(format!("p.folder = ?{}", args.len()));
    }
    if let Some(s) = status_filter {
        args.push(Box::new(s.to_string()));
        conditions.push(format!("p.status = ?{}", args.len()));
    }
    if let Some(t) = tag_filter {
        args.push(Box::new(t.to_string()));
        conditions.push(format!(
            "p.id IN (SELECT photo_id FROM tags WHERE tag = ?{})",
            args.len()
        ));
    }

    let where_clause = if conditions.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", conditions.join(" AND "))
    };

    let count_sql = format!("SELECT COUNT(*) FROM photos p {}", where_clause);
    let total: i64 = {
        let mut stmt = conn.prepare(&count_sql)?;
        let args_refs: Vec<&dyn rusqlite::ToSql> = args.iter().map(|a| a.as_ref()).collect();
        stmt.query_row(args_refs.as_slice(), |r| r.get(0))?
    };

    let fetch_sql = format!(
        "SELECT p.id, p.path, p.filename, p.status, p.provider_used,
                (SELECT COUNT(*) FROM tags WHERE photo_id = p.id) AS tag_count,
                COALESCE((SELECT GROUP_CONCAT(tag, '|||') FROM (SELECT tag FROM tags WHERE photo_id = p.id LIMIT 10)), '') AS tag_list
         FROM photos p
         {}
         ORDER BY p.created_at DESC
         LIMIT ?{} OFFSET ?{}",
        where_clause,
        args.len() + 1,
        args.len() + 2
    );

    args.push(Box::new(limit));
    args.push(Box::new(offset));

    let mut stmt = conn.prepare(&fetch_sql)?;
    let args_refs: Vec<&dyn rusqlite::ToSql> = args.iter().map(|a| a.as_ref()).collect();

    let photos = stmt
        .query_map(args_refs.as_slice(), |row| {
            let tag_list: String = row.get(6)?;
            let tags: Vec<String> = if tag_list.is_empty() {
                vec![]
            } else {
                tag_list.split("|||").map(|s| s.to_string()).collect()
            };
            Ok(PhotoSummary {
                id: row.get(0)?,
                path: row.get(1)?,
                filename: row.get(2)?,
                status: row.get(3)?,
                provider_used: row.get(4)?,
                tag_count: row.get(5)?,
                tags,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();

    Ok((photos, total))
}

pub fn get_photo_detail(conn: &Connection, id: i64) -> Result<Photo> {
    let photo = conn.query_row(
        "SELECT id, path, filename, folder, hash, size, width, height,
                created_at, tagged_at, thumbnail_path, status, provider_used
         FROM photos WHERE id = ?1",
        params![id],
        |row| {
            Ok(Photo {
                id: row.get(0)?,
                path: row.get(1)?,
                filename: row.get(2)?,
                folder: row.get(3)?,
                hash: row.get(4)?,
                size: row.get(5)?,
                width: row.get(6)?,
                height: row.get(7)?,
                created_at: row.get(8)?,
                tagged_at: row.get(9)?,
                thumbnail_path: row.get(10)?,
                status: row.get(11)?,
                provider_used: row.get(12)?,
                tags: vec![],
            })
        },
    )?;

    let mut tag_stmt = conn.prepare(
        "SELECT tag, confidence, source FROM tags WHERE photo_id = ?1 ORDER BY tag",
    )?;
    let tags: Vec<TagEntry> = tag_stmt
        .query_map(params![id], |row| {
            Ok(TagEntry {
                tag: row.get(0)?,
                confidence: row.get(1)?,
                source: row.get(2)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();

    Ok(Photo { tags, ..photo })
}

pub fn search_photos_fts(conn: &Connection, query: &str) -> Result<Vec<PhotoSummary>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT t.photo_id
         FROM tags_fts f
         JOIN tags t ON t.id = f.rowid
         WHERE tags_fts MATCH ?1
         LIMIT 500",
    )?;

    let ids: Vec<i64> = stmt
        .query_map(params![query], |r| r.get(0))?
        .filter_map(|r| r.ok())
        .collect();

    if ids.is_empty() {
        return Ok(vec![]);
    }

    let placeholders: String = ids
        .iter()
        .enumerate()
        .map(|(i, _)| format!("?{}", i + 1))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "SELECT p.id, p.path, p.filename, p.status, p.provider_used,
                (SELECT COUNT(*) FROM tags WHERE photo_id = p.id) AS tag_count,
                COALESCE((SELECT GROUP_CONCAT(tag, '|||') FROM (SELECT tag FROM tags WHERE photo_id = p.id LIMIT 10)), '') AS tag_list
         FROM photos p
         WHERE p.id IN ({})
         ORDER BY p.tagged_at DESC",
        placeholders
    );

    let mut stmt = conn.prepare(&sql)?;
    let args: Vec<Box<dyn rusqlite::ToSql>> =
        ids.iter().map(|id| Box::new(*id) as Box<dyn rusqlite::ToSql>).collect();
    let args_refs: Vec<&dyn rusqlite::ToSql> = args.iter().map(|a| a.as_ref()).collect();

    let photos = stmt
        .query_map(args_refs.as_slice(), |row| {
            let tag_list: String = row.get(6)?;
            let tags: Vec<String> = if tag_list.is_empty() {
                vec![]
            } else {
                tag_list.split("|||").map(|s| s.to_string()).collect()
            };
            Ok(PhotoSummary {
                id: row.get(0)?,
                path: row.get(1)?,
                filename: row.get(2)?,
                status: row.get(3)?,
                provider_used: row.get(4)?,
                tag_count: row.get(5)?,
                tags,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();

    Ok(photos)
}

/// Search for photos whose tags match ANY of the provided terms (OR logic).
/// Used for multi-language search where the translator returns multiple terms.
pub fn search_photos_multi(conn: &Connection, terms: &[String]) -> Result<Vec<PhotoSummary>> {
    if terms.is_empty() {
        return Ok(vec![]);
    }

    // Build a FTS5 OR query: "term1" OR "term2" OR "term3"
    let fts_query = terms
        .iter()
        .map(|t| format!("\"{}\"", t.replace('"', "")))
        .collect::<Vec<_>>()
        .join(" OR ");

    search_photos_fts(conn, &fts_query)
}

pub fn get_stats(conn: &Connection) -> Result<AppStats> {
    let total: i64 = conn.query_row("SELECT COUNT(*) FROM photos", [], |r| r.get(0))?;
    let tagged: i64 = conn.query_row(
        "SELECT COUNT(*) FROM photos WHERE status = 'tagged'",
        [],
        |r| r.get(0),
    )?;
    let pending: i64 = conn.query_row(
        "SELECT COUNT(*) FROM photos WHERE status = 'pending'",
        [],
        |r| r.get(0),
    )?;
    let error: i64 = conn.query_row(
        "SELECT COUNT(*) FROM photos WHERE status = 'error'",
        [],
        |r| r.get(0),
    )?;
    let total_tags: i64 = conn.query_row("SELECT COUNT(*) FROM tags", [], |r| r.get(0))?;
    let unique_tags: i64 =
        conn.query_row("SELECT COUNT(DISTINCT tag) FROM tags", [], |r| r.get(0))?;
    let folders: i64 =
        conn.query_row("SELECT COUNT(DISTINCT folder) FROM photos", [], |r| r.get(0))?;
    let total_cost: f64 = conn
        .query_row(
            "SELECT COALESCE(SUM(cost_usd), 0) FROM provider_usage",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0.0);

    Ok(AppStats {
        total_photos: total,
        tagged_photos: tagged,
        pending_photos: pending,
        error_photos: error,
        total_tags,
        unique_tags,
        folders_scanned: folders,
        total_cost_usd: total_cost,
    })
}

pub fn get_folders(conn: &Connection) -> Result<Vec<(String, i64)>> {
    let mut stmt = conn.prepare(
        "SELECT folder, COUNT(*) AS cnt FROM photos GROUP BY folder ORDER BY folder",
    )?;
    let folders = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .filter_map(|r| r.ok())
        .collect();
    Ok(folders)
}

pub fn get_pending_photos(conn: &Connection) -> Result<Vec<(i64, String)>> {
    let mut stmt =
        conn.prepare("SELECT id, path FROM photos WHERE status = 'pending' ORDER BY id")?;
    let rows = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows)
}

/// Clear all tags and reset all photos to 'pending' for re-tagging
pub fn clear_all_tags(conn: &Connection) -> Result<usize> {
    conn.execute("DELETE FROM tags", [])?;
    conn.execute("DELETE FROM tags_fts", [])?;
    let count = conn.execute("UPDATE photos SET status = 'pending', tagged_at = NULL", [])?;
    // Also clear CLIP embeddings so semantic re-index happens
    conn.execute("UPDATE photos SET clip_emb = NULL, clip_tier = NULL WHERE clip_emb IS NOT NULL", []).ok();
    // Clear translation cache
    conn.execute("DELETE FROM translation_cache", []).ok();
    Ok(count)
}

/// Reset all 'error' status photos back to 'pending' so they can be retried
pub fn reset_error_photos(conn: &Connection) -> Result<usize> {
    let count = conn.execute(
        "UPDATE photos SET status = 'pending' WHERE status = 'error'",
        [],
    )?;
    Ok(count)
}

/// Get count of photos by status
pub fn get_status_counts(conn: &Connection) -> Result<(usize, usize, usize, usize)> {
    let pending: usize = conn.query_row("SELECT COUNT(*) FROM photos WHERE status='pending'", [], |r| r.get(0)).unwrap_or(0);
    let tagged: usize = conn.query_row("SELECT COUNT(*) FROM photos WHERE status='tagged'", [], |r| r.get(0)).unwrap_or(0);
    let error: usize = conn.query_row("SELECT COUNT(*) FROM photos WHERE status='error'", [], |r| r.get(0)).unwrap_or(0);
    let total: usize = conn.query_row("SELECT COUNT(*) FROM photos", [], |r| r.get(0)).unwrap_or(0);
    Ok((total, pending, tagged, error))
}

pub fn get_photo_path_and_hash(conn: &Connection, id: i64) -> Result<(String, String)> {
    Ok(conn.query_row(
        "SELECT path, hash FROM photos WHERE id = ?1",
        params![id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?)
}

pub fn get_photo_thumbnail_path(conn: &Connection, id: i64) -> Result<Option<String>> {
    match conn.query_row(
        "SELECT thumbnail_path FROM photos WHERE id = ?1",
        params![id],
        |r| r.get(0),
    ) {
        Ok(v) => Ok(v),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

// ── Collections ─────────────────────────────────────────────────────────────

pub fn create_collection(conn: &Connection, name: &str, ctype: &str, rules_json: Option<&str>) -> Result<i64> {
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO collections (name, collection_type, rules_json, created_at) VALUES (?1, ?2, ?3, ?4)",
        params![name, ctype, rules_json, now],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn delete_collection(conn: &Connection, id: i64) -> Result<()> {
    conn.execute("DELETE FROM collections WHERE id = ?1", params![id])?;
    Ok(())
}

pub fn get_collections(conn: &Connection) -> Result<Vec<crate::models::Collection>> {
    let mut stmt = conn.prepare(
        "SELECT c.id, c.name, c.collection_type, c.rules_json, c.created_at,
                (SELECT COUNT(*) FROM collection_photos WHERE collection_id = c.id) AS cnt
         FROM collections c ORDER BY c.name"
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(crate::models::Collection {
            id: r.get(0)?,
            name: r.get(1)?,
            collection_type: r.get(2)?,
            rules_json: r.get(3)?,
            created_at: r.get(4)?,
            photo_count: r.get(5)?,
        })
    })?.filter_map(|r| r.ok()).collect();
    Ok(rows)
}

pub fn add_photo_to_collection(conn: &Connection, collection_id: i64, photo_id: i64) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO collection_photos (collection_id, photo_id) VALUES (?1, ?2)",
        params![collection_id, photo_id],
    )?;
    Ok(())
}

pub fn remove_photo_from_collection(conn: &Connection, collection_id: i64, photo_id: i64) -> Result<()> {
    conn.execute(
        "DELETE FROM collection_photos WHERE collection_id = ?1 AND photo_id = ?2",
        params![collection_id, photo_id],
    )?;
    Ok(())
}

pub fn get_collection_photo_ids(conn: &Connection, collection_id: i64) -> Result<Vec<i64>> {
    let mut stmt = conn.prepare("SELECT photo_id FROM collection_photos WHERE collection_id = ?1")?;
    let ids = stmt.query_map(params![collection_id], |r| r.get(0))?.filter_map(|r| r.ok()).collect();
    Ok(ids)
}

/// Execute smart collection rules to find matching photos
pub fn query_smart_collection(conn: &Connection, rules: &[crate::models::CollectionRule]) -> Result<Vec<PhotoSummary>> {
    let mut conditions = Vec::new();
    let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

    for rule in rules {
        match rule.field.as_str() {
            "tag" => {
                args.push(Box::new(format!("%{}%", rule.value)));
                conditions.push(format!(
                    "p.id IN (SELECT photo_id FROM tags WHERE tag LIKE ?{})", args.len()
                ));
            }
            "folder" => {
                args.push(Box::new(format!("%{}%", rule.value)));
                conditions.push(format!("p.folder LIKE ?{}", args.len()));
            }
            "provider" => {
                args.push(Box::new(rule.value.clone()));
                conditions.push(format!("p.provider_used = ?{}", args.len()));
            }
            "status" => {
                args.push(Box::new(rule.value.clone()));
                conditions.push(format!("p.status = ?{}", args.len()));
            }
            "date_after" => {
                args.push(Box::new(rule.value.clone()));
                conditions.push(format!("p.created_at >= ?{}", args.len()));
            }
            "date_before" => {
                args.push(Box::new(rule.value.clone()));
                conditions.push(format!("p.created_at <= ?{}", args.len()));
            }
            _ => {}
        }
    }

    if conditions.is_empty() {
        return Ok(vec![]);
    }

    let where_clause = conditions.join(" AND ");
    let sql = format!(
        "SELECT p.id, p.path, p.filename, p.status, p.provider_used,
                (SELECT COUNT(*) FROM tags WHERE photo_id = p.id) AS tag_count,
                COALESCE((SELECT GROUP_CONCAT(tag, '|||') FROM (SELECT tag FROM tags WHERE photo_id = p.id LIMIT 10)), '') AS tag_list
         FROM photos p WHERE {} ORDER BY p.created_at DESC LIMIT 1000", where_clause
    );

    let mut stmt = conn.prepare(&sql)?;
    let args_refs: Vec<&dyn rusqlite::ToSql> = args.iter().map(|a| a.as_ref()).collect();
    let photos = stmt.query_map(args_refs.as_slice(), |row| {
        let tag_list: String = row.get(6)?;
        let tags: Vec<String> = if tag_list.is_empty() { vec![] } else { tag_list.split("|||").map(|s| s.to_string()).collect() };
        Ok(PhotoSummary { id: row.get(0)?, path: row.get(1)?, filename: row.get(2)?, status: row.get(3)?, provider_used: row.get(4)?, tag_count: row.get(5)?, tags })
    })?.filter_map(|r| r.ok()).collect();
    Ok(photos)
}

// ── Watch Folders ───────────────────────────────────────────────────────────

pub fn add_watch_folder(conn: &Connection, path: &str, auto_tag: bool) -> Result<i64> {
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "INSERT OR IGNORE INTO watch_folders (path, enabled, auto_tag, created_at) VALUES (?1, 1, ?2, ?3)",
        params![path, auto_tag as i32, now],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn remove_watch_folder(conn: &Connection, id: i64) -> Result<()> {
    conn.execute("DELETE FROM watch_folders WHERE id = ?1", params![id])?;
    Ok(())
}

pub fn get_watch_folders(conn: &Connection) -> Result<Vec<crate::models::WatchFolder>> {
    let mut stmt = conn.prepare(
        "SELECT w.id, w.path, w.enabled, w.auto_tag, w.last_checked,
                (SELECT COUNT(*) FROM photos WHERE folder LIKE w.path || '%') AS cnt
         FROM watch_folders w ORDER BY w.path"
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(crate::models::WatchFolder {
            id: r.get(0)?,
            path: r.get(1)?,
            enabled: r.get::<_, i32>(2)? != 0,
            auto_tag: r.get::<_, i32>(3)? != 0,
            last_checked: r.get(4)?,
            photo_count: r.get(5)?,
        })
    })?.filter_map(|r| r.ok()).collect();
    Ok(rows)
}

pub fn update_watch_folder_checked(conn: &Connection, id: i64) -> Result<()> {
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute("UPDATE watch_folders SET last_checked = ?1 WHERE id = ?2", params![now, id])?;
    Ok(())
}

// ── Tag Management ──────────────────────────────────────────────────────────

pub fn merge_tags(conn: &Connection, source_tag: &str, target_tag: &str) -> Result<usize> {
    let tx = conn.unchecked_transaction()?;
    // Update all instances of source_tag to target_tag, skip if duplicate
    let updated = tx.execute(
        "UPDATE OR IGNORE tags SET tag = ?1 WHERE tag = ?2",
        params![target_tag, source_tag],
    )?;
    // Delete remaining (duplicates that couldn't be updated)
    tx.execute("DELETE FROM tags WHERE tag = ?1", params![source_tag])?;
    tx.commit()?;
    Ok(updated)
}

pub fn rename_tag(conn: &Connection, old_name: &str, new_name: &str) -> Result<usize> {
    let tx = conn.unchecked_transaction()?;
    let updated = tx.execute(
        "UPDATE OR IGNORE tags SET tag = ?1 WHERE tag = ?2",
        params![new_name, old_name],
    )?;
    tx.execute("DELETE FROM tags WHERE tag = ?1", params![old_name])?;
    tx.commit()?;
    Ok(updated)
}

pub fn delete_tag_globally(conn: &Connection, tag: &str) -> Result<usize> {
    let deleted = conn.execute("DELETE FROM tags WHERE tag = ?1", params![tag])?;
    Ok(deleted)
}

pub fn get_tag_details(conn: &Connection) -> Result<Vec<crate::models::TagInfo>> {
    let mut stmt = conn.prepare(
        "SELECT tag, COUNT(*) as cnt,
                GROUP_CONCAT(DISTINCT source) as providers
         FROM tags GROUP BY tag ORDER BY cnt DESC LIMIT 500"
    )?;
    let rows = stmt.query_map([], |r| {
        let providers_str: String = r.get::<_, String>(2).unwrap_or_default();
        let providers = providers_str.split(',').map(|s| s.to_string()).collect();
        Ok(crate::models::TagInfo {
            tag: r.get(0)?,
            count: r.get(1)?,
            providers,
        })
    })?.filter_map(|r| r.ok()).collect();
    Ok(rows)
}

// ── GPS ─────────────────────────────────────────────────────────────────────

pub fn update_photo_gps(conn: &Connection, id: i64, lat: f64, lon: f64, alt: Option<f64>) -> Result<()> {
    conn.execute(
        "UPDATE photos SET gps_lat = ?1, gps_lon = ?2, gps_alt = ?3 WHERE id = ?4",
        params![lat, lon, alt, id],
    )?;
    Ok(())
}

pub fn get_photos_with_gps(conn: &Connection) -> Result<Vec<crate::models::GpsPhoto>> {
    let mut stmt = conn.prepare(
        "SELECT p.id, p.filename, p.gps_lat, p.gps_lon,
                (SELECT COUNT(*) FROM tags WHERE photo_id = p.id) AS tag_count
         FROM photos p WHERE p.gps_lat IS NOT NULL AND p.gps_lon IS NOT NULL
         ORDER BY p.created_at DESC LIMIT 5000"
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(crate::models::GpsPhoto {
            id: r.get(0)?,
            filename: r.get(1)?,
            lat: r.get(2)?,
            lon: r.get(3)?,
            tag_count: r.get(4)?,
        })
    })?.filter_map(|r| r.ok()).collect();
    Ok(rows)
}

// ── Perceptual Hash / Duplicates ────────────────────────────────────────────

pub fn update_photo_phash(conn: &Connection, id: i64, phash: &str) -> Result<()> {
    conn.execute("UPDATE photos SET phash = ?1 WHERE id = ?2", params![phash, id])?;
    Ok(())
}

pub fn get_duplicate_groups(conn: &Connection) -> Result<Vec<(String, Vec<PhotoSummary>)>> {
    // Group by exact phash match first
    let mut stmt = conn.prepare(
        "SELECT phash FROM photos WHERE phash IS NOT NULL GROUP BY phash HAVING COUNT(*) > 1"
    )?;
    let hashes: Vec<String> = stmt.query_map([], |r| r.get(0))?.filter_map(|r| r.ok()).collect();

    let mut groups = Vec::new();
    for hash in hashes {
        let mut stmt2 = conn.prepare(
            "SELECT p.id, p.path, p.filename, p.status, p.provider_used,
                    (SELECT COUNT(*) FROM tags WHERE photo_id = p.id) AS tag_count,
                    COALESCE((SELECT GROUP_CONCAT(tag, '|||') FROM (SELECT tag FROM tags WHERE photo_id = p.id LIMIT 10)), '') AS tag_list
             FROM photos p WHERE p.phash = ?1"
        )?;
        let photos: Vec<PhotoSummary> = stmt2.query_map(params![hash], |row| {
            let tag_list: String = row.get(6)?;
            let tags: Vec<String> = if tag_list.is_empty() { vec![] } else { tag_list.split("|||").map(|s| s.to_string()).collect() };
            Ok(PhotoSummary { id: row.get(0)?, path: row.get(1)?, filename: row.get(2)?, status: row.get(3)?, provider_used: row.get(4)?, tag_count: row.get(5)?, tags })
        })?.filter_map(|r| r.ok()).collect();

        if photos.len() > 1 {
            groups.push((hash, photos));
        }
    }
    Ok(groups)
}

// ── Cost Dashboard ──────────────────────────────────────────────────────────

pub fn get_cost_dashboard(conn: &Connection) -> Result<crate::models::CostDashboard> {
    let total_cost: f64 = conn.query_row(
        "SELECT COALESCE(SUM(cost_usd), 0) FROM provider_usage", [], |r| r.get(0)
    ).unwrap_or(0.0);

    let total_tagged: i64 = conn.query_row(
        "SELECT COUNT(*) FROM provider_usage WHERE success = 1", [], |r| r.get(0)
    ).unwrap_or(0);

    let avg_cost = if total_tagged > 0 { total_cost / total_tagged as f64 } else { 0.0 };

    // Estimated savings: if all images were tagged by most expensive provider (Grok $0.0005)
    let estimated_savings = (total_tagged as f64 * 0.0005) - total_cost;

    // Per-provider breakdown
    let mut stmt = conn.prepare(
        "SELECT provider, COUNT(*), COALESCE(SUM(cost_usd), 0) FROM provider_usage WHERE success = 1 GROUP BY provider"
    )?;
    let provider_costs: Vec<crate::models::ProviderCostInfo> = stmt.query_map([], |r| {
        let count: i64 = r.get(1)?;
        let cost: f64 = r.get(2)?;
        Ok(crate::models::ProviderCostInfo {
            provider: r.get(0)?,
            count,
            cost,
            avg_cost: if count > 0 { cost / count as f64 } else { 0.0 },
        })
    })?.filter_map(|r| r.ok()).collect();

    // Daily costs (last 30 days)
    let mut stmt2 = conn.prepare(
        "SELECT DATE(created_at) as d, COALESCE(SUM(cost_usd), 0), COUNT(*)
         FROM provider_usage WHERE success = 1 AND created_at >= DATE('now', '-30 days')
         GROUP BY d ORDER BY d"
    )?;
    let daily_costs: Vec<crate::models::DailyCost> = stmt2.query_map([], |r| {
        Ok(crate::models::DailyCost { date: r.get(0)?, cost: r.get(1)?, count: r.get(2)? })
    })?.filter_map(|r| r.ok()).collect();

    Ok(crate::models::CostDashboard {
        total_cost,
        total_tagged,
        avg_cost_per_image: avg_cost,
        estimated_savings: estimated_savings.max(0.0),
        provider_costs,
        daily_costs,
    })
}

pub fn get_monthly_spend(conn: &Connection) -> Result<f64> {
    let cost: f64 = conn.query_row(
        "SELECT COALESCE(SUM(cost_usd), 0) FROM provider_usage
         WHERE created_at >= DATE('now', 'start of month')",
        [], |r| r.get(0),
    ).unwrap_or(0.0);
    Ok(cost)
}

// ── Photos without phash (for background computation) ───────────────────────

pub fn get_photos_without_phash(conn: &Connection, limit: i64) -> Result<Vec<(i64, String)>> {
    let mut stmt = conn.prepare(
        "SELECT id, path FROM photos WHERE phash IS NULL ORDER BY id LIMIT ?1"
    )?;
    let rows = stmt.query_map(params![limit], |r| Ok((r.get(0)?, r.get(1)?)))?.filter_map(|r| r.ok()).collect();
    Ok(rows)
}

/// Get all unique tags, optionally filtered by prefix (for autocomplete)
pub fn get_all_tags(conn: &Connection, prefix: Option<&str>) -> Result<Vec<(String, i64)>> {
    let (sql, args): (String, Vec<Box<dyn rusqlite::ToSql>>) = if let Some(p) = prefix {
        (
            "SELECT tag, COUNT(*) AS cnt FROM tags WHERE tag LIKE ?1 GROUP BY tag ORDER BY cnt DESC LIMIT 50".to_string(),
            vec![Box::new(format!("{}%", p))],
        )
    } else {
        (
            "SELECT tag, COUNT(*) AS cnt FROM tags GROUP BY tag ORDER BY cnt DESC LIMIT 200".to_string(),
            vec![],
        )
    };

    let mut stmt = conn.prepare(&sql)?;
    let args_refs: Vec<&dyn rusqlite::ToSql> = args.iter().map(|a| a.as_ref()).collect();
    let tags = stmt
        .query_map(args_refs.as_slice(), |r| Ok((r.get(0)?, r.get(1)?)))?
        .filter_map(|r| r.ok())
        .collect();
    Ok(tags)
}

// ── Face Recognition ─────────────────────────────────────────────────────────

pub struct FaceRegionRow {
    pub id: i64,
    pub x1: i32,
    pub y1: i32,
    pub x2: i32,
    pub y2: i32,
    pub score: f32,
    pub embedding_bytes: Option<Vec<u8>>,
    pub person_id: Option<i64>,
    pub person_name: Option<String>,
}

pub struct PersonRow {
    pub id: i64,
    pub name: String,
    pub thumbnail: Option<String>,
    pub face_count: i64,
}

pub fn insert_face_region(
    conn: &Connection,
    photo_id: i64,
    x1: i32, y1: i32, x2: i32, y2: i32,
    score: f32,
    embedding: &[u8],
) -> Result<i64> {
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO face_regions (photo_id, x1, y1, x2, y2, score, embedding, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![photo_id, x1, y1, x2, y2, score as f64, embedding, now],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn delete_faces_for_photo(conn: &Connection, photo_id: i64) -> Result<()> {
    conn.execute("DELETE FROM face_regions WHERE photo_id = ?1", params![photo_id])?;
    Ok(())
}

pub fn get_faces_for_photo(conn: &Connection, photo_id: i64) -> Result<Vec<FaceRegionRow>> {
    let mut stmt = conn.prepare(
        "SELECT f.id, f.x1, f.y1, f.x2, f.y2, f.score, f.embedding, f.person_id, p.name
         FROM face_regions f
         LEFT JOIN persons p ON f.person_id = p.id
         WHERE f.photo_id = ?1
         ORDER BY f.x1",
    )?;
    let rows = stmt
        .query_map(params![photo_id], |r| {
            Ok(FaceRegionRow {
                id: r.get(0)?,
                x1: r.get(1)?,
                y1: r.get(2)?,
                x2: r.get(3)?,
                y2: r.get(4)?,
                score: r.get::<_, f64>(5)? as f32,
                embedding_bytes: r.get(6)?,
                person_id: r.get(7)?,
                person_name: r.get(8)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows)
}

pub fn assign_face_to_person(
    conn: &Connection,
    face_id: i64,
    person_id: Option<i64>,
) -> Result<()> {
    conn.execute(
        "UPDATE face_regions SET person_id = ?1 WHERE id = ?2",
        params![person_id, face_id],
    )?;
    Ok(())
}

/// Simple struct for a face region with photo_id
pub struct FaceRegionFull {
    pub id: i64,
    pub photo_id: i64,
    pub x1: i32, pub y1: i32, pub x2: i32, pub y2: i32,
    pub score: f32,
    pub person_id: Option<i64>,
    pub person_name: Option<String>,
}

/// Get a single face region by ID
pub fn get_face_region(conn: &Connection, face_id: i64) -> Result<Option<FaceRegionFull>> {
    let mut stmt = conn.prepare(
        "SELECT fr.id, fr.photo_id, fr.x1, fr.y1, fr.x2, fr.y2, fr.score, fr.person_id, p.name
         FROM face_regions fr LEFT JOIN persons p ON fr.person_id = p.id
         WHERE fr.id = ?1"
    )?;
    let row = stmt.query_row(params![face_id], |r| {
        Ok(FaceRegionFull {
            id: r.get(0)?,
            photo_id: r.get(1)?,
            x1: r.get(2)?, y1: r.get(3)?, x2: r.get(4)?, y2: r.get(5)?,
            score: r.get(6)?,
            person_id: r.get(7)?,
            person_name: r.get(8)?,
        })
    }).optional()?;
    Ok(row)
}

/// Find person by name, returns person ID
pub fn find_person_by_name(conn: &Connection, name: &str) -> Result<Option<i64>> {
    let id = conn.query_row(
        "SELECT id FROM persons WHERE name = ?1 COLLATE NOCASE",
        params![name],
        |r| r.get(0),
    ).optional()?;
    Ok(id)
}

/// Returns (face_id, photo_id, embedding_bytes) for unassigned faces with embeddings.
pub fn get_unassigned_faces_with_embeddings(
    conn: &Connection,
) -> Result<Vec<(i64, i64, Vec<u8>)>> {
    let mut stmt = conn.prepare(
        "SELECT id, photo_id, embedding FROM face_regions
         WHERE embedding IS NOT NULL AND person_id IS NULL",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, Vec<u8>>(2)?,
            ))
        })?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows)
}

/// Returns all (person_id, person_name, embedding_bytes) for faces already assigned to persons.
pub fn get_known_face_embeddings(
    conn: &Connection,
) -> Result<Vec<(i64, String, Vec<u8>)>> {
    let mut stmt = conn.prepare(
        "SELECT p.id, p.name, f.embedding
         FROM persons p
         INNER JOIN face_regions f ON f.person_id = p.id
         WHERE f.embedding IS NOT NULL",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Vec<u8>>(2)?,
            ))
        })?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows)
}

pub fn create_person(conn: &Connection, name: &str) -> Result<i64> {
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO persons (name, created_at) VALUES (?1, ?2)",
        params![name, now],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn get_persons(conn: &Connection) -> Result<Vec<PersonRow>> {
    let mut stmt = conn.prepare(
        "SELECT p.id, p.name, p.thumbnail, COUNT(f.id) as face_count
         FROM persons p
         LEFT JOIN face_regions f ON f.person_id = p.id
         GROUP BY p.id
         ORDER BY p.name",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok(PersonRow {
                id: r.get(0)?,
                name: r.get(1)?,
                thumbnail: r.get(2)?,
                face_count: r.get(3)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows)
}

pub fn delete_person(conn: &Connection, person_id: i64) -> Result<()> {
    conn.execute("DELETE FROM persons WHERE id = ?1", params![person_id])?;
    Ok(())
}

pub fn rename_person(conn: &Connection, person_id: i64, new_name: &str) -> Result<()> {
    conn.execute(
        "UPDATE persons SET name = ?1 WHERE id = ?2",
        params![new_name, person_id],
    )?;
    Ok(())
}

pub fn update_person_thumbnail(conn: &Connection, person_id: i64, thumb: &str) -> Result<()> {
    conn.execute(
        "UPDATE persons SET thumbnail = ?1 WHERE id = ?2",
        params![thumb, person_id],
    )?;
    Ok(())
}

/// True if there are already detected faces for this photo.
pub fn photo_has_faces(conn: &Connection, photo_id: i64) -> bool {
    conn.query_row(
        "SELECT COUNT(*) FROM face_regions WHERE photo_id = ?1",
        params![photo_id],
        |r| r.get::<_, i64>(0),
    )
    .unwrap_or(0)
        > 0
}

// ── CLIP Semantic Search ──────────────────────────────────────────────────────

/// Save a CLIP embedding for a photo.
pub fn save_clip_embedding(
    conn: &Connection,
    photo_id: i64,
    emb_bytes: &[u8],
    tier: &str,
) -> Result<()> {
    conn.execute(
        "UPDATE photos SET clip_emb = ?1, clip_tier = ?2 WHERE id = ?3",
        params![emb_bytes, tier, photo_id],
    )?;
    Ok(())
}

/// Returns (photo_id, clip_emb_bytes) for all photos with a CLIP embedding
/// matching the given tier (or any tier if tier is empty).
pub fn get_photos_with_clip_emb(
    conn: &Connection,
    tier: &str,
) -> Result<Vec<(i64, Vec<u8>)>> {
    if tier.is_empty() {
        let mut stmt = conn.prepare(
            "SELECT id, clip_emb FROM photos WHERE clip_emb IS NOT NULL",
        )?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?)))?
            .filter_map(|r| r.ok())
            .collect();
        Ok(rows)
    } else {
        let mut stmt = conn.prepare(
            "SELECT id, clip_emb FROM photos WHERE clip_emb IS NOT NULL AND clip_tier = ?1",
        )?;
        let rows = stmt
            .query_map(params![tier], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?)))?
            .filter_map(|r| r.ok())
            .collect();
        Ok(rows)
    }
}

/// Returns photo_ids that do NOT yet have a CLIP embedding for the given tier.
pub fn get_photos_without_clip_emb(conn: &Connection, tier: &str) -> Result<Vec<(i64, String)>> {
    let mut stmt = conn.prepare(
        "SELECT id, path FROM photos WHERE clip_emb IS NULL OR clip_tier != ?1",
    )?;
    let rows: Vec<(i64, String)> = stmt
        .query_map(params![tier], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows)
}

/// Count how many photos have CLIP embeddings for a given tier.
pub fn count_clip_indexed(conn: &Connection, tier: &str) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM photos WHERE clip_emb IS NOT NULL AND clip_tier = ?1",
        params![tier],
        |r| r.get(0),
    )
    .unwrap_or(0)
}

/// Get photo summaries by a list of IDs (for returning semantic search results).
pub fn get_photos_by_ids(conn: &Connection, ids: &[i64]) -> Result<Vec<super::models::PhotoSummary>> {
    if ids.is_empty() {
        return Ok(vec![]);
    }
    let placeholders = ids.iter().enumerate()
        .map(|(i, _)| format!("?{}", i + 1))
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT p.id, p.path, p.filename, p.status, p.provider_used,
                GROUP_CONCAT(t.tag, ',') as tags, COUNT(t.id) as tag_count
         FROM photos p
         LEFT JOIN tags t ON t.photo_id = p.id
         WHERE p.id IN ({})
         GROUP BY p.id",
        placeholders
    );
    let mut stmt = conn.prepare(&sql)?;
    let params_refs: Vec<&dyn rusqlite::ToSql> =
        ids.iter().map(|id| id as &dyn rusqlite::ToSql).collect();
    let rows = stmt
        .query_map(params_refs.as_slice(), |r| {
            let tags_str: Option<String> = r.get(5)?;
            let tags: Vec<String> = tags_str
                .unwrap_or_default()
                .split(',')
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .collect();
            Ok(super::models::PhotoSummary {
                id: r.get(0)?,
                path: r.get(1)?,
                filename: r.get(2)?,
                status: r.get(3)?,
                provider_used: r.get(4)?,
                tags,
                tag_count: r.get(6)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows)
}

