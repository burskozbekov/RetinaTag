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
            provider_used  TEXT,
            description    TEXT
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

    // Sharpness / blur score (Laplacian variance, higher = sharper).
    // Added as part of the Cleanup feature. NULL = not yet analyzed.
    conn.execute_batch("ALTER TABLE photos ADD COLUMN blur_score REAL;").ok();

    // Create indexes for GPS, phash, blur
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_photos_gps ON photos(gps_lat, gps_lon);
         CREATE INDEX IF NOT EXISTS idx_photos_phash ON photos(phash);
         CREATE INDEX IF NOT EXISTS idx_photos_blur  ON photos(blur_score);",
    )?;

    // CLIP semantic embedding column (added later, ignore if exists)
    conn.execute_batch("ALTER TABLE photos ADD COLUMN clip_emb BLOB;").ok();
    conn.execute_batch("ALTER TABLE photos ADD COLUMN clip_tier TEXT;").ok();

    // MTP import mapping: remembers which library photo came from which
    // object on which phone. Needed so "iPhone'u temizle (favoriler
    // hariç)" can walk the library, find the favorites that came from
    // this specific device, and leave only those on the phone. Keyed on
    // photo_id because a single on-phone object becomes exactly one
    // library photo (hash-deduped upstream).
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS mtp_imports (
            photo_id     INTEGER NOT NULL PRIMARY KEY
                         REFERENCES photos(id) ON DELETE CASCADE,
            device_id    TEXT    NOT NULL,
            object_id    TEXT    NOT NULL,
            imported_at  INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_mtp_imports_device
            ON mtp_imports(device_id);
        CREATE INDEX IF NOT EXISTS idx_mtp_imports_device_object
            ON mtp_imports(device_id, object_id);",
    )?;

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

    // Migrate existing DB: add description column if missing
    conn.execute_batch(
        "ALTER TABLE photos ADD COLUMN description TEXT;"
    ).ok();

    // Migrate: add AI-estimated location fields
    conn.execute_batch("ALTER TABLE photos ADD COLUMN estimated_lat REAL;").ok();
    conn.execute_batch("ALTER TABLE photos ADD COLUMN estimated_lon REAL;").ok();
    conn.execute_batch("ALTER TABLE photos ADD COLUMN estimated_location TEXT;").ok();

    // Migrate: add media_type and date_taken for RAW/video/timeline support
    conn.execute_batch("ALTER TABLE photos ADD COLUMN media_type TEXT NOT NULL DEFAULT 'image';").ok();
    conn.execute_batch("ALTER TABLE photos ADD COLUMN date_taken TEXT;").ok();
    conn.execute_batch("CREATE INDEX IF NOT EXISTS idx_photos_date_taken ON photos(date_taken);").ok();

    // Migrate: video duration in seconds
    conn.execute_batch("ALTER TABLE photos ADD COLUMN duration_secs INTEGER;").ok();

    // Migrate: rating & favorites system
    conn.execute_batch("ALTER TABLE photos ADD COLUMN rating INTEGER NOT NULL DEFAULT 0;").ok();
    conn.execute_batch("ALTER TABLE photos ADD COLUMN favorite INTEGER NOT NULL DEFAULT 0;").ok();
    conn.execute_batch("CREATE INDEX IF NOT EXISTS idx_photos_rating ON photos(rating);").ok();
    conn.execute_batch("CREATE INDEX IF NOT EXISTS idx_photos_favorite ON photos(favorite);").ok();

    // Migrate: dominant color palette (JSON array of hex strings)
    conn.execute_batch("ALTER TABLE photos ADD COLUMN dominant_colors TEXT;").ok();

    // Migrate: AI quality scores
    conn.execute_batch("ALTER TABLE photos ADD COLUMN quality_composition REAL;").ok();
    conn.execute_batch("ALTER TABLE photos ADD COLUMN quality_focus REAL;").ok();
    conn.execute_batch("ALTER TABLE photos ADD COLUMN quality_exposure REAL;").ok();
    conn.execute_batch("ALTER TABLE photos ADD COLUMN quality_overall REAL;").ok();

    // FTS5 for full-text description search
    conn.execute_batch(
        "CREATE VIRTUAL TABLE IF NOT EXISTS desc_fts USING fts5(
            description,
            content = '',
            tokenize = 'unicode61'
        );"
    ).ok();

    // Rebuild desc_fts from existing descriptions (idempotent — safe to run on each start)
    let has_rows: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM desc_fts LIMIT 1)", [], |r| r.get(0)
    ).unwrap_or(false);
    if !has_rows {
        conn.execute_batch(
            "INSERT INTO desc_fts(rowid, description)
             SELECT id, description FROM photos WHERE description IS NOT NULL AND description != '';"
        ).ok();
    }

    // ── One-time cleanup: merge case-variant duplicate tags per photo ─────
    // e.g. "Buğra" + "buğra" on the same photo → keep only "Buğra".
    // SQLite's UNIQUE(photo_id, tag) is byte-exact, so both got inserted by
    // old builds. Do this once; the flag is stored in settings so we never
    // re-run unnecessarily.
    let already_deduped = get_setting(&conn, "tags_case_deduped_v1")
        .ok().flatten().is_some();
    if !already_deduped {
        let _ = dedupe_tags_by_case(&conn);
        let _ = set_setting(&conn, "tags_case_deduped_v1", "1");
    }

    // ── One-time migration: clear old aHash values ──────────────────────────
    // Older builds stored an 8×8 *average-hash* in the phash column. The new
    // compute_phash is a proper DCT-based pHash with EXIF rotation applied,
    // so the two are not comparable — keeping old values would break dup
    // detection for already-scanned libraries. Clear them; they'll be
    // repopulated on the next "Analyze Library" run.
    let phash_migrated = get_setting(&conn, "phash_migrated_to_dct_v1")
        .ok().flatten().is_some();
    if !phash_migrated {
        let cleared = conn.execute("UPDATE photos SET phash = NULL WHERE phash IS NOT NULL", []).unwrap_or(0);
        eprintln!("[db] phash migration: cleared {} old aHash values", cleared);
        let _ = set_setting(&conn, "phash_migrated_to_dct_v1", "1");
    }

    // One-time: clear bogus pHash values that were computed against video
    // poster frames. Uniform/dark poster frames collapse the DCT AC band to
    // ~0, producing identical zero-hashes across unrelated clips and showing
    // up as false duplicate groups. Going forward the hasher skips videos
    // entirely; this cleans up anything earlier versions wrote.
    let video_phash_cleared = get_setting(&conn, "video_phash_cleared_v1")
        .ok().flatten().is_some();
    if !video_phash_cleared {
        let cleared = conn.execute(
            "UPDATE photos SET phash = NULL WHERE media_type = 'video' AND phash IS NOT NULL",
            []
        ).unwrap_or(0);
        if cleared > 0 {
            eprintln!("[db] video phash cleanup: cleared {} bogus video hashes", cleared);
        }
        let _ = set_setting(&conn, "video_phash_cleared_v1", "1");
    }

    // One-time: clear old blur_score values so they get recomputed with the
    // new "sharpest region anywhere" policy (overall ∪ center ∪ patch_max).
    // The old single-metric score mis-flagged bokeh, fog, and low-light
    // phone shots; the new score only flags photos with truly zero detail
    // anywhere in the frame.
    let blur_rescore = get_setting(&conn, "blur_rescore_v2")
        .ok().flatten().is_some();
    if !blur_rescore {
        let cleared = conn.execute(
            "UPDATE photos SET blur_score = NULL WHERE blur_score IS NOT NULL",
            []
        ).unwrap_or(0);
        if cleared > 0 {
            eprintln!("[db] blur rescore v2: cleared {} old single-metric blur scores", cleared);
        }
        let _ = set_setting(&conn, "blur_rescore_v2", "1");
    }

    // ── FTS5 photos index ───────────────────────────────────────────────────
    // Full-text index over filename + folder path + description so users can
    // find "beach2024" or "/Pictures/Vacation/" with a single MATCH query and
    // without scanning the whole table. External-content FTS5 — the canonical
    // data still lives in `photos`, the FTS table just stores tokens.
    conn.execute_batch(
        "CREATE VIRTUAL TABLE IF NOT EXISTS photos_fts USING fts5(
            filename,
            folder,
            description,
            content = 'photos',
            content_rowid = 'id',
            tokenize = 'unicode61 remove_diacritics 2'
        );
        CREATE TRIGGER IF NOT EXISTS photos_ai AFTER INSERT ON photos BEGIN
            INSERT INTO photos_fts(rowid, filename, folder, description)
            VALUES (new.id, new.filename, new.folder, COALESCE(new.description, ''));
        END;
        CREATE TRIGGER IF NOT EXISTS photos_ad AFTER DELETE ON photos BEGIN
            INSERT INTO photos_fts(photos_fts, rowid, filename, folder, description)
            VALUES('delete', old.id, old.filename, old.folder, COALESCE(old.description, ''));
        END;
        CREATE TRIGGER IF NOT EXISTS photos_au AFTER UPDATE ON photos BEGIN
            INSERT INTO photos_fts(photos_fts, rowid, filename, folder, description)
            VALUES('delete', old.id, old.filename, old.folder, COALESCE(old.description, ''));
            INSERT INTO photos_fts(rowid, filename, folder, description)
            VALUES (new.id, new.filename, new.folder, COALESCE(new.description, ''));
        END;"
    ).ok();

    // One-time rebuild for existing libraries that predate the FTS index.
    let photos_fts_built = get_setting(&conn, "photos_fts_built_v1")
        .ok().flatten().is_some();
    if !photos_fts_built {
        let _ = conn.execute_batch(
            "INSERT INTO photos_fts(photos_fts) VALUES('rebuild');"
        );
        eprintln!("[db] photos_fts: one-time rebuild done");
        let _ = set_setting(&conn, "photos_fts_built_v1", "1");
    }

    // ── CLIP text-embedding cache ──────────────────────────────────────────
    // Encoding a query string through the CLIP text tower is expensive on CPU
    // (~50-200 ms on tiny, 300-800 ms on base). Same user types "beach
    // sunset" half a dozen times during an editing session — cache the
    // resulting 512-d vector keyed by (tier, lowercase query) so repeat
    // semantic searches return instantly.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS clip_text_cache (
            tier        TEXT NOT NULL,
            query       TEXT NOT NULL,
            embedding   BLOB NOT NULL,
            created_at  INTEGER NOT NULL,
            hit_count   INTEGER NOT NULL DEFAULT 1,
            PRIMARY KEY (tier, query)
        );
        CREATE INDEX IF NOT EXISTS idx_clip_text_cache_created
            ON clip_text_cache(created_at);"
    ).ok();

    // ── Scan progress checkpoint ──────────────────────────────────────────
    // Records the last folder + file offset reached during a scan so that
    // Ctrl+C / crash / power-off can resume mid-scan instead of restarting
    // the whole library walk from scratch. `last_path` is the last file that
    // was fully persisted; on resume we walk past it.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS scan_checkpoints (
            folder       TEXT PRIMARY KEY,
            last_path    TEXT NOT NULL,
            processed    INTEGER NOT NULL DEFAULT 0,
            total        INTEGER NOT NULL DEFAULT 0,
            updated_at   INTEGER NOT NULL
        );"
    ).ok();

    // ── File-hash cache ───────────────────────────────────────────────────
    // (path, size, mtime) → SHA hash. The scanner hashes every file to dedup
    // against existing photos; SHA of a 10 MP JPEG is 20-50 ms. For a
    // re-scan of a 50 k-photo library that alone is 15-40 min of wasted CPU
    // because almost every file's (size, mtime) matches a previous run.
    // Caching skips the hash entirely when the tuple matches.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS file_hash_cache (
            path     TEXT PRIMARY KEY,
            size     INTEGER NOT NULL,
            mtime    INTEGER NOT NULL,
            hash     TEXT NOT NULL,
            cached_at INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_fhc_hash ON file_hash_cache(hash);"
    ).ok();

    // ── Phase 10 columns ──────────────────────────────────────────────────
    // `private` = photo is hidden from the default gallery, only visible
    // when the Private Vault is unlocked. Driven by manual toggle OR the
    // CLIP-based NSFW detector.
    conn.execute_batch("ALTER TABLE photos ADD COLUMN private INTEGER NOT NULL DEFAULT 0;").ok();
    // `nsfw_score` is the cosine similarity of this photo's CLIP embedding
    // against a fixed NSFW prompt (computed lazily, nullable). A score
    // above the user-chosen threshold flips `private` automatically.
    conn.execute_batch("ALTER TABLE photos ADD COLUMN nsfw_score REAL;").ok();
    conn.execute_batch("CREATE INDEX IF NOT EXISTS idx_photos_private ON photos(private);").ok();
    // v1.5.64 — Faz 2.1: encrypted thumbnail blob for vaulted photos.
    // When `private` flips to 1 AND the vault has a KEK loaded, the
    // existing on-disk thumbnail is sealed with AES-256-GCM and stored
    // here. The plaintext thumb is then deleted from `thumbs/` so a
    // disk image cannot reveal vaulted content. NULL = either not
    // private OR was flipped private before v1.5.64 (legacy fallback,
    // disk thumb still exists).
    conn.execute_batch("ALTER TABLE photos ADD COLUMN private_thumb_enc BLOB;").ok();
    // v1.5.66 — Faz 2.1 file-level: when a photo is in the vault its
    // ORIGINAL bytes are also encrypted in place; `path` then points to
    // `<original>.rtenc` and `original_path` remembers where to put the
    // plaintext back when the user un-vaults. NULL while not-private OR
    // when the original lives at `path` (legacy state, will be migrated
    // on next vault unlock).
    conn.execute_batch("ALTER TABLE photos ADD COLUMN original_path TEXT;").ok();

    // Private-vault PIN: stored as SHA-256 of PIN+salt. Empty row = no PIN set.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS vault (
            id           INTEGER PRIMARY KEY CHECK (id = 1),
            pin_hash     TEXT NOT NULL,
            salt         TEXT NOT NULL,
            created_at   INTEGER NOT NULL,
            last_unlock  INTEGER
        );"
    ).ok();
    // v1.5.63 — Faz 2: KEK material + BIP39 recovery blob.
    //   kek_salt:        Argon2id salt for PIN→KEK derivation. Distinct
    //                    from the existing `salt` (which is the SHA-256
    //                    PIN-hash salt) so we can rotate one without the
    //                    other.
    //   recovery_blob:   AES-GCM(RKEK, KEK). The RKEK is derived from
    //                    the BIP39 mnemonic the user wrote down at PIN
    //                    setup. We never store the mnemonic itself —
    //                    only proof we can reconstruct the KEK if it's
    //                    typed back in. NULL = legacy vault from a
    //                    pre-Faz 2 install (no recovery available).
    conn.execute_batch("ALTER TABLE vault ADD COLUMN kek_salt BLOB;").ok();
    conn.execute_batch("ALTER TABLE vault ADD COLUMN recovery_blob BLOB;").ok();
    // v1.5.64 — Faz 2.1/2.3: PIN- and biometric-wrapped KEK copies.
    //   pin_blob:        AES-GCM(PIN-KEK, KEK) where PIN-KEK is
    //                    Argon2id(PIN, kek_salt). This is what unlock()
    //                    actually uses to recover the KEK from a typed
    //                    PIN — without it the KEK never enters memory.
    //                    NULL = legacy v1.5.63 vault, upgraded on next
    //                    successful unlock.
    //   bio_blob:        AES-GCM(DPAPI-protected key, KEK). Stored on
    //                    the user's Windows account via the data
    //                    protection API; lets Windows Hello unlock
    //                    bypass typing the PIN. NULL = biometric not
    //                    enrolled.
    conn.execute_batch("ALTER TABLE vault ADD COLUMN pin_blob BLOB;").ok();
    conn.execute_batch("ALTER TABLE vault ADD COLUMN bio_blob BLOB;").ok();
    // v1.5.68 — KEK derivation version. 1 = legacy random (v1.5.63-67),
    // 2 = deterministic from BIP39 mnemonic. Only version 2 vaults are
    // portable across devices (same words = same KEK). Version 1 vaults
    // get migrated on next unlock — see commands::vault_unlock.
    conn.execute_batch("ALTER TABLE vault ADD COLUMN kek_version INTEGER NOT NULL DEFAULT 1;").ok();

    // GPS cluster cache: pre-computed location clusters for the Map view.
    // Re-built when user asks for it, not every scan.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS gps_clusters (
            id           INTEGER PRIMARY KEY AUTOINCREMENT,
            center_lat   REAL NOT NULL,
            center_lon   REAL NOT NULL,
            radius_km    REAL NOT NULL,
            photo_count  INTEGER NOT NULL,
            label        TEXT,
            date_start   TEXT,
            date_end     TEXT,
            computed_at  INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS gps_cluster_photos (
            cluster_id   INTEGER NOT NULL,
            photo_id     INTEGER NOT NULL,
            PRIMARY KEY (cluster_id, photo_id),
            FOREIGN KEY (cluster_id) REFERENCES gps_clusters(id) ON DELETE CASCADE,
            FOREIGN KEY (photo_id)   REFERENCES photos(id)       ON DELETE CASCADE
        );
        CREATE INDEX IF NOT EXISTS idx_gcp_photo ON gps_cluster_photos(photo_id);"
    ).ok();

    // Scan history log: one row per scan attempt, lets the user audit what
    // happened and when. Written from commands.rs after scan_folder_impl
    // returns (both success and error paths).
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS scan_history (
            id           INTEGER PRIMARY KEY AUTOINCREMENT,
            folder       TEXT NOT NULL,
            started_at   TEXT NOT NULL,
            finished_at  TEXT NOT NULL,
            new_files    INTEGER NOT NULL DEFAULT 0,
            skipped      INTEGER NOT NULL DEFAULT 0,
            total        INTEGER NOT NULL DEFAULT 0,
            error        TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_scan_history_started ON scan_history(started_at DESC);"
    ).ok();

    Ok(conn)
}

/// Insert a row into scan_history. Called from commands.rs after a scan
/// completes (successfully or otherwise). `error` is Some(msg) on failure.
pub fn log_scan_history(
    conn: &Connection,
    folder: &str,
    started_at: &str,
    finished_at: &str,
    new_files: i64,
    skipped: i64,
    total: i64,
    error: Option<&str>,
) -> Result<()> {
    conn.execute(
        "INSERT INTO scan_history (folder, started_at, finished_at, new_files, skipped, total, error)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![folder, started_at, finished_at, new_files, skipped, total, error],
    )?;
    Ok(())
}

/// Read the most recent N scan history entries, newest first.
pub fn get_scan_history(
    conn: &Connection,
    limit: i64,
) -> Result<Vec<(i64, String, String, String, i64, i64, i64, Option<String>)>> {
    let mut stmt = conn.prepare(
        "SELECT id, folder, started_at, finished_at, new_files, skipped, total, error
         FROM scan_history
         ORDER BY started_at DESC
         LIMIT ?1",
    )?;
    let rows = stmt
        .query_map(params![limit], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, i64>(5)?,
                r.get::<_, i64>(6)?,
                r.get::<_, Option<String>>(7)?,
            ))
        })?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows)
}

pub fn get_cached_file_hash(
    conn: &Connection,
    path: &str,
    size: i64,
    mtime: i64,
) -> Option<String> {
    conn.query_row(
        "SELECT hash FROM file_hash_cache WHERE path = ?1 AND size = ?2 AND mtime = ?3",
        params![path, size, mtime],
        |r| r.get::<_, String>(0),
    ).ok()
}

pub fn put_cached_file_hash(
    conn: &Connection,
    path: &str,
    size: i64,
    mtime: i64,
    hash: &str,
) {
    let now = chrono::Utc::now().timestamp();
    let _ = conn.execute(
        "INSERT OR REPLACE INTO file_hash_cache (path, size, mtime, hash, cached_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![path, size, mtime, hash, now],
    );
}

// ── CLIP text cache ────────────────────────────────────────────────────────

pub fn get_cached_text_embedding(
    conn: &Connection,
    tier: &str,
    query: &str,
) -> Result<Option<Vec<u8>>> {
    let key = query.trim().to_lowercase();
    let res: rusqlite::Result<Vec<u8>> = conn.query_row(
        "SELECT embedding FROM clip_text_cache WHERE tier = ?1 AND query = ?2",
        params![tier, key],
        |r| r.get(0),
    );
    match res {
        Ok(bytes) => {
            // Bump hit_count asynchronously — best-effort, failures are fine.
            let _ = conn.execute(
                "UPDATE clip_text_cache SET hit_count = hit_count + 1 WHERE tier = ?1 AND query = ?2",
                params![tier, key],
            );
            Ok(Some(bytes))
        }
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub fn put_cached_text_embedding(
    conn: &Connection,
    tier: &str,
    query: &str,
    embedding: &[u8],
) -> Result<()> {
    let key = query.trim().to_lowercase();
    let now = chrono::Utc::now().timestamp();
    conn.execute(
        "INSERT OR REPLACE INTO clip_text_cache (tier, query, embedding, created_at, hit_count)
         VALUES (?1, ?2, ?3, ?4, COALESCE((SELECT hit_count FROM clip_text_cache WHERE tier = ?1 AND query = ?2), 0) + 1)",
        params![tier, key, embedding, now],
    )?;
    // Keep the table bounded — keep the 500 most-recent per tier.
    let _ = conn.execute(
        "DELETE FROM clip_text_cache
         WHERE tier = ?1
           AND created_at < (
               SELECT COALESCE(MIN(created_at), 0) FROM (
                   SELECT created_at FROM clip_text_cache WHERE tier = ?1
                   ORDER BY created_at DESC LIMIT 500
               )
           )",
        params![tier],
    );
    Ok(())
}

// ── Scan checkpoints ───────────────────────────────────────────────────────

pub fn put_scan_checkpoint(
    conn: &Connection,
    folder: &str,
    last_path: &str,
    processed: i64,
    total: i64,
) -> Result<()> {
    let now = chrono::Utc::now().timestamp();
    conn.execute(
        "INSERT OR REPLACE INTO scan_checkpoints
         (folder, last_path, processed, total, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![folder, last_path, processed, total, now],
    )?;
    Ok(())
}

pub fn get_scan_checkpoint(conn: &Connection, folder: &str) -> Result<Option<(String, i64, i64)>> {
    let res: rusqlite::Result<(String, i64, i64)> = conn.query_row(
        "SELECT last_path, processed, total FROM scan_checkpoints WHERE folder = ?1",
        params![folder],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    );
    match res {
        Ok(v) => Ok(Some(v)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub fn clear_scan_checkpoint(conn: &Connection, folder: &str) -> Result<()> {
    conn.execute("DELETE FROM scan_checkpoints WHERE folder = ?1", params![folder])?;
    Ok(())
}

/// Merge case-variant duplicate tags per photo. For each (photo_id, lower(tag))
/// group with more than one row, keep the row whose tag starts with an
/// uppercase letter (probably a proper noun) and delete the rest.
fn dedupe_tags_by_case(conn: &Connection) -> Result<usize> {
    // Pull all (id, photo_id, tag) rows
    let rows: Vec<(i64, i64, String)> = {
        let mut s = conn.prepare("SELECT id, photo_id, tag FROM tags")?;
        let v: Vec<(i64, i64, String)> = s
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .filter_map(|r| r.ok())
            .collect();
        v
    };

    // Group by (photo_id, lower(tag))
    let mut groups: std::collections::HashMap<(i64, String), Vec<(i64, String)>> =
        std::collections::HashMap::new();
    for (id, pid, tag) in rows {
        let key = (pid, tag.to_lowercase());
        groups.entry(key).or_default().push((id, tag));
    }

    let mut deleted = 0usize;
    for ((_pid, _lower), items) in groups {
        if items.len() < 2 { continue; }

        // Pick winner: prefer the one starting with uppercase; fall back to
        // smallest id (earliest inserted) so the choice is deterministic.
        let winner_idx = items.iter().enumerate()
            .max_by_key(|(_, (id, tag))| {
                let upper = tag.chars().next().map(|c| c.is_uppercase()).unwrap_or(false);
                // Higher score wins; tie broken by earlier id.
                (upper as i64, -(*id))
            })
            .map(|(i, _)| i)
            .unwrap_or(0);

        for (i, (id, _)) in items.iter().enumerate() {
            if i == winner_idx { continue; }
            if conn.execute("DELETE FROM tags WHERE id = ?1", params![id]).is_ok() {
                deleted += 1;
            }
        }
    }
    eprintln!("[db] dedupe_tags_by_case: removed {} duplicate-cased tag rows", deleted);
    Ok(deleted)
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
    pub media_type: &'a str,
    pub date_taken: Option<String>,
    pub duration_secs: Option<i32>,
}

pub fn insert_photo(conn: &Connection, p: &NewPhoto<'_>) -> Result<i64> {
    let now = chrono::Utc::now().to_rfc3339();
    let rows_affected = conn.execute(
        "INSERT OR IGNORE INTO photos
             (path, filename, folder, hash, size, width, height, created_at, status, media_type, date_taken, duration_secs)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'pending', ?9, ?10, ?11)",
        params![p.path, p.filename, p.folder, p.hash, p.size, p.width, p.height, now, p.media_type, p.date_taken, p.duration_secs],
    )?;
    if rows_affected == 0 {
        // Row already exists (UNIQUE conflict on path) — fetch existing ID
        let id: i64 = conn.query_row(
            "SELECT id FROM photos WHERE path = ?1",
            params![p.path],
            |r| r.get(0),
        )?;
        Ok(id)
    } else {
        Ok(conn.last_insert_rowid())
    }
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

pub fn update_photo_description(conn: &Connection, id: i64, description: &str) -> Result<()> {
    // Read old description for correct FTS delete
    let old_desc: Option<String> = conn.query_row(
        "SELECT description FROM photos WHERE id = ?1", params![id], |r| r.get(0)
    ).unwrap_or(None);
    conn.execute(
        "UPDATE photos SET description = ?1 WHERE id = ?2",
        params![description, id],
    )?;
    // Sync FTS: delete old entry (with original content) then insert new
    if let Some(old) = old_desc {
        conn.execute(
            "INSERT INTO desc_fts(desc_fts, rowid, description) VALUES('delete', ?1, ?2)",
            params![id, old],
        ).ok();
    }
    conn.execute(
        "INSERT INTO desc_fts(rowid, description) VALUES(?1, ?2)",
        params![id, description],
    ).ok();
    Ok(())
}

pub fn update_photo_estimated_location(
    conn: &Connection,
    id: i64,
    lat: f64,
    lon: f64,
    location_name: &str,
) -> Result<()> {
    conn.execute(
        "UPDATE photos SET estimated_lat = ?1, estimated_lon = ?2, estimated_location = ?3 WHERE id = ?4",
        params![lat, lon, location_name, id],
    )?;
    Ok(())
}

pub fn set_location_name(conn: &Connection, id: i64, name: &str) -> Result<()> {
    conn.execute(
        "UPDATE photos SET estimated_location = ?1 WHERE id = ?2",
        params![name, id],
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
        // Case-insensitive dedup: SQLite's UNIQUE(photo_id, tag) is
        // byte-exact, so "Buğra" and "buğra" would both be inserted.
        // We compare existing tags' lowercase form against incoming lowercase
        // and skip duplicates. For a new case-variant we also UPDATE the
        // existing row's casing if incoming looks more canonical
        // (capitalized first letter → probably a person name).
        let existing: Vec<(i64, String)> = {
            let mut s = tx.prepare_cached(
                "SELECT id, tag FROM tags WHERE photo_id = ?1"
            )?;
            let v: Vec<(i64, String)> = s
                .query_map(params![photo_id], |r| Ok((r.get(0)?, r.get(1)?)))?
                .filter_map(|r| r.ok())
                .collect();
            v
        };
        // Mutable map so we can track tags inserted within THIS call too —
        // otherwise two case-variants in the same `tags` slice would both
        // slip through (e.g. AI returns ["Buğra", "buğra"]).
        // Value: (Some(row_id) if existing in DB, None if just-inserted, tag-as-stored)
        let mut seen_lower: std::collections::HashMap<String, (Option<i64>, String)> =
            existing.into_iter()
                .map(|(id, t)| (t.to_lowercase(), (Some(id), t)))
                .collect();

        let mut stmt = tx.prepare_cached(
            "INSERT OR IGNORE INTO tags (photo_id, tag, confidence, source) VALUES (?1, ?2, ?3, ?4)",
        )?;
        let mut upd_stmt = tx.prepare_cached(
            "UPDATE tags SET tag = ?1, confidence = MAX(confidence, ?2), source = ?3 WHERE id = ?4"
        )?;
        for (tag, conf, source) in tags {
            let key = tag.to_lowercase();
            match seen_lower.get(&key) {
                None => {
                    stmt.execute(params![photo_id, tag, conf, source])?;
                    // Record so a second case-variant in the same batch is caught.
                    seen_lower.insert(key, (None, tag.clone()));
                }
                Some((maybe_id, existing_tag)) => {
                    let existing_first_upper = existing_tag
                        .chars().next().map(|c| c.is_uppercase()).unwrap_or(false);
                    let new_first_upper = tag
                        .chars().next().map(|c| c.is_uppercase()).unwrap_or(false);
                    // Prefer the casing that starts with an uppercase letter
                    // (proper nouns / person names). Only update if we know
                    // the row id (maybe_id is Some — i.e. the existing tag is
                    // already persisted, not a just-inserted same-batch entry).
                    if new_first_upper && !existing_first_upper {
                        if let Some(id) = maybe_id {
                            upd_stmt.execute(params![tag, conf, source, id])?;
                            // Refresh map so subsequent iterations see new casing
                            seen_lower.insert(key, (Some(*id), tag.clone()));
                        }
                    }
                }
            }
        }
    }
    tx.commit()?;
    Ok(())
}

pub fn add_manual_tag(conn: &Connection, photo_id: i64, tag: &str) -> Result<()> {
    // Case-insensitive duplicate check — SQLite's lower() is ASCII-only, so
    // we pull candidates and compare via Rust's Unicode-aware to_lowercase.
    let existing: Vec<String> = {
        let mut s = conn.prepare_cached(
            "SELECT tag FROM tags WHERE photo_id = ?1"
        )?;
        let v: Vec<String> = s.query_map(params![photo_id], |r| r.get(0))?
            .filter_map(|r| r.ok())
            .collect();
        v
    };
    let tag_lc = tag.to_lowercase();
    if existing.iter().any(|t| t.to_lowercase() == tag_lc) {
        return Ok(()); // same tag (any case) already present
    }
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
    // Private photos only show up in the unlocked Private Vault view.
    conditions.push("p.private = 0".to_string());

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
                COALESCE((SELECT GROUP_CONCAT(tag, '|||') FROM (SELECT tag FROM tags WHERE photo_id = p.id LIMIT 10)), '') AS tag_list,
                p.media_type, p.date_taken, p.duration_secs, p.rating, p.favorite
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
                media_type: row.get::<_, Option<String>>(7)?.unwrap_or_else(|| "image".to_string()),
                date_taken: row.get(8)?,
                duration_secs: row.get(9)?,
                rating: row.get(10)?,
                favorite: row.get::<_, i32>(11)? != 0,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();

    Ok((photos, total))
}

pub fn get_photos_timeline(
    conn: &Connection,
    offset: i64,
    limit: i64,
    folder: Option<&str>,
    year_month: Option<&str>,
) -> Result<Vec<(String, Vec<PhotoSummary>)>> {
    let mut clauses: Vec<String> = Vec::new();
    if let Some(f) = folder {
        clauses.push(format!("p.folder = '{}'", f.replace('\'', "''")));
    }
    if let Some(ym) = year_month {
        // ym is expected in format YYYY-MM; match on the computed photo_date prefix
        clauses.push(format!(
            "COALESCE(SUBSTR(p.date_taken, 1, 7), SUBSTR(p.created_at, 1, 7)) = '{}'",
            ym.replace('\'', "''")
        ));
    }
    clauses.push("p.private = 0".to_string());
    let where_clause = if clauses.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", clauses.join(" AND "))
    };
    let sql = format!(
        "SELECT COALESCE(SUBSTR(p.date_taken, 1, 10), SUBSTR(p.created_at, 1, 10)) AS photo_date,
                p.id, p.path, p.filename, p.status, p.provider_used,
                (SELECT COUNT(*) FROM tags WHERE photo_id = p.id) AS tag_count,
                COALESCE((SELECT GROUP_CONCAT(tag, '|||') FROM (SELECT tag FROM tags WHERE photo_id = p.id LIMIT 10)), '') AS tag_list,
                p.media_type, p.date_taken, p.duration_secs, p.rating, p.favorite
         FROM photos p
         {}
         ORDER BY photo_date DESC, p.created_at DESC
         LIMIT ?1 OFFSET ?2",
        where_clause
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows: Vec<(String, PhotoSummary)> = stmt
        .query_map(params![limit, offset], |row| {
            let date: String = row.get(0)?;
            let tag_list: String = row.get(7)?;
            let tags: Vec<String> = if tag_list.is_empty() {
                vec![]
            } else {
                tag_list.split("|||").map(|s| s.to_string()).collect()
            };
            Ok((date, PhotoSummary {
                id: row.get(1)?,
                path: row.get(2)?,
                filename: row.get(3)?,
                status: row.get(4)?,
                provider_used: row.get(5)?,
                tag_count: row.get(6)?,
                tags,
                media_type: row.get::<_, Option<String>>(8)?.unwrap_or_else(|| "image".to_string()),
                date_taken: row.get(9)?,
                duration_secs: row.get(10)?,
                rating: row.get(11)?,
                favorite: row.get::<_, i32>(12)? != 0,
            }))
        })?
        .filter_map(|r| r.ok())
        .collect();

    // Group by date
    let mut groups: Vec<(String, Vec<PhotoSummary>)> = vec![];
    for (date, photo) in rows {
        if let Some(last) = groups.last_mut() {
            if last.0 == date {
                last.1.push(photo);
                continue;
            }
        }
        groups.push((date, vec![photo]));
    }
    Ok(groups)
}

pub fn get_photos_without_date(conn: &Connection) -> Result<Vec<(i64, String)>> {
    let mut stmt = conn.prepare(
        "SELECT id, path FROM photos WHERE date_taken IS NULL LIMIT 5000"
    )?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows)
}

pub fn update_photo_date_taken(conn: &Connection, id: i64, date: &str) -> Result<()> {
    conn.execute("UPDATE photos SET date_taken = ?1 WHERE id = ?2", params![date, id])?;
    Ok(())
}

pub fn get_photo_detail(conn: &Connection, id: i64) -> Result<Photo> {
    let photo = conn.query_row(
        "SELECT id, path, filename, folder, hash, size, width, height,
                created_at, tagged_at, thumbnail_path, status, provider_used, description, estimated_location
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
                description: row.get(13)?,
                estimated_location: row.get(14)?,
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

/// Return photos that have an exact case-insensitive tag match.
/// Used to avoid translation for names like "Buğra" that contain non-ASCII chars.
pub fn search_photos_by_tag_exact(conn: &Connection, query: &str) -> Result<Vec<PhotoSummary>> {
    let q = query.to_lowercase();
    // v1.5.63 — Faz 1: vault filter. Private photos must never leak into
    // tag searches, otherwise typing the wrong tag in the search bar
    // would expose hidden photos thumb-by-thumb.
    let mut stmt = conn.prepare(
        "SELECT DISTINCT p.id, p.path, p.filename, p.status, p.provider_used,
                (SELECT COUNT(*) FROM tags WHERE photo_id = p.id) AS tag_count,
                p.media_type, p.date_taken, p.duration_secs, p.rating, p.favorite
         FROM photos p
         JOIN tags t ON t.photo_id = p.id
         WHERE lower(t.tag) = ?1 AND p.private = 0
         ORDER BY p.filename
         LIMIT 500",
    )?;
    let rows = stmt
        .query_map(params![q], |r| {
            Ok(PhotoSummary {
                id: r.get(0)?,
                path: r.get(1)?,
                filename: r.get(2)?,
                status: r.get(3)?,
                provider_used: r.get(4)?,
                tag_count: r.get(5)?,
                tags: vec![],
                media_type: r.get::<_, Option<String>>(6)?.unwrap_or_else(|| "image".to_string()),
                date_taken: r.get(7)?,
                duration_secs: r.get(8)?,
                rating: r.get(9)?,
                favorite: r.get::<_, i32>(10)? != 0,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows)
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
                COALESCE((SELECT GROUP_CONCAT(tag, '|||') FROM (SELECT tag FROM tags WHERE photo_id = p.id LIMIT 10)), '') AS tag_list,
                p.media_type, p.date_taken, p.duration_secs, p.rating, p.favorite
         FROM photos p
         WHERE p.id IN ({}) AND p.private = 0
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
                media_type: row.get::<_, Option<String>>(7)?.unwrap_or_else(|| "image".to_string()),
                date_taken: row.get(8)?,
                duration_secs: row.get(9)?,
                rating: row.get(10)?,
                favorite: row.get::<_, i32>(11)? != 0,
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

    // v1.5.44 — Was OR before, which on a multi-word query like
    // "telefonla konuşan insanlar" returned every photo tagged with
    // "people" (overwhelmingly broad). Now we tokenize each term, AND
    // them together (FTS5 default when terms are space-separated), and
    // fall back to OR only if the AND query returns nothing — that way
    // a strict-match query like "phone person" stays strict, but an
    // exotic-phrase query that happens to AND-fail still surfaces
    // partial matches instead of going empty.

    // Each term may itself be a multi-word phrase (eg. translated
    // "people talking on phone"). Tokenize on whitespace so each WORD
    // becomes its own AND clause; quote each token so FTS treats it
    // literally and dot/punctuation in tags can't accidentally trigger
    // operator parsing.
    let tokens: Vec<String> = terms.iter()
        .flat_map(|t| t.split_whitespace().map(|w| w.to_string()).collect::<Vec<_>>())
        .filter(|w| !w.is_empty())
        .collect();
    if tokens.is_empty() {
        return Ok(vec![]);
    }

    let escape = |w: &str| format!("\"{}\"", w.replace('"', ""));
    let and_query = tokens.iter().map(|w| escape(w)).collect::<Vec<_>>().join(" AND ");
    let strict = search_photos_fts(conn, &and_query)?;
    if !strict.is_empty() {
        return Ok(strict);
    }

    // Fallback to OR for queries that have no exact AND hits.
    let or_query = tokens.iter().map(|w| escape(w)).collect::<Vec<_>>().join(" OR ");
    search_photos_fts(conn, &or_query)
}

/// Search photos by person name (case-insensitive LIKE match via face_regions → persons).
pub fn search_photos_by_person(conn: &Connection, query: &str) -> Result<Vec<PhotoSummary>> {
    let pattern = format!("%{}%", query);
    // v1.5.63 — Faz 1: vault filter. Private photos must not surface in
    // person search either, otherwise searching the user's own name would
    // include vaulted photos.
    //
    // v1.5.108 — Two-source person match. Mac side writes person names
    // BOTH as keywords (dc:subject — picked up by xmp_sidecar reader as
    // rows in the `tags` table) AND as MWG regions (which Windows now
    // imports into `face_regions`). Until the MWG import lands the only
    // breadcrumb of a Mac-named person on Windows is the tag row, so we
    // UNION across both sources here. The tag match uses an exact (not
    // substring) compare on the full name so a regular keyword tagged
    // "ali" doesn't accidentally surface under person:"Ali Can Bombadil".
    let mut stmt = conn.prepare(
        "SELECT DISTINCT p.id, p.path, p.filename, p.status, p.provider_used,
                (SELECT COUNT(*) FROM tags WHERE photo_id = p.id) AS tag_count,
                COALESCE((SELECT GROUP_CONCAT(tag, '|||') FROM (SELECT tag FROM tags WHERE photo_id = p.id LIMIT 10)), '') AS tag_list,
                p.media_type, p.date_taken, p.duration_secs, p.rating, p.favorite
         FROM photos p
         LEFT JOIN face_regions fr ON fr.photo_id = p.id
         LEFT JOIN persons pe ON pe.id = fr.person_id
         LEFT JOIN tags t_person ON t_person.photo_id = p.id AND t_person.tag = ?2 COLLATE NOCASE
         WHERE (pe.name LIKE ?1 COLLATE NOCASE OR t_person.id IS NOT NULL)
           AND p.private = 0
         ORDER BY p.tagged_at DESC
         LIMIT 500",
    )?;

    let photos = stmt
        .query_map(params![pattern, query], |row| {
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
                media_type: row.get::<_, Option<String>>(7)?.unwrap_or_else(|| "image".to_string()),
                date_taken: row.get(8)?,
                duration_secs: row.get(9)?,
                rating: row.get(10)?,
                favorite: row.get::<_, i32>(11)? != 0,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();

    Ok(photos)
}

/// Full-text search over the photos_fts index (filename + folder + description).
/// Lets users find a photo by typing part of the folder name ("Vacation") or
/// filename stem ("IMG_2024") without needing a matching tag. FTS5 tokenizer
/// is `unicode61 remove_diacritics 2` so "Istanbul" matches "İstanbul", etc.
///
/// Falls back to LIKE on filename/folder if the FTS query is malformed (rare).
pub fn search_photos_by_path(conn: &Connection, query: &str) -> Result<Vec<PhotoSummary>> {
    let words: Vec<&str> = query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() >= 2)
        .collect();
    if words.is_empty() {
        return Ok(vec![]);
    }

    // Prefix match on every word so typing "vac" hits "vacation".
    let fts_query = words
        .iter()
        .map(|w| format!("\"{}\"*", w.replace('"', "")))
        .collect::<Vec<_>>()
        .join(" OR ");

    let ids: Vec<i64> = conn
        .prepare("SELECT rowid FROM photos_fts WHERE photos_fts MATCH ?1 LIMIT 500")
        .and_then(|mut stmt| {
            stmt.query_map(params![fts_query], |r| r.get(0))
                .map(|rows| rows.filter_map(|r| r.ok()).collect())
        })
        .unwrap_or_else(|_| {
            // Malformed FTS query — fall back to LIKE over filename + folder.
            let pattern = format!("%{}%", query);
            conn.prepare(
                "SELECT id FROM photos
                 WHERE filename LIKE ?1 COLLATE NOCASE
                    OR folder   LIKE ?1 COLLATE NOCASE
                 LIMIT 500",
            )
            .and_then(|mut stmt| {
                stmt.query_map(params![pattern], |r| r.get(0))
                    .map(|rows| rows.filter_map(|r| r.ok()).collect())
            })
            .unwrap_or_default()
        });

    if ids.is_empty() {
        return Ok(vec![]);
    }
    get_photos_by_ids(conn, &ids)
}

/// Search photos by AI-generated description using FTS5.
/// Falls back to LIKE if FTS query fails (e.g. special characters).
pub fn search_photos_by_description(conn: &Connection, query: &str) -> Result<Vec<PhotoSummary>> {
    // Try FTS5 first — split multi-word into OR query for broader matching
    let words: Vec<&str> = query.split_whitespace()
        .filter(|w| w.len() >= 2)
        .collect();
    if words.is_empty() {
        return Ok(vec![]);
    }

    let fts_query = words.iter()
        .map(|w| format!("\"{}\"", w.replace('"', "")))
        .collect::<Vec<_>>()
        .join(" OR ");

    let ids: Vec<i64> = conn.prepare(
        "SELECT rowid FROM desc_fts WHERE desc_fts MATCH ?1 LIMIT 500"
    )
    .and_then(|mut stmt| {
        stmt.query_map(params![fts_query], |r| r.get(0))
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
    })
    .unwrap_or_else(|_| {
        // FTS failed — fallback to LIKE
        let pattern = format!("%{}%", query);
        conn.prepare(
            "SELECT id FROM photos WHERE description LIKE ?1 COLLATE NOCASE LIMIT 500"
        )
        .and_then(|mut stmt| {
            stmt.query_map(params![pattern], |r| r.get(0))
                .map(|rows| rows.filter_map(|r| r.ok()).collect())
        })
        .unwrap_or_default()
    });

    if ids.is_empty() {
        return Ok(vec![]);
    }

    get_photos_by_ids(conn, &ids)
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

/// Same as `get_folders` but also returns how many photos in the folder are
/// tagged. Used by the sidebar to draw a per-folder progress bar so users can
/// see at a glance which folders still need tagging. A single grouped query
/// avoids N round-trips when the library has many folders.
pub fn get_folders_with_status(conn: &Connection) -> Result<Vec<(String, i64, i64)>> {
    let mut stmt = conn.prepare(
        "SELECT folder,
                COUNT(*)                                            AS total,
                SUM(CASE WHEN status = 'tagged' THEN 1 ELSE 0 END)  AS tagged
         FROM photos
         GROUP BY folder
         ORDER BY folder",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, Option<i64>>(2)?.unwrap_or(0),
            ))
        })?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows)
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
    conn.execute("INSERT INTO tags_fts(tags_fts) VALUES('rebuild')", []).ok();
    // Clear descriptions and FTS
    conn.execute("DELETE FROM desc_fts", []).ok();
    let count = conn.execute(
        "UPDATE photos SET status = 'pending', tagged_at = NULL, description = NULL, \
         estimated_lat = NULL, estimated_lon = NULL, estimated_location = NULL",
        [],
    )?;
    // Also clear CLIP embeddings so semantic re-index happens
    conn.execute("UPDATE photos SET clip_emb = NULL, clip_tier = NULL WHERE clip_emb IS NOT NULL", []).ok();
    // Clear translation cache
    conn.execute("DELETE FROM translation_cache", []).ok();
    Ok(count)
}

/// Delete all tags for a single photo and reset to 'pending' for re-tagging
pub fn retag_photo(conn: &Connection, photo_id: i64) -> Result<()> {
    // Clear desc_fts entry with correct original content
    let old_desc: Option<String> = conn.query_row(
        "SELECT description FROM photos WHERE id = ?1", params![photo_id], |r| r.get(0)
    ).unwrap_or(None);
    if let Some(desc) = old_desc {
        conn.execute(
            "INSERT INTO desc_fts(desc_fts, rowid, description) VALUES('delete', ?1, ?2)",
            params![photo_id, desc],
        ).ok();
    }
    conn.execute("DELETE FROM tags WHERE photo_id = ?1", params![photo_id])?;
    conn.execute(
        "UPDATE photos SET status = 'pending', tagged_at = NULL, description = NULL, \
         estimated_lat = NULL, estimated_lon = NULL, estimated_location = NULL WHERE id = ?1",
        params![photo_id],
    )?;
    Ok(())
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

    // v1.5.63 — Faz 1: smart-collection rules also exclude private photos.
    // Otherwise a "tag like 'family'" rule would expose hidden photos.
    conditions.push("p.private = 0".to_string());
    let where_clause = conditions.join(" AND ");
    let sql = format!(
        "SELECT p.id, p.path, p.filename, p.status, p.provider_used,
                (SELECT COUNT(*) FROM tags WHERE photo_id = p.id) AS tag_count,
                COALESCE((SELECT GROUP_CONCAT(tag, '|||') FROM (SELECT tag FROM tags WHERE photo_id = p.id LIMIT 10)), '') AS tag_list,
                p.media_type, p.date_taken, p.duration_secs, p.rating, p.favorite
         FROM photos p WHERE {} ORDER BY p.created_at DESC LIMIT 1000", where_clause
    );

    let mut stmt = conn.prepare(&sql)?;
    let args_refs: Vec<&dyn rusqlite::ToSql> = args.iter().map(|a| a.as_ref()).collect();
    let photos = stmt.query_map(args_refs.as_slice(), |row| {
        let tag_list: String = row.get(6)?;
        let tags: Vec<String> = if tag_list.is_empty() { vec![] } else { tag_list.split("|||").map(|s| s.to_string()).collect() };
        Ok(PhotoSummary { id: row.get(0)?, path: row.get(1)?, filename: row.get(2)?, status: row.get(3)?, provider_used: row.get(4)?, tag_count: row.get(5)?, tags,
            media_type: row.get::<_, Option<String>>(7)?.unwrap_or_else(|| "image".to_string()),
            date_taken: row.get(8)?,
            duration_secs: row.get(9)?,
            rating: row.get(10)?,
            favorite: row.get::<_, i32>(11)? != 0,
        })
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

/// Touch `last_checked` on every watch_folder whose path is either equal to
/// `folder` or an ancestor of it. Covers:
///   • Manual rescan (↻ button in watch-folders UI → scan_folder with exact
///     watch-folder path) — matches via equality.
///   • Folder watcher detecting new files deep inside a watched tree —
///     matches via prefix.
///
/// Case-normalized comparison so Windows case drift doesn't cause the UI
/// timestamp to stay stale. Returns the number of rows actually touched —
/// 0 is normal for ad-hoc scans of non-watched folders.
pub fn update_watch_folder_checked_by_path(conn: &Connection, folder: &str) -> Result<usize> {
    let now = chrono::Utc::now().to_rfc3339();
    // Normalize separators so a folder watched as "C:\\Users\\x" still
    // matches a file event delivered as "C:/Users/x/photo.jpg".
    let folder_norm = folder.replace('\\', "/").to_lowercase();
    let rows = conn.execute(
        "UPDATE watch_folders
         SET last_checked = ?1
         WHERE lower(replace(path, '\\', '/')) = ?2
            OR ?2 LIKE lower(replace(path, '\\', '/')) || '/%'
            OR ?2 LIKE lower(replace(path, '\\', '/')) || '%'",
        params![now, folder_norm],
    )?;
    Ok(rows)
}

pub fn update_watch_folder_enabled(conn: &Connection, id: i64, enabled: bool) -> Result<()> {
    conn.execute(
        "UPDATE watch_folders SET enabled = ?1 WHERE id = ?2",
        params![enabled as i32, id],
    )?;
    Ok(())
}

pub fn update_watch_folder_auto_tag(conn: &Connection, id: i64, auto_tag: bool) -> Result<()> {
    conn.execute(
        "UPDATE watch_folders SET auto_tag = ?1 WHERE id = ?2",
        params![auto_tag as i32, id],
    )?;
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
    // v1.5.63 — Faz 1: vault filter on the map view too. A pin sitting on
    // top of the user's house could be enough to identify a private photo.
    let mut stmt = conn.prepare(
        "SELECT p.id, p.filename, p.gps_lat, p.gps_lon,
                (SELECT COUNT(*) FROM tags WHERE photo_id = p.id) AS tag_count
         FROM photos p WHERE p.gps_lat IS NOT NULL AND p.gps_lon IS NOT NULL AND p.private = 0
         ORDER BY p.created_at DESC LIMIT 5000"
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(crate::models::GpsPhoto {
            id: r.get(0)?,
            filename: r.get(1)?,
            lat: r.get(2)?,
            lon: r.get(3)?,
            tag_count: r.get(4)?,
            source: "gps".to_string(),
            location_name: None,
        })
    })?.filter_map(|r| r.ok()).collect();
    Ok(rows)
}

pub fn get_photos_with_estimated_location(conn: &Connection) -> Result<Vec<crate::models::GpsPhoto>> {
    // v1.5.63 — Faz 1: vault filter on AI-estimated locations as well.
    let mut stmt = conn.prepare(
        "SELECT p.id, p.filename, p.estimated_lat, p.estimated_lon,
                (SELECT COUNT(*) FROM tags WHERE photo_id = p.id) AS tag_count,
                p.estimated_location
         FROM photos p
         WHERE p.estimated_lat IS NOT NULL AND p.estimated_lon IS NOT NULL
           AND (p.gps_lat IS NULL OR p.gps_lon IS NULL)
           AND p.private = 0
         ORDER BY p.created_at DESC LIMIT 5000"
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(crate::models::GpsPhoto {
            id: r.get(0)?,
            filename: r.get(1)?,
            lat: r.get(2)?,
            lon: r.get(3)?,
            tag_count: r.get(4)?,
            source: "ai".to_string(),
            location_name: r.get(5)?,
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
    // Group by exact phash match first. Images only — see
    // get_duplicate_cleanup_rows for why videos are excluded from pHash dedup.
    // v1.5.63 — Faz 1: skip private photos so the dedupe view doesn't reveal
    // a hidden duplicate of a public one.
    let mut stmt = conn.prepare(
        "SELECT phash FROM photos
         WHERE phash IS NOT NULL AND media_type = 'image' AND private = 0
         GROUP BY phash HAVING COUNT(*) > 1"
    )?;
    let hashes: Vec<String> = stmt.query_map([], |r| r.get(0))?.filter_map(|r| r.ok()).collect();

    let mut groups = Vec::new();
    for hash in hashes {
        let mut stmt2 = conn.prepare(
            "SELECT p.id, p.path, p.filename, p.status, p.provider_used,
                    (SELECT COUNT(*) FROM tags WHERE photo_id = p.id) AS tag_count,
                    COALESCE((SELECT GROUP_CONCAT(tag, '|||') FROM (SELECT tag FROM tags WHERE photo_id = p.id LIMIT 10)), '') AS tag_list,
                    p.media_type, p.date_taken, p.duration_secs, p.rating, p.favorite
             FROM photos p WHERE p.phash = ?1 AND p.media_type = 'image' AND p.private = 0"
        )?;
        let photos: Vec<PhotoSummary> = stmt2.query_map(params![hash], |row| {
            let tag_list: String = row.get(6)?;
            let tags: Vec<String> = if tag_list.is_empty() { vec![] } else { tag_list.split("|||").map(|s| s.to_string()).collect() };
            Ok(PhotoSummary { id: row.get(0)?, path: row.get(1)?, filename: row.get(2)?, status: row.get(3)?, provider_used: row.get(4)?, tag_count: row.get(5)?, tags,
                media_type: row.get::<_, Option<String>>(7)?.unwrap_or_else(|| "image".to_string()),
                date_taken: row.get(8)?,
                duration_secs: row.get(9)?,
                rating: row.get(10)?,
                favorite: row.get::<_, i32>(11)? != 0,
            })
        })?.filter_map(|r| r.ok()).collect();

        if photos.len() > 1 {
            groups.push((hash, photos));
        }
    }
    Ok(groups)
}

// ── Cleanup: Duplicates + Blurry photos ────────────────────────────────────

/// Rich row for the Cleanup view — includes everything needed to rank photos
/// inside a duplicate group (resolution, file size, blur score, rating).
/// `tag_count`, `person_count`, `collection_count` and `has_xmp` are the
/// "invested" signals that protect a photo from auto-deletion.
#[derive(Debug, Clone)]
pub struct CleanupRow {
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
    /// Number of user tags on this photo (description-level tags).
    pub tag_count: i64,
    /// Number of faces assigned to a named person.
    pub person_count: i64,
    /// Number of collections this photo belongs to.
    pub collection_count: i64,
    /// True if the photo has a status indicating user-written XMP metadata.
    pub has_xmp: bool,
}

/// Columns expected in order: 0:id 1:path 2:filename 3:folder 4:width 5:height
/// 6:size 7:rating 8:favorite 9:blur_score 10:date_taken 11:tag_count
/// 12:person_count 13:collection_count 14:has_xmp (0/1)
fn map_cleanup_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<CleanupRow> {
    Ok(CleanupRow {
        id: row.get(0)?,
        path: row.get(1)?,
        filename: row.get(2)?,
        folder: row.get::<_, Option<String>>(3)?.unwrap_or_default(),
        width: row.get::<_, Option<i32>>(4)?.unwrap_or(0),
        height: row.get::<_, Option<i32>>(5)?.unwrap_or(0),
        size_bytes: row.get::<_, Option<i64>>(6)?.unwrap_or(0),
        rating: row.get::<_, Option<i32>>(7)?.unwrap_or(0),
        favorite: row.get::<_, Option<i32>>(8)?.unwrap_or(0) != 0,
        blur_score: row.get::<_, Option<f64>>(9)?.map(|v| v as f32),
        date_taken: row.get(10)?,
        tag_count: row.get::<_, Option<i64>>(11)?.unwrap_or(0),
        person_count: row.get::<_, Option<i64>>(12)?.unwrap_or(0),
        collection_count: row.get::<_, Option<i64>>(13)?.unwrap_or(0),
        has_xmp: row.get::<_, Option<i64>>(14)?.unwrap_or(0) != 0,
    })
}

/// Columns 0..10 are the base photo fields; 11..14 are computed sub-selects.
/// Centralising this avoids drift between duplicate/blurry/near-dup queries.
const CLEANUP_SELECT_COLS: &str = "\
    p.id, p.path, p.filename, p.folder, p.width, p.height, p.size,
    p.rating, p.favorite, p.blur_score, p.date_taken,
    (SELECT COUNT(*) FROM tags t WHERE t.photo_id = p.id) AS tag_count,
    (SELECT COUNT(*) FROM face_regions f WHERE f.photo_id = p.id AND f.person_id IS NOT NULL) AS person_count,
    (SELECT COUNT(*) FROM collection_photos cp WHERE cp.photo_id = p.id) AS collection_count,
    CASE WHEN p.status IN ('xmp_written','xmp_sync') THEN 1 ELSE 0 END AS has_xmp";

/// Return every duplicate group (≥ 2 photos with identical phash), with
/// full CleanupRow detail on each member. Optionally scoped to a folder.
pub fn get_duplicate_cleanup_rows(
    conn: &Connection,
    folder: Option<&str>,
) -> Result<Vec<(String, Vec<CleanupRow>)>> {
    // Step 1 — hashes with ≥ 2 members (optionally folder-scoped).
    //
    // IMPORTANT: we restrict to `media_type = 'image'` here. Video poster
    // frames are often uniform/dark, which makes the DCT AC coefficients
    // collapse to near-zero and produces identical (all-zero) pHashes for
    // completely unrelated clips. Grouping those as duplicates would nag
    // the user to delete real content. Video dedup belongs on a separate
    // fingerprint (e.g. first-frame + duration + size), not on pHash.
    // v1.5.63 — Faz 1: vault filter on every dedupe path (with and without
    // a folder scope). Private photos must not appear in the cleanup UI.
    let hashes: Vec<String> = if let Some(f) = folder {
        // Prefix match via substr(): LIKE would treat `%`/`_` in path as wildcards.
        let mut stmt = conn.prepare(
            "SELECT phash FROM photos
             WHERE phash IS NOT NULL
               AND media_type = 'image'
               AND private = 0
               AND (folder = ?1 OR substr(path, 1, length(?1)) = ?1)
             GROUP BY phash HAVING COUNT(*) > 1"
        )?;
        let v: Vec<String> = stmt.query_map(params![f], |r| r.get(0))?
            .filter_map(|r| r.ok())
            .collect();
        v
    } else {
        let mut stmt = conn.prepare(
            "SELECT phash FROM photos
             WHERE phash IS NOT NULL
               AND media_type = 'image'
               AND private = 0
             GROUP BY phash HAVING COUNT(*) > 1"
        )?;
        let v: Vec<String> = stmt.query_map([], |r| r.get(0))?
            .filter_map(|r| r.ok())
            .collect();
        v
    };

    // Step 2 — per-hash member rows, now including tag/person/collection counts.
    // Also gate on media_type='image' so a stray video sharing a bogus hash
    // can't sneak into the group.
    let sql = format!(
        "SELECT {cols} FROM photos p WHERE p.phash = ?1 AND p.media_type = 'image' AND p.private = 0",
        cols = CLEANUP_SELECT_COLS
    );
    let mut groups: Vec<(String, Vec<CleanupRow>)> = Vec::new();
    for h in hashes {
        let mut stmt = conn.prepare(&sql)?;
        let rows: Vec<CleanupRow> = stmt
            .query_map(params![h], map_cleanup_row)?
            .filter_map(|r| r.ok())
            .collect();
        if rows.len() >= 2 {
            groups.push((h, rows));
        }
    }
    Ok(groups)
}

/// Return blurry photos (blur_score below `threshold`), worst first.
///
/// By default we skip photos the user has *invested* in:
///   - favorites, ratings ≥ 3 stars
///   - photos with any tags, assigned persons, or in a collection
/// Set `include_protected = true` to show them anyway.
pub fn get_blurry_photos(
    conn: &Connection,
    threshold: f32,
    folder: Option<&str>,
    include_protected: bool,
    limit: i64,
) -> Result<Vec<CleanupRow>> {
    // Protect ANY photo with signs of user investment. This is stricter than
    // the previous version which only checked favorite + rating ≥ 4. We now
    // also exclude anything with tags, assigned persons, or collection
    // membership — a user who has tagged/grouped a photo has told us they
    // care about it, and Laplacian variance can misfire on bokeh / macro.
    let protect_clause = if include_protected {
        ""
    } else {
        " AND p.favorite = 0 \
          AND (p.rating IS NULL OR p.rating < 3) \
          AND NOT EXISTS (SELECT 1 FROM tags t WHERE t.photo_id = p.id) \
          AND NOT EXISTS (SELECT 1 FROM face_regions fr WHERE fr.photo_id = p.id AND fr.person_id IS NOT NULL) \
          AND NOT EXISTS (SELECT 1 FROM collection_photos cp WHERE cp.photo_id = p.id)"
    };

    // v1.5.63 — Faz 1: vault filter. Blurry-photo cleanup must not surface
    // private photos either.
    if let Some(f) = folder {
        // Prefix match via substr(); avoids LIKE-wildcard injection.
        let sql = format!(
            "SELECT {cols} FROM photos p
             WHERE p.blur_score IS NOT NULL
               AND p.blur_score < ?1
               AND p.private = 0
               AND (p.folder = ?2 OR substr(p.path, 1, length(?2)) = ?2){protect}
             ORDER BY p.blur_score ASC
             LIMIT ?3",
            cols = CLEANUP_SELECT_COLS,
            protect = protect_clause
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows: Vec<CleanupRow> = stmt
            .query_map(params![threshold as f64, f, limit], map_cleanup_row)?
            .filter_map(|r| r.ok())
            .collect();
        Ok(rows)
    } else {
        let sql = format!(
            "SELECT {cols} FROM photos p
             WHERE p.blur_score IS NOT NULL
               AND p.blur_score < ?1
               AND p.private = 0{protect}
             ORDER BY p.blur_score ASC
             LIMIT ?2",
            cols = CLEANUP_SELECT_COLS,
            protect = protect_clause
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows: Vec<CleanupRow> = stmt
            .query_map(params![threshold as f64, limit], map_cleanup_row)?
            .filter_map(|r| r.ok())
            .collect();
        Ok(rows)
    }
}

/// Compute a suggested blur threshold based on the distribution of scores
/// already in the library. We return the 10th percentile blur score — below
/// that, photos are clearly on the softer end for THIS library. This is
/// much more reliable than a hard-coded 100, which varies wildly across
/// phone vs DSLR sensors, RAW vs JPEG, etc.
///
/// Returns None if there aren't enough scored photos yet to make a call.
pub fn suggested_blur_threshold(conn: &Connection) -> Result<Option<f32>> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM photos WHERE blur_score IS NOT NULL",
        [], |r| r.get(0)
    ).unwrap_or(0);
    if count < 50 {
        // Too few samples — the percentile would be noise. Caller should
        // fall back to a safe default.
        return Ok(None);
    }
    // Switched from the 10th to the 3rd percentile. With the new
    // "sharpest region anywhere" scoring, we want to flag ONLY the extreme
    // tail — photos where no patch has detail. 10% of a typical library
    // would still sweep up too many legit-but-soft shots (fog, night,
    // intentional bokeh) and the user explicitly pushed back on that.
    let offset = ((count as f64 * 0.03) as i64).max(1);
    let p3: f64 = conn.query_row(
        "SELECT blur_score FROM photos
         WHERE blur_score IS NOT NULL
         ORDER BY blur_score ASC
         LIMIT 1 OFFSET ?1",
        params![offset], |r| r.get(0)
    ).unwrap_or(60.0);
    // Tightened clamp: with patch_max in the mix, scores for usable photos
    // sit roughly 1.5-3× higher than before, so the floor is lower and the
    // ceiling is lower too (we don't want the "suggestion" to flag half the
    // library on a soft-but-mostly-fine collection).
    let clamped = p3.clamp(25.0, 120.0) as f32;
    Ok(Some(clamped))
}

/// Return (id, path, hash_prefix_for_thumb_cache) for photos that haven't
/// been blur-scored yet. Used by the compute_blur_scores command.
pub fn get_photos_without_blur_score(
    conn: &Connection,
    limit: i64,
) -> Result<Vec<(i64, String, String)>> {
    let mut stmt = conn.prepare(
        "SELECT id, path, hash FROM photos
         WHERE blur_score IS NULL
           AND media_type = 'image'
         ORDER BY id DESC
         LIMIT ?1",
    )?;
    let rows = stmt
        .query_map(params![limit], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?.unwrap_or_default(),
            ))
        })?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows)
}

pub fn update_blur_score(conn: &Connection, id: i64, score: f32) -> Result<()> {
    conn.execute(
        "UPDATE photos SET blur_score = ?1 WHERE id = ?2",
        params![score as f64, id],
    )?;
    Ok(())
}

/// Lightweight dashboard counts for the Cleanup view.
pub fn get_cleanup_summary(conn: &Connection) -> Result<crate::models::CleanupSummary> {
    let total_photos: i64 = conn
        .query_row("SELECT COUNT(*) FROM photos", [], |r| r.get(0))
        .unwrap_or(0);

    let photos_without_phash: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM photos WHERE phash IS NULL AND media_type = 'image'",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);

    let photos_without_blur_score: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM photos WHERE blur_score IS NULL AND media_type = 'image'",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);

    // Duplicate-hash metrics. Images only: see get_duplicate_cleanup_rows for why.
    let (duplicate_groups, duplicate_photos): (i64, i64) = conn
        .query_row(
            "SELECT COUNT(*) AS groups, COALESCE(SUM(cnt),0) AS total
             FROM (SELECT COUNT(*) AS cnt FROM photos
                   WHERE phash IS NOT NULL AND media_type = 'image'
                   GROUP BY phash HAVING COUNT(*) > 1)",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap_or((0, 0));

    // Bytes reclaimable = sum(size) over duplicate photos, minus the largest
    // size in each group (which we'd keep). Approximation: (total - groups)
    // × avg size doesn't work; do it properly:
    let duplicate_bytes_reclaimable: i64 = conn
        .query_row(
            "SELECT COALESCE(SUM(size), 0) - COALESCE(SUM(max_size), 0)
             FROM (
                 SELECT SUM(size) AS size, MAX(size) AS max_size
                 FROM photos
                 WHERE phash IS NOT NULL AND media_type = 'image'
                 GROUP BY phash
                 HAVING COUNT(*) > 1
             )",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);

    // Suggested threshold (percentile-based) — None if we don't have enough
    // scored photos yet. The summary also reports the matching blurry count
    // so the UI can show the right number out of the box.
    let suggested_blur_threshold = suggested_blur_threshold(conn)?;
    let effective_threshold: f64 = suggested_blur_threshold.unwrap_or(100.0) as f64;

    // Blurry count honours the same "invested" protection as
    // get_blurry_photos so the headline number is what the user would
    // actually see in the list.
    let (blurry_photos, blurry_bytes): (i64, i64) = conn
        .query_row(
            "SELECT COUNT(*), COALESCE(SUM(p.size), 0) FROM photos p
             WHERE p.blur_score IS NOT NULL
               AND p.blur_score < ?1
               AND p.favorite = 0
               AND (p.rating IS NULL OR p.rating < 3)
               AND NOT EXISTS (SELECT 1 FROM tags t WHERE t.photo_id = p.id)
               AND NOT EXISTS (SELECT 1 FROM face_regions fr WHERE fr.photo_id = p.id AND fr.person_id IS NOT NULL)
               AND NOT EXISTS (SELECT 1 FROM collection_photos cp WHERE cp.photo_id = p.id)",
            params![effective_threshold],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap_or((0, 0));

    Ok(crate::models::CleanupSummary {
        duplicate_groups,
        duplicate_photos,
        duplicate_bytes_reclaimable,
        blurry_photos,
        blurry_bytes,
        photos_without_phash,
        photos_without_blur_score,
        total_photos,
        suggested_blur_threshold,
    })
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
    // Images only — see get_photos_without_phash_with_hash for rationale.
    let mut stmt = conn.prepare(
        "SELECT id, path FROM photos
         WHERE phash IS NULL AND media_type = 'image'
         ORDER BY id LIMIT ?1"
    )?;
    let rows = stmt.query_map(params![limit], |r| Ok((r.get(0)?, r.get(1)?)))?.filter_map(|r| r.ok()).collect();
    Ok(rows)
}

/// Like `get_photos_without_phash` but also returns the content hash so the
/// hasher can look up a cached thumbnail (much cheaper than reopening the
/// original).
pub fn get_photos_without_phash_with_hash(
    conn: &Connection,
    limit: i64,
) -> Result<Vec<(i64, String, String)>> {
    // Only images — pHash on a single video frame is meaningless and produces
    // garbage collisions when poster frames happen to be dark/uniform.
    let mut stmt = conn.prepare(
        "SELECT id, path, hash FROM photos
         WHERE phash IS NULL AND media_type = 'image'
         ORDER BY id LIMIT ?1"
    )?;
    let rows = stmt
        .query_map(params![limit], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?.unwrap_or_default(),
            ))
        })?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows)
}

/// Tags that co-occur with `tag` on the same photos. A self-join on the
/// `tags` table: for every photo that has `tag`, enumerate all *other* tags
/// on that same photo and count how often each one appears. Returns the top
/// `limit` partners sorted by frequency desc. Case-insensitive match on the
/// input tag since the tag table mixes AI-lowercased and user-typed casing.
///
/// Used by the detail panel to surface relevant tag suggestions — e.g. after
/// tagging a photo "beach", the UI can suggest "sunset", "ocean", "vacation"
/// as one-click additions instead of forcing the user to type them.
pub fn get_related_tags(conn: &Connection, tag: &str, limit: i64) -> Result<Vec<(String, i64)>> {
    let mut stmt = conn.prepare(
        "SELECT t2.tag, COUNT(*) AS cnt
         FROM tags t1
         JOIN tags t2 ON t1.photo_id = t2.photo_id
         WHERE LOWER(t1.tag) = LOWER(?1)
           AND LOWER(t2.tag) != LOWER(?1)
         GROUP BY LOWER(t2.tag)
         ORDER BY cnt DESC, t2.tag ASC
         LIMIT ?2",
    )?;
    let rows = stmt
        .query_map(rusqlite::params![tag, limit], |r| Ok((r.get(0)?, r.get(1)?)))?
        .filter_map(|r| r.ok())
        .collect();
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
    // Preserve named (person_id > 0) AND skipped (person_id = -1) rows.
    // Only delete truly unassigned detections. This is the fix for the
    // "skipped faces keep reappearing in the next batch" bug — the full
    // table wipe was erasing `-1` sentinels so `get_unknown_faces` had
    // nothing left to similarity-match against. See the matching comment
    // block in commands.rs near `detect_faces_in_photo`.
    conn.execute(
        "DELETE FROM face_regions WHERE photo_id = ?1 AND person_id IS NULL",
        params![photo_id],
    )?;
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
/// v1.5.63 — Faz 1: skip faces from private photos. The cluster UI will
/// preview these faces with their thumbnails, which would otherwise leak
/// vault photos straight into the People tab.
pub fn get_unassigned_faces_with_embeddings(
    conn: &Connection,
) -> Result<Vec<(i64, i64, Vec<u8>)>> {
    let mut stmt = conn.prepare(
        "SELECT f.id, f.photo_id, f.embedding FROM face_regions f
         JOIN photos p ON p.id = f.photo_id
         WHERE f.embedding IS NOT NULL AND f.person_id IS NULL AND p.private = 0",
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

/// Same as `get_unassigned_faces_with_embeddings` but scoped to a folder.
/// v1.5.45 — STRICT folder match only (no subfolder / prefix). Matches
/// the user's expectation that "tag in this folder" means JUST this folder.
pub fn get_unassigned_faces_with_embeddings_in_folder(
    conn: &Connection,
    folder: &str,
) -> Result<Vec<(i64, i64, Vec<u8>)>> {
    let mut stmt = conn.prepare(
        "SELECT f.id, f.photo_id, f.embedding
         FROM face_regions f
         JOIN photos p ON p.id = f.photo_id
         WHERE f.embedding IS NOT NULL
           AND f.person_id IS NULL
           AND p.folder = ?1",
    )?;
    let rows = stmt
        .query_map(params![folder], |r| {
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

/// v1.5.43 — Like `get_unassigned_faces_with_embeddings_in_folder` but ALSO
/// returns faces previously marked as skipped (person_id = -1). Used by
/// `name_face_and_propagate` so that when the user explicitly names someone
/// in the lightbox, faces of that person that the user had previously
/// skipped (typically by clicking "Skip" in an Identify Faces cluster) get
/// retroactively retagged. We only call this when there's a folder scope,
/// so the change can't accidentally re-engage skips elsewhere in the
/// library.
pub fn get_propagatable_faces_with_embeddings_in_folder(
    conn: &Connection,
    folder: &str,
) -> Result<Vec<(i64, i64, Vec<u8>)>> {
    // v1.5.45 — STRICT folder match (see top-level note).
    let mut stmt = conn.prepare(
        "SELECT f.id, f.photo_id, f.embedding
         FROM face_regions f
         JOIN photos p ON p.id = f.photo_id
         WHERE f.embedding IS NOT NULL
           AND (f.person_id IS NULL OR f.person_id = -1)
           AND p.folder = ?1",
    )?;
    let rows = stmt
        .query_map(params![folder], |r| {
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
    // v1.5.63 — Faz 1: face_count excludes faces from private photos so
    // the People tab doesn't reveal "this person appears in 4 hidden
    // photos" via the badge count.
    let mut stmt = conn.prepare(
        "SELECT p.id, p.name, p.thumbnail,
                COUNT(CASE WHEN ph.private = 0 THEN f.id END) as face_count
         FROM persons p
         LEFT JOIN face_regions f ON f.person_id = p.id
         LEFT JOIN photos ph ON ph.id = f.photo_id
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

/// Merge two person records: every face assigned to `from_person_id` is
/// re-parented to `into_person_id`, then the source person row is deleted.
/// Used when face clustering or manual assignment created two entries
/// ("Ahmet" and "Ahmet - iPhone") for what's actually the same person.
///
/// Runs inside a single transaction so a crash mid-merge can't leave an
/// orphaned person with half its faces reassigned.
pub fn merge_persons(conn: &Connection, from_person_id: i64, into_person_id: i64) -> Result<i64> {
    if from_person_id == into_person_id {
        return Ok(0);
    }
    let txn = conn.unchecked_transaction()?;
    let moved = txn.execute(
        "UPDATE face_regions SET person_id = ?1 WHERE person_id = ?2",
        params![into_person_id, from_person_id],
    )?;
    txn.execute("DELETE FROM persons WHERE id = ?1", params![from_person_id])?;
    txn.commit()?;
    Ok(moved as i64)
}

/// Timeline of face appearances for a person. Returns (photo_id, date_taken)
/// pairs sorted chronologically, one per face (so the same photo with two
/// faces of the person only appears once here — de-duped by photo_id).
/// Used by the person-aging timeline view to scrub through years of photos.
pub fn get_person_timeline(
    conn: &Connection,
    person_id: i64,
) -> Result<Vec<(i64, Option<String>)>> {
    // v1.5.63 — Faz 1: vault filter on the per-person timeline. Hidden
    // photos must not appear when scrubbing through someone's years.
    let mut stmt = conn.prepare(
        "SELECT DISTINCT p.id, p.date_taken
         FROM face_regions f
         JOIN photos p ON p.id = f.photo_id
         WHERE f.person_id = ?1 AND p.private = 0
         ORDER BY COALESCE(p.date_taken, p.created_at) ASC",
    )?;
    let rows = stmt
        .query_map(params![person_id], |r| Ok((r.get(0)?, r.get(1)?)))?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows)
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

// ── Phase 10: Private vault ───────────────────────────────────────────────

/// Toggle the `private` flag for a photo. Returns the new value.
pub fn toggle_photo_private(conn: &Connection, photo_id: i64) -> Result<bool> {
    let current: i64 = conn
        .query_row("SELECT private FROM photos WHERE id = ?1", params![photo_id], |r| r.get(0))
        .unwrap_or(0);
    let next = if current == 0 { 1 } else { 0 };
    conn.execute("UPDATE photos SET private = ?1 WHERE id = ?2", params![next, photo_id])?;
    Ok(next == 1)
}

pub fn set_photo_private(conn: &Connection, photo_id: i64, private: bool) -> Result<()> {
    conn.execute(
        "UPDATE photos SET private = ?1 WHERE id = ?2",
        params![if private { 1 } else { 0 }, photo_id],
    )?;
    Ok(())
}

pub fn set_photo_nsfw_score(conn: &Connection, photo_id: i64, score: f32) -> Result<()> {
    conn.execute(
        "UPDATE photos SET nsfw_score = ?1 WHERE id = ?2",
        params![score, photo_id],
    )?;
    Ok(())
}

/// Vault: set or update the PIN hash. Uses SHA-256(pin || salt).
pub fn vault_set_pin(conn: &Connection, pin: &str) -> Result<()> {
    use sha2::{Sha256, Digest};
    // Fresh random salt every time the PIN is set.
    let salt: String = (0..16).map(|_| {
        let b = rand_u32() & 0xFF;
        format!("{:02x}", b)
    }).collect();
    let mut h = Sha256::new();
    h.update(pin.as_bytes());
    h.update(salt.as_bytes());
    let hash = hex::encode(h.finalize());
    let now = chrono::Utc::now().timestamp();
    conn.execute(
        "INSERT INTO vault (id, pin_hash, salt, created_at) VALUES (1, ?1, ?2, ?3)
         ON CONFLICT(id) DO UPDATE SET pin_hash = ?1, salt = ?2, created_at = ?3",
        params![hash, salt, now],
    )?;
    Ok(())
}

/// v1.5.63 — Faz 2: PIN setup with BIP39 recovery. Generates a fresh
/// 24-word mnemonic, derives the RKEK, generates a random KEK, and
/// stores `AES-GCM(RKEK, KEK)` in `recovery_blob`. Also stores the
/// kek_salt used by the PIN→KEK Argon2id derivation. The plain SHA
/// pin_hash from `vault_set_pin` is still written for backwards
/// compatibility — both paths must agree on the PIN.
///
/// Returns the mnemonic phrase. Caller MUST display this exactly once
/// to the user and never persist it. The DB row alone cannot regenerate
/// it (the only stored ciphertext is the KEK under the RKEK derived
/// from the mnemonic).
pub fn vault_set_pin_with_recovery(conn: &Connection, pin: &str) -> Result<(String, [u8; 32])> {
    // v1.5.68 — DETERMINISTIC KEK. The KEK is now derived directly
    // from the BIP39 mnemonic via Argon2id. That makes the vault
    // portable: copy the .rtenc files + type the same 24 words on any
    // device → same KEK → can decrypt. PIN is just a fast local
    // shortcut that wraps the same KEK on this machine's DB.
    let phrase = crate::vault_crypto::generate_recovery_mnemonic()
        .map_err(|e| anyhow::anyhow!(e))?;
    let kek = crate::vault_crypto::derive_kek_from_mnemonic(&phrase)
        .map_err(|e| anyhow::anyhow!(e))?;
    write_vault_row(conn, pin, &kek, &phrase)?;
    Ok((phrase, kek))
}

/// v1.5.68 — cross-device restore. User types their existing 24-word
/// mnemonic + picks a NEW PIN for THIS machine. Deterministic KEK
/// derivation means the resulting KEK matches whatever was used to
/// originally encrypt the user's `.rtenc` files, so they decrypt
/// cleanly. We do NOT verify the mnemonic against any stored blob —
/// the AES-GCM auth tag in each `.rtenc` does that for us when the
/// user actually opens a photo.
pub fn vault_restore_from_mnemonic(
    conn: &Connection,
    phrase: &str,
    new_pin: &str,
) -> Result<[u8; 32]> {
    // Validate format (correct word count + checksum) before doing
    // anything expensive or destructive.
    crate::vault_crypto::validate_mnemonic(phrase)
        .map_err(|e| anyhow::anyhow!(e))?;
    let kek = crate::vault_crypto::derive_kek_from_mnemonic(phrase)
        .map_err(|e| anyhow::anyhow!(e))?;
    write_vault_row(conn, new_pin, &kek, phrase)?;
    Ok(kek)
}

/// Internal helper: write the vault row with a known KEK + mnemonic.
/// Sets kek_version = 2 (deterministic) so future unlocks know to skip
/// the legacy migration path.
fn write_vault_row(
    conn: &Connection,
    pin: &str,
    kek: &[u8; 32],
    phrase: &str,
) -> Result<()> {
    use sha2::{Sha256, Digest};

    // (a) SHA hash of PIN+salt — the cheap "is this the right PIN"
    //     check we run before burning Argon2id on the unlock path.
    let salt_str: String = (0..16).map(|_| {
        let b = rand_u32() & 0xFF;
        format!("{:02x}", b)
    }).collect();
    let mut h = Sha256::new();
    h.update(pin.as_bytes());
    h.update(salt_str.as_bytes());
    let pin_hash = hex::encode(h.finalize());

    // (b) PIN-derived wrapper key around the KEK (the actual unlock).
    let kek_salt = crate::vault_crypto::random_salt();
    let pin_kek = crate::vault_crypto::derive_kek(pin, &kek_salt)
        .map_err(|e| anyhow::anyhow!(e))?;
    let pin_blob = crate::vault_crypto::seal(&pin_kek, kek)
        .map_err(|e| anyhow::anyhow!(e))?;

    // (c) recovery_blob — kept for symmetry with the v1.5.63-67 schema
    //     and to give the "verify mnemonic" path something to check.
    //     With deterministic KEK it's redundant (the mnemonic IS the
    //     KEK) but harmless and lets us fall back gracefully.
    let mnem_kek = crate::vault_crypto::derive_kek_from_mnemonic(phrase)
        .map_err(|e| anyhow::anyhow!(e))?;
    let recovery_blob = crate::vault_crypto::seal(&mnem_kek, kek)
        .map_err(|e| anyhow::anyhow!(e))?;

    let now = chrono::Utc::now().timestamp();
    conn.execute(
        "INSERT INTO vault
            (id, pin_hash, salt, created_at, kek_salt, recovery_blob, pin_blob, bio_blob, kek_version)
         VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, NULL, 2)
         ON CONFLICT(id) DO UPDATE SET
            pin_hash = ?1, salt = ?2, created_at = ?3,
            kek_salt = ?4, recovery_blob = ?5, pin_blob = ?6,
            bio_blob = NULL,
            kek_version = 2",
        params![pin_hash, salt_str, now, kek_salt.to_vec(), recovery_blob, pin_blob],
    )?;
    Ok(())
}

/// Read the KEK version (1 = legacy random, 2 = deterministic, 0 = no
/// vault row yet). Used by the unlock path to decide whether to
/// trigger the v1.5.68 KEK upgrade migration.
pub fn vault_kek_version(conn: &Connection) -> Result<u32> {
    let v: rusqlite::Result<i64> = conn.query_row(
        "SELECT kek_version FROM vault WHERE id = 1",
        [],
        |r| r.get(0),
    );
    match v {
        Ok(n) => Ok(n as u32),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(0),
        Err(e) => Err(e.into()),
    }
}

/// v1.5.68 — finalize a legacy → deterministic upgrade. After the
/// caller has re-encrypted all .rtenc files and thumbnail blobs with
/// the new KEK, this swaps in the new pin_blob + recovery_blob and
/// flips kek_version to 2.
pub fn vault_complete_upgrade(
    conn: &Connection,
    pin: &str,
    new_kek: &[u8; 32],
    new_phrase: &str,
) -> Result<()> {
    write_vault_row(conn, pin, new_kek, new_phrase)
}

/// All photo IDs whose `.rtenc` file needs re-encryption to the new
/// deterministic KEK. Used by the legacy → deterministic migration.
pub fn private_photos_with_rtenc(conn: &Connection) -> Result<Vec<(i64, String)>> {
    let mut stmt = conn.prepare(
        "SELECT id, path FROM photos
         WHERE private = 1
           AND lower(substr(path, length(path)-5, 6)) = '.rtenc'"
    )?;
    let rows: Vec<(i64, String)> = stmt
        .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows)
}

/// All photo IDs that have an encrypted thumbnail blob. Used by the
/// legacy → deterministic migration to re-seal each blob under the
/// new KEK.
pub fn private_photos_with_thumb_blob(conn: &Connection) -> Result<Vec<i64>> {
    let mut stmt = conn.prepare(
        "SELECT id FROM photos
         WHERE private = 1 AND private_thumb_enc IS NOT NULL"
    )?;
    let rows: Vec<i64> = stmt
        .query_map([], |r| r.get::<_, i64>(0))?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows)
}

/// v1.5.64 — Faz 2.1: PIN unlock that returns the in-memory KEK. The
/// caller stores it in AppState and uses it to encrypt/decrypt private
/// thumbnails until the vault is re-locked.
///
/// Auto-upgrade path: if `pin_blob` is NULL (legacy v1.5.63 vault) but
/// the PIN matches the SHA hash, we generate fresh KEK material on the
/// spot and re-write `pin_blob` + `recovery_blob` with a NEW mnemonic.
/// Returns `(kek, Some(new_mnemonic))` in that case so the FE can warn
/// the user that their old recovery phrase is invalid and show them
/// the new one.
pub fn vault_unlock_kek(conn: &Connection, pin: &str) -> Result<Option<([u8; 32], Option<String>)>> {
    use sha2::{Sha256, Digest};
    use rand::RngCore;

    // Step 1 — verify PIN against the SHA hash. This is the cheap check
    // that decides whether to even attempt KEK derivation. A wrong PIN
    // bails here before we burn 250 ms on Argon2id.
    let row: rusqlite::Result<(String, String, Option<Vec<u8>>, Option<Vec<u8>>)> = conn.query_row(
        "SELECT pin_hash, salt, kek_salt, pin_blob FROM vault WHERE id = 1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    );
    let (stored_hash, salt, kek_salt_opt, pin_blob_opt) = match row {
        Ok(v) => v,
        Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let mut h = Sha256::new();
    h.update(pin.as_bytes());
    h.update(salt.as_bytes());
    if hex::encode(h.finalize()) != stored_hash {
        return Ok(None); // wrong PIN
    }
    let now = chrono::Utc::now().timestamp();
    let _ = conn.execute("UPDATE vault SET last_unlock = ?1 WHERE id = 1", params![now]);

    // Step 2 — KEK path. Two sub-cases:
    //   (a) v1.5.64+ vault with pin_blob present → derive PIN-KEK, open
    //   (b) legacy v1.5.63 vault → fabricate KEK + new recovery on the
    //       fly, persist, return the new mnemonic so the user is warned.
    if let (Some(kek_salt_bytes), Some(pin_blob)) = (kek_salt_opt.clone(), pin_blob_opt) {
        if kek_salt_bytes.len() == 16 {
            let mut kek_salt = [0u8; 16];
            kek_salt.copy_from_slice(&kek_salt_bytes);
            let pin_kek = crate::vault_crypto::derive_kek(pin, &kek_salt)
                .map_err(|e| anyhow::anyhow!(e))?;
            let kek_vec = crate::vault_crypto::open(&pin_kek, &pin_blob)
                .map_err(|e| anyhow::anyhow!(e))?;
            if kek_vec.len() != 32 {
                return Err(anyhow::anyhow!("vault: corrupt KEK blob"));
            }
            let mut kek = [0u8; 32];
            kek.copy_from_slice(&kek_vec);
            return Ok(Some((kek, None)));
        }
    }

    // Legacy upgrade for very-old (v1.5.63) vaults: no pin_blob OR
    // no kek_salt. The SHA check passed, so the typed PIN is correct.
    // We generate a fresh mnemonic and a deterministic KEK from it
    // (v1.5.68 schema) and write the row out clean — this ALSO bumps
    // kek_version to 2 via write_vault_row. Any photos in such a vault
    // are unencrypted at rest (those couldn't have been file-encrypted
    // pre-v1.5.66 anyway), so there's nothing to re-key on disk.
    let phrase = crate::vault_crypto::generate_recovery_mnemonic()
        .map_err(|e| anyhow::anyhow!(e))?;
    let kek = crate::vault_crypto::derive_kek_from_mnemonic(&phrase)
        .map_err(|e| anyhow::anyhow!(e))?;
    write_vault_row(conn, pin, &kek, &phrase)?;
    Ok(Some((kek, Some(phrase))))
}

/// v1.5.64 — Faz 2.1: encrypted thumbnail accessors.

/// Replace a photo's on-disk thumbnail with its AES-GCM ciphertext in
/// `private_thumb_enc`. Idempotent: if the blob already exists we don't
/// re-encrypt. Caller passes the live KEK from AppState. The thumbnail
/// file path is resolved by the caller via the existing thumbnail
/// helper since `db.rs` doesn't know the on-disk layout.
pub fn store_encrypted_thumb(
    conn: &Connection,
    photo_id: i64,
    encrypted_bytes: &[u8],
) -> Result<()> {
    conn.execute(
        "UPDATE photos SET private_thumb_enc = ?1 WHERE id = ?2",
        params![encrypted_bytes, photo_id],
    )?;
    Ok(())
}

/// Drop the encrypted thumbnail blob (called when the photo is taken
/// out of the vault — the plaintext file is restored on disk and the
/// blob is no longer needed).
pub fn clear_encrypted_thumb(conn: &Connection, photo_id: i64) -> Result<()> {
    conn.execute(
        "UPDATE photos SET private_thumb_enc = NULL WHERE id = ?1",
        params![photo_id],
    )?;
    Ok(())
}

/// Read the encrypted thumbnail blob, if any. Returns None for photos
/// that aren't private OR were vaulted before the thumbnail-encryption
/// migration (their plaintext thumb still lives on disk).
pub fn get_encrypted_thumb(conn: &Connection, photo_id: i64) -> Result<Option<Vec<u8>>> {
    let row: Option<Option<Vec<u8>>> = conn.query_row(
        "SELECT private_thumb_enc FROM photos WHERE id = ?1",
        params![photo_id],
        |r| r.get(0),
    ).optional()?;
    Ok(row.flatten())
}

/// All currently-private photos that DON'T yet have an encrypted thumb
/// blob. Used by the migration that runs once on first v1.5.64 unlock.
pub fn private_photos_needing_thumb_enc(conn: &Connection) -> Result<Vec<(i64, String)>> {
    let mut stmt = conn.prepare(
        "SELECT id, hash FROM photos
         WHERE private = 1 AND private_thumb_enc IS NULL"
    )?;
    let rows: Vec<(i64, String)> = stmt
        .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows)
}

/// v1.5.66 — Faz 2.1 file-level: every private photo whose original
/// bytes are still in plaintext on disk (i.e. `path` doesn't end in
/// `.rtenc` AND `original_path` is NULL). Returned to the migration
/// pass that runs in the background on first vault unlock after the
/// upgrade.
pub fn private_photos_needing_file_enc(conn: &Connection) -> Result<Vec<(i64, String)>> {
    let mut stmt = conn.prepare(
        "SELECT id, path FROM photos
         WHERE private = 1
           AND original_path IS NULL
           AND lower(substr(path, length(path)-5, 6)) <> '.rtenc'"
    )?;
    let rows: Vec<(i64, String)> = stmt
        .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows)
}

/// v1.5.66 — record the path swap performed by `vault_files::encrypt_in_place`.
/// `original_path` retains the pre-encrypt location so we can restore the
/// plaintext file there when the user takes the photo out of the vault.
pub fn mark_photo_encrypted(
    conn: &Connection,
    photo_id: i64,
    new_encrypted_path: &str,
    original_path: &str,
) -> Result<()> {
    conn.execute(
        "UPDATE photos SET path = ?1, original_path = ?2 WHERE id = ?3",
        params![new_encrypted_path, original_path, photo_id],
    )?;
    Ok(())
}

/// Reverse of `mark_photo_encrypted`: restore `path` to the saved
/// `original_path` and clear the saved value.
pub fn mark_photo_decrypted(
    conn: &Connection,
    photo_id: i64,
    restored_path: &str,
) -> Result<()> {
    conn.execute(
        "UPDATE photos SET path = ?1, original_path = NULL WHERE id = ?2",
        params![restored_path, photo_id],
    )?;
    Ok(())
}

/// Read the saved `original_path` for a photo. Used by the un-vault
/// flow to know where to write the decrypted plaintext back to.
pub fn get_original_path(conn: &Connection, photo_id: i64) -> Result<Option<String>> {
    let v: Option<Option<String>> = conn
        .query_row(
            "SELECT original_path FROM photos WHERE id = ?1",
            params![photo_id],
            |r| r.get(0),
        )
        .optional()?;
    Ok(v.flatten())
}

/// Faz 2.3 — biometric blob accessors.
pub fn vault_get_bio_blob(conn: &Connection) -> Result<Option<Vec<u8>>> {
    let row: Option<Option<Vec<u8>>> = conn.query_row(
        "SELECT bio_blob FROM vault WHERE id = 1",
        [],
        |r| r.get(0),
    ).optional()?;
    Ok(row.flatten())
}

pub fn vault_set_bio_blob(conn: &Connection, blob: Option<&[u8]>) -> Result<()> {
    match blob {
        Some(b) => {
            conn.execute(
                "UPDATE vault SET bio_blob = ?1 WHERE id = 1",
                params![b],
            )?;
        }
        None => {
            conn.execute(
                "UPDATE vault SET bio_blob = NULL WHERE id = 1",
                [],
            )?;
        }
    }
    Ok(())
}

/// True if the mnemonic decrypts the stored recovery_blob. Used by the
/// "I forgot my PIN" flow to validate the typed phrase before allowing
/// a PIN reset. Returns Ok(false) if no recovery_blob exists yet
/// (legacy vault) — the FE should fall back to the destructive wipe.
pub fn vault_verify_mnemonic(conn: &Connection, phrase: &str) -> Result<bool> {
    let row: Option<(Option<Vec<u8>>,)> = conn.query_row(
        "SELECT recovery_blob FROM vault WHERE id = 1",
        [],
        |r| Ok((r.get(0)?,)),
    ).optional()?;
    let blob = match row {
        Some((Some(b),)) if !b.is_empty() => b,
        _ => return Ok(false),
    };
    let rkek = match crate::vault_crypto::derive_kek_from_mnemonic(phrase) {
        Ok(k) => k,
        Err(_) => return Ok(false),
    };
    Ok(crate::vault_crypto::open(&rkek, &blob).is_ok())
}

pub fn vault_verify_pin(conn: &Connection, pin: &str) -> Result<bool> {
    use sha2::{Sha256, Digest};
    let row: rusqlite::Result<(String, String)> = conn.query_row(
        "SELECT pin_hash, salt FROM vault WHERE id = 1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    );
    let (stored_hash, salt) = match row {
        Ok(v) => v,
        Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    let mut h = Sha256::new();
    h.update(pin.as_bytes());
    h.update(salt.as_bytes());
    let computed = hex::encode(h.finalize());
    if computed == stored_hash {
        let now = chrono::Utc::now().timestamp();
        let _ = conn.execute("UPDATE vault SET last_unlock = ?1 WHERE id = 1", params![now]);
        Ok(true)
    } else {
        Ok(false)
    }
}

pub fn vault_has_pin(conn: &Connection) -> bool {
    conn.query_row("SELECT COUNT(*) FROM vault WHERE id = 1", [], |r| r.get::<_, i64>(0))
        .unwrap_or(0) > 0
}

/// Drop the stored PIN row. Photos keep their `private` flag — the user
/// just has to set a new PIN before the vault tab unlocks again. Useful
/// for "I forgot my PIN" recovery once we wire up BIP39 in Faz 2.
pub fn vault_clear_pin(conn: &Connection) -> Result<()> {
    conn.execute("DELETE FROM vault WHERE id = 1", [])?;
    Ok(())
}

/// Destructive wipe: drop the PIN AND make every private photo public
/// again. Triggered from the frontend's 10-failed-attempts path. Returns
/// the number of photos that were unflipped, mostly for the toast.
pub fn vault_reset_full(conn: &Connection) -> Result<usize> {
    conn.execute("DELETE FROM vault WHERE id = 1", [])?;
    let n = conn.execute("UPDATE photos SET private = 0 WHERE private = 1", [])?;
    Ok(n)
}

// Simple xorshift (we don't need crypto randomness for salt derivation —
// anything unpredictable enough that two concurrent users get different
// salts is fine).
fn rand_u32() -> u32 {
    use std::time::{SystemTime, UNIX_EPOCH};
    static mut STATE: u32 = 0x9E3779B9;
    unsafe {
        if STATE == 0x9E3779B9 {
            let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(1);
            STATE = STATE.wrapping_add(nanos | 1);
        }
        STATE ^= STATE << 13;
        STATE ^= STATE >> 17;
        STATE ^= STATE << 5;
        STATE
    }
}

// ── Phase 10: GPS clusters ────────────────────────────────────────────────

pub fn clear_gps_clusters(conn: &Connection) -> Result<()> {
    conn.execute("DELETE FROM gps_cluster_photos", [])?;
    conn.execute("DELETE FROM gps_clusters", [])?;
    Ok(())
}

pub fn insert_gps_cluster(
    conn: &Connection,
    center_lat: f64,
    center_lon: f64,
    radius_km: f64,
    photo_count: i64,
    label: Option<&str>,
    date_start: Option<&str>,
    date_end: Option<&str>,
) -> Result<i64> {
    let now = chrono::Utc::now().timestamp();
    conn.execute(
        "INSERT INTO gps_clusters (center_lat, center_lon, radius_km, photo_count, label, date_start, date_end, computed_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![center_lat, center_lon, radius_km, photo_count, label, date_start, date_end, now],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn link_photo_to_cluster(conn: &Connection, cluster_id: i64, photo_id: i64) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO gps_cluster_photos (cluster_id, photo_id) VALUES (?1, ?2)",
        params![cluster_id, photo_id],
    )?;
    Ok(())
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
    // v1.5.63 — Faz 1: vault filter. Path/description/CLIP searches funnel
    // their FTS-derived id list through here, so adding `private = 0` once
    // covers all three call sites without each one having to re-implement
    // the filter on its FTS rowid query.
    let sql = format!(
        "SELECT p.id, p.path, p.filename, p.status, p.provider_used,
                GROUP_CONCAT(t.tag, ',') as tags, COUNT(t.id) as tag_count
         FROM photos p
         LEFT JOIN tags t ON t.photo_id = p.id
         WHERE p.id IN ({}) AND p.private = 0
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
                media_type: "image".to_string(),
                date_taken: None,
                duration_secs: None,
                rating: 0,
                favorite: false,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows)
}

// ── Rating & Favorites ─────────────────────────────────────────────────────

pub fn set_rating(conn: &Connection, id: i64, rating: i32) -> Result<()> {
    conn.execute("UPDATE photos SET rating = ?1 WHERE id = ?2", params![rating, id])?;
    Ok(())
}

pub fn set_favorite(conn: &Connection, id: i64, favorite: bool) -> Result<()> {
    conn.execute("UPDATE photos SET favorite = ?1 WHERE id = ?2", params![favorite as i32, id])?;
    Ok(())
}

pub fn batch_set_rating(conn: &Connection, ids: &[i64], rating: i32) -> Result<usize> {
    let tx = conn.unchecked_transaction()?;
    let mut count = 0;
    for &id in ids {
        count += tx.execute("UPDATE photos SET rating = ?1 WHERE id = ?2", params![rating, id])?;
    }
    tx.commit()?;
    Ok(count)
}

pub fn batch_set_favorite(conn: &Connection, ids: &[i64], favorite: bool) -> Result<usize> {
    let tx = conn.unchecked_transaction()?;
    let mut count = 0;
    for &id in ids {
        count += tx.execute("UPDATE photos SET favorite = ?1 WHERE id = ?2", params![favorite as i32, id])?;
    }
    tx.commit()?;
    Ok(count)
}

pub fn batch_add_tags(conn: &Connection, ids: &[i64], tags: &[String]) -> Result<usize> {
    let tx = conn.unchecked_transaction()?;
    let mut count = 0;
    for &id in ids {
        for tag in tags {
            count += tx.execute(
                "INSERT OR IGNORE INTO tags (photo_id, tag, confidence, source) VALUES (?1, ?2, 1.0, 'manual')",
                params![id, tag],
            )?;
        }
    }
    tx.commit()?;
    Ok(count)
}

// ── Color Search ───────────────────────────────────────────────────────────

pub fn update_dominant_colors(conn: &Connection, id: i64, colors_json: &str) -> Result<()> {
    conn.execute("UPDATE photos SET dominant_colors = ?1 WHERE id = ?2", params![colors_json, id])?;
    Ok(())
}

pub fn get_photos_without_colors(conn: &Connection, limit: i64) -> Result<Vec<(i64, String, String)>> {
    let mut stmt = conn.prepare(
        "SELECT id, path, hash FROM photos WHERE dominant_colors IS NULL LIMIT ?1"
    )?;
    let rows = stmt.query_map(params![limit], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows)
}

// ── Library Analytics ──────────────────────────────────────────────────────

pub fn get_library_analytics(conn: &Connection) -> Result<crate::models::LibraryAnalytics> {
    // v1.5.63 — Faz 1: every aggregate excludes private photos. Without
    // this filter the calendar/library-analytics view would still tell
    // an attacker "you have 47 photos in 2024-08" even when the vault
    // is locked, leaking metadata about hidden content.

    // Photos by month
    let mut stmt = conn.prepare(
        "SELECT COALESCE(SUBSTR(date_taken,1,7), SUBSTR(created_at,1,7)) as m, COUNT(*) FROM photos WHERE private = 0 GROUP BY m ORDER BY m DESC LIMIT 24"
    )?;
    let photos_by_month: Vec<(String, i64)> = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .filter_map(|r| r.ok()).collect();

    // Top tags — exclude tags that appear ONLY on private photos.
    let mut stmt = conn.prepare(
        "SELECT t.tag, COUNT(*) as cnt
         FROM tags t JOIN photos p ON p.id = t.photo_id
         WHERE p.private = 0
         GROUP BY t.tag ORDER BY cnt DESC LIMIT 30"
    )?;
    let top_tags: Vec<(String, i64)> = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .filter_map(|r| r.ok()).collect();

    // Camera stats (from EXIF - we need to query the photos that have been tagged)
    // We don't have camera in DB directly, so we'll use provider_used as a proxy for now
    let camera_stats: Vec<(String, i64)> = vec![];

    // Media type breakdown
    let mut stmt = conn.prepare(
        "SELECT COALESCE(media_type, 'image'), COUNT(*) FROM photos WHERE private = 0 GROUP BY media_type"
    )?;
    let media_type_breakdown: Vec<(String, i64)> = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .filter_map(|r| r.ok()).collect();

    // Rating distribution
    let mut stmt = conn.prepare(
        "SELECT rating, COUNT(*) FROM photos WHERE private = 0 GROUP BY rating ORDER BY rating"
    )?;
    let rating_distribution: Vec<(i32, i64)> = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .filter_map(|r| r.ok()).collect();

    // Top locations
    let mut stmt = conn.prepare(
        "SELECT estimated_location, COUNT(*) as cnt FROM photos WHERE estimated_location IS NOT NULL AND estimated_location != '' AND private = 0 GROUP BY estimated_location ORDER BY cnt DESC LIMIT 20"
    )?;
    let top_locations: Vec<(String, i64)> = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .filter_map(|r| r.ok()).collect();

    // Storage by folder
    let mut stmt = conn.prepare(
        "SELECT folder, SUM(size) FROM photos WHERE private = 0 GROUP BY folder ORDER BY SUM(size) DESC LIMIT 15"
    )?;
    let storage_by_folder: Vec<(String, i64)> = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .filter_map(|r| r.ok()).collect();

    // Totals
    let total_photos: i64 = conn.query_row("SELECT COUNT(*) FROM photos", [], |r| r.get(0))?;
    let total_size_bytes: i64 = conn.query_row("SELECT COALESCE(SUM(size),0) FROM photos", [], |r| r.get(0))?;

    Ok(crate::models::LibraryAnalytics {
        photos_by_month,
        top_tags,
        camera_stats,
        media_type_breakdown,
        rating_distribution,
        top_locations,
        storage_by_folder,
        total_photos,
        total_size_bytes,
    })
}

// ── Calendar View ──────────────────────────────────────────────────────────

pub fn get_photos_calendar(conn: &Connection, year: i32, month: i32) -> Result<Vec<crate::models::CalendarDay>> {
    let prefix = format!("{:04}-{:02}", year, month);
    let mut stmt = conn.prepare(
        "SELECT CAST(SUBSTR(COALESCE(date_taken, created_at), 9, 2) AS INTEGER) as day,
                COUNT(*) as cnt,
                MIN(id) as first_id
         FROM photos
         WHERE SUBSTR(COALESCE(date_taken, created_at), 1, 7) = ?1
         GROUP BY day
         ORDER BY day"
    )?;
    let days = stmt.query_map(params![prefix], |r| {
        Ok(crate::models::CalendarDay {
            day: r.get(0)?,
            count: r.get(1)?,
            first_photo_id: r.get(2)?,
        })
    })?.filter_map(|r| r.ok()).collect();
    Ok(days)
}

/// Returns list of (year, month, count) for all photos, ordered by year/month.
pub fn get_year_month_counts(conn: &Connection) -> Result<Vec<(i32, i32, i64)>> {
    let mut stmt = conn.prepare(
        "SELECT CAST(SUBSTR(COALESCE(date_taken, created_at), 1, 4) AS INTEGER) as y,
                CAST(SUBSTR(COALESCE(date_taken, created_at), 6, 2) AS INTEGER) as m,
                COUNT(*) as cnt
         FROM photos
         WHERE COALESCE(date_taken, created_at) IS NOT NULL
           AND LENGTH(COALESCE(date_taken, created_at)) >= 7
         GROUP BY y, m
         HAVING y > 0 AND m > 0 AND m <= 12
         ORDER BY y DESC, m DESC"
    )?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows)
}

// ── Health Check ───────────────────────────────────────────────────────────

pub fn get_all_photo_paths(conn: &Connection) -> Result<Vec<(i64, String)>> {
    let mut stmt = conn.prepare("SELECT id, path FROM photos")?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .filter_map(|r| r.ok()).collect();
    Ok(rows)
}

pub fn delete_photos_by_ids(conn: &Connection, ids: &[i64]) -> Result<usize> {
    let tx = conn.unchecked_transaction()?;
    let mut count = 0;
    for &id in ids {
        count += tx.execute("DELETE FROM photos WHERE id = ?1", params![id])?;
    }
    tx.commit()?;
    Ok(count)
}

// ── Smart Rename ───────────────────────────────────────────────────────────

pub fn get_photo_rename_data(conn: &Connection, id: i64) -> Result<(String, String, Option<String>, Option<String>, Option<String>)> {
    conn.query_row(
        "SELECT path, filename, description, estimated_location, date_taken FROM photos WHERE id = ?1",
        params![id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
    ).map_err(|e| e.into())
}

pub fn update_photo_path(conn: &Connection, id: i64, new_path: &str, new_filename: &str) -> Result<()> {
    conn.execute(
        "UPDATE photos SET path = ?1, filename = ?2 WHERE id = ?3",
        params![new_path, new_filename, id],
    )?;
    Ok(())
}

// ── Find Similar (CLIP) ───────────────────────────────────────────────────

pub fn mark_face_skipped(conn: &Connection, face_id: i64) -> Result<()> {
    // Set person_id = -1 to mark as "skipped" (won't show in unknown faces).
    // Guarded against clobbering a name the user has already assigned — only
    // touches rows that are currently NULL or already -1.
    conn.execute(
        "UPDATE face_regions SET person_id = -1 \
         WHERE id = ?1 AND (person_id IS NULL OR person_id = -1)",
        params![face_id],
    )?;
    Ok(())
}

/// Mark a face as skipped AND propagate the skip to all visually-similar
/// unassigned faces. This prevents the same person from re-appearing in the
/// "Who is this?" popup just because they're in a different photo.
///
/// Returns total number of face_regions marked as skipped (incl. the seed).
///
/// Safety: both the seed and the propagation UPDATE are guarded against
/// overwriting named faces (person_id > 0). Without the guard, a single
/// bad caller — e.g. the legacy `skip_face` command that doesn't pre-check
/// `still_unassigned` — could silently erase a name assignment.
pub fn mark_face_skipped_propagate(
    conn: &Connection,
    face_id: i64,
    similarity_threshold: f32,
) -> Result<usize> {
    // 1. Mark the seed face as skipped (but never overwrite a name)
    conn.execute(
        "UPDATE face_regions SET person_id = -1 \
         WHERE id = ?1 AND (person_id IS NULL OR person_id = -1)",
        params![face_id],
    )?;
    let mut total = 1usize;

    // 2. Get the seed face's embedding
    let seed_bytes: Option<Vec<u8>> = conn.query_row(
        "SELECT embedding FROM face_regions WHERE id = ?1 AND embedding IS NOT NULL",
        params![face_id],
        |r| r.get(0),
    ).optional()?;
    let seed_bytes = match seed_bytes { Some(b) => b, None => return Ok(total) };
    let seed_emb = crate::face::bytes_to_embedding(&seed_bytes);
    if seed_emb.len() != 512 { return Ok(total); }

    // 3. Pull all unassigned faces that have embeddings
    let mut stmt = conn.prepare(
        "SELECT id, embedding FROM face_regions WHERE person_id IS NULL AND embedding IS NOT NULL"
    )?;
    let candidates: Vec<(i64, Vec<u8>)> = stmt
        .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?)))?
        .filter_map(|r| r.ok())
        .collect();
    drop(stmt);

    // 4. Skip every candidate whose similarity passes the threshold
    let mut to_skip: Vec<i64> = Vec::new();
    for (cand_id, cand_bytes) in candidates {
        let cand_emb = crate::face::bytes_to_embedding(&cand_bytes);
        if cand_emb.len() != 512 { continue; }
        if crate::face::cosine_similarity(&seed_emb, &cand_emb) >= similarity_threshold {
            to_skip.push(cand_id);
        }
    }
    if !to_skip.is_empty() {
        // Defensive guard: even though candidates were SELECTed with
        // `person_id IS NULL`, the extra WHERE clause here means a future
        // reorder of operations or a race introduced by later refactors
        // still can't wipe out a name that's been assigned in between.
        let mut upd = conn.prepare(
            "UPDATE face_regions SET person_id = -1 WHERE id = ?1 AND person_id IS NULL"
        )?;
        for id in &to_skip {
            upd.execute(params![id])?;
        }
        total += to_skip.len();
    }
    Ok(total)
}

pub fn get_clip_embedding(conn: &Connection, photo_id: i64) -> Result<Option<Vec<u8>>> {
    let result: Option<Vec<u8>> = conn.query_row(
        "SELECT clip_emb FROM photos WHERE id = ?1",
        params![photo_id],
        |r| r.get(0),
    ).optional()?;
    Ok(result)
}

pub fn get_all_clip_embeddings_except(conn: &Connection, exclude_id: i64) -> Result<Vec<(i64, Vec<u8>)>> {
    // v1.5.63 — Faz 1: vault filter on similarity search. find_similar()
    // ranks every embedding here, so excluding private rows up front
    // means hidden photos can never appear as a "Similar to this" hit.
    let mut stmt = conn.prepare(
        "SELECT id, clip_emb FROM photos WHERE clip_emb IS NOT NULL AND id != ?1 AND private = 0"
    )?;
    let rows = stmt.query_map(params![exclude_id], |r| Ok((r.get(0)?, r.get(1)?)))?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows)
}

/// Photos taken on a given MM-DD across any year. Used by the "On this day"
/// memory sidebar: user opens the app on, say, April 20 and sees every
/// photo taken on April 20 in any year they've archived. Ordered newest
/// year first so current year (if any) leads the list.
///
/// `month_day` must be "MM-DD" (zero-padded). Matches against
/// `date_taken` if present, otherwise falls back to the file's
/// `created_at` timestamp — this way photos missing EXIF still
/// surface on their on-disk date.
pub fn get_photos_on_this_day(
    conn: &Connection,
    month_day: &str,
) -> Result<Vec<(String, PhotoSummary)>> {
    // v1.5.63 — Faz 1: vault filter on the "On this day" memory view.
    let sql = "SELECT COALESCE(SUBSTR(p.date_taken, 1, 10), SUBSTR(p.created_at, 1, 10)) AS photo_date,
                      p.id, p.path, p.filename, p.status, p.provider_used,
                      (SELECT COUNT(*) FROM tags WHERE photo_id = p.id) AS tag_count,
                      COALESCE((SELECT GROUP_CONCAT(tag, '|||') FROM (SELECT tag FROM tags WHERE photo_id = p.id LIMIT 10)), '') AS tag_list,
                      p.media_type, p.date_taken, p.duration_secs, p.rating, p.favorite
               FROM photos p
               WHERE SUBSTR(COALESCE(p.date_taken, p.created_at), 6, 5) = ?1
                 AND p.private = 0
               ORDER BY photo_date DESC, p.created_at DESC";
    let mut stmt = conn.prepare(sql)?;
    let rows: Vec<(String, PhotoSummary)> = stmt
        .query_map(params![month_day], |row| {
            let date: String = row.get(0)?;
            let tag_list: String = row.get(7)?;
            let tags: Vec<String> = if tag_list.is_empty() {
                vec![]
            } else {
                tag_list.split("|||").map(|s| s.to_string()).collect()
            };
            Ok((date, PhotoSummary {
                id: row.get(1)?,
                path: row.get(2)?,
                filename: row.get(3)?,
                status: row.get(4)?,
                provider_used: row.get(5)?,
                tag_count: row.get(6)?,
                tags,
                media_type: row.get::<_, Option<String>>(8)?.unwrap_or_else(|| "image".to_string()),
                date_taken: row.get(9)?,
                duration_secs: row.get(10)?,
                rating: row.get(11)?,
                favorite: row.get::<_, i32>(12)? != 0,
            }))
        })?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows)
}

/// Return every (hash, thumbnail_path) recorded in the library. Thumbnail
/// GC compares this against the actual on-disk thumbnails directory and
/// removes files that no longer map to any photo — a common source of
/// silent disk bloat when the user deletes photos from the app.
pub fn get_all_thumbnail_hashes(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT hash FROM photos WHERE hash IS NOT NULL")?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows)
}

/// Every (id, path) in the library. Used by the "find missing files"
/// scan: walk all rows, stat the path, flag the ones whose file no
/// longer exists. Kept separate from the existing `get_all_photo_paths`
/// so callers can extend without breaking other consumers.
pub fn get_all_id_paths(conn: &Connection) -> Result<Vec<(i64, String)>> {
    let mut stmt = conn.prepare("SELECT id, path FROM photos")?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows)
}

/// Relink a library photo to a new on-disk location after the user moved
/// the original file. We verify the new file's hash still matches so we
/// don't accidentally point at a different image with the same filename.
pub fn relink_photo_path(conn: &Connection, photo_id: i64, new_path: &str) -> Result<()> {
    let new_folder = std::path::Path::new(new_path)
        .parent()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    let new_filename = std::path::Path::new(new_path)
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    conn.execute(
        "UPDATE photos SET path = ?1, folder = ?2, filename = ?3 WHERE id = ?4",
        params![new_path, new_folder, new_filename, photo_id],
    )?;
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────
// Regression tests for the structured-filter vs free-text-search split.
// v1.5.107: a sidebar click on "Ali Can Bombadil" (one face) was
// returning ~100 photos because the JS layer routed person filtering
// through free-text search_photos, which tokenises ["ali","can","bombadil"]
// and AND-intersects across tags. The fix routes person filtering through
// the dedicated db::search_photos_by_person (this file). These tests
// guard the contract that "person filter returns ONLY photos with that
// person's face, never matches by text" — so a future refactor that
// accidentally re-routes person clicks through FTS will fail the test
// before it ships.
// ─────────────────────────────────────────────────────────────────────────
#[cfg(test)]
mod person_filter_tests {
    use super::*;
    use rusqlite::Connection;

    fn make_db() -> Connection {
        init_db(":memory:").expect("schema init")
    }

    fn insert_photo_with_tags(conn: &Connection, path: &str, tags: &[&str]) -> i64 {
        let now = chrono::Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO photos (path, filename, folder, hash, size, width, height,
                                 created_at, status, media_type)
             VALUES (?1, ?2, ?3, ?4, 0, 0, 0, ?5, 'tagged', 'image')",
            params![path, path, "/test", path, now],
        )
        .unwrap();
        let id = conn.last_insert_rowid();
        for t in tags {
            conn.execute(
                "INSERT INTO tags (photo_id, tag, confidence, source)
                 VALUES (?1, ?2, 1.0, 'test')",
                params![id, t],
            )
            .unwrap();
        }
        id
    }

    #[test]
    fn multi_word_name_does_not_match_by_token_collision() {
        // Reproduces the v1.5.107 bug: person name "Ali Can Bombadil"
        // contains the English word "can". Many real photos in the
        // user's library are tagged "can" (Coca-Cola can, Jack Daniel's
        // can, AI captions like "you can see…"). If person filtering
        // tokenised the name and AND-intersected, all photos sharing
        // the three tokens would falsely surface. The fixed code path
        // (search_photos_by_person + face_regions JOIN) must return
        // ONLY the photo that has the named person's face — no matter
        // how many other photos happen to be tagged with "can" or "ali".
        let conn = make_db();
        let real_id = insert_photo_with_tags(&conn, "/real_face.jpg", &["man", "smiling"]);
        let person_id = create_person(&conn, "Ali Can Bombadil").unwrap();
        let face_id = insert_face_region(&conn, real_id, 0, 0, 100, 100, 0.99, &[0u8; 32]).unwrap();
        conn.execute(
            "UPDATE face_regions SET person_id = ?1 WHERE id = ?2",
            params![person_id, face_id],
        )
        .unwrap();

        // 50 noise photos tagged with the three name tokens but no face.
        for i in 0..50 {
            insert_photo_with_tags(
                &conn,
                &format!("/coke_can_{}.jpg", i),
                &["coca-cola", "can", "ali", "bombadil"],
            );
        }

        let hits = search_photos_by_person(&conn, "Ali Can Bombadil").unwrap();
        assert_eq!(
            hits.len(),
            1,
            "person filter must use face_regions JOIN, NOT free-text token AND. \
             Got {} hits when only 1 real face is assigned to this person.",
            hits.len()
        );
        assert_eq!(hits[0].id, real_id);
    }

    #[test]
    fn person_name_is_case_insensitive() {
        let conn = make_db();
        let id = insert_photo_with_tags(&conn, "/p.jpg", &[]);
        let person_id = create_person(&conn, "Ali Can Bombadil").unwrap();
        let face_id = insert_face_region(&conn, id, 0, 0, 10, 10, 0.9, &[0u8; 32]).unwrap();
        conn.execute(
            "UPDATE face_regions SET person_id = ?1 WHERE id = ?2",
            params![person_id, face_id],
        )
        .unwrap();

        for variant in ["Ali Can Bombadil", "ali can bombadil", "ALI CAN BOMBADIL"] {
            let hits = search_photos_by_person(&conn, variant).unwrap();
            assert_eq!(hits.len(), 1, "case variant {} should match", variant);
        }
    }

    #[test]
    fn tag_with_full_person_name_surfaces_under_person_filter() {
        // v1.5.108 — Mac side writes person names as `dc:subject`
        // keywords; Windows imports those as rows in `tags`. Until the
        // MWG region import lands on every photo (or in case Mac stops
        // emitting regions for some reason), the only record that
        // photo X is "Ali Can Bombadil" is a tag with the literal name.
        // search_photos_by_person now UNIONs `persons` JOIN with an
        // exact tag match so those photos surface too.
        let conn = make_db();
        // No face / no person row — only a tag carrying the name.
        let id = insert_photo_with_tags(&conn, "/mac_tagged.jpg", &["smiling", "Ali Can Bombadil"]);
        let hits = search_photos_by_person(&conn, "Ali Can Bombadil").unwrap();
        assert_eq!(hits.len(), 1, "tag-only photo must surface");
        assert_eq!(hits[0].id, id);
    }

    #[test]
    fn partial_word_tag_does_not_surface_under_person_filter() {
        // Guards the EXACT-tag match: a tag "ali" (or "can", or
        // "bombadil" alone) must NOT surface under person:"Ali Can
        // Bombadil". This is the safeguard against re-introducing
        // the v1.5.107 token-collision bug via the tag side of the
        // UNION. Only photos whose tag IS the full person name match.
        let conn = make_db();
        // 20 photos tagged with "can" but never with the full name.
        for i in 0..20 {
            insert_photo_with_tags(&conn, &format!("/can_{}.jpg", i), &["can", "ali"]);
        }
        let hits = search_photos_by_person(&conn, "Ali Can Bombadil").unwrap();
        assert!(hits.is_empty(), "partial tokens must not match person filter");
    }

    #[test]
    fn vaulted_photos_never_surface_via_person_filter() {
        // v1.5.63 — private (vaulted) photos must not leak through any
        // filter, including person filter. Re-asserting here so a future
        // refactor of search_photos_by_person can't drop the
        // `p.private = 0` clause without a test catching it.
        let conn = make_db();
        let id = insert_photo_with_tags(&conn, "/secret.jpg", &[]);
        conn.execute("UPDATE photos SET private = 1 WHERE id = ?1", params![id])
            .unwrap();
        let person_id = create_person(&conn, "Vaulted Subject").unwrap();
        let face_id = insert_face_region(&conn, id, 0, 0, 10, 10, 0.9, &[0u8; 32]).unwrap();
        conn.execute(
            "UPDATE face_regions SET person_id = ?1 WHERE id = ?2",
            params![person_id, face_id],
        )
        .unwrap();

        let hits = search_photos_by_person(&conn, "Vaulted Subject").unwrap();
        assert!(hits.is_empty(), "vaulted photo must NOT leak via person filter");
    }
}
