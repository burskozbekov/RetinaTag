use anyhow::{Context, Result};
use rusqlite::Connection;

/// Export all photos + tags as CSV
pub fn export_csv(conn: &Connection, output_path: &str) -> Result<usize> {
    let mut stmt = conn.prepare(
        "SELECT p.id, p.path, p.filename, p.folder, p.status, p.provider_used,
                p.gps_lat, p.gps_lon,
                COALESCE((SELECT GROUP_CONCAT(tag, '; ') FROM tags WHERE photo_id = p.id), '') AS all_tags
         FROM photos p ORDER BY p.folder, p.filename"
    )?;

    let mut wtr = csv::Writer::from_path(output_path).context("Failed to create CSV file")?;

    wtr.write_record([
        "ID", "Path", "Filename", "Folder", "Status", "Provider",
        "GPS Latitude", "GPS Longitude", "Tags",
    ])?;

    let mut count = 0usize;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, Option<String>>(5)?,
            row.get::<_, Option<f64>>(6)?,
            row.get::<_, Option<f64>>(7)?,
            row.get::<_, String>(8)?,
        ))
    })?;

    for row in rows.flatten() {
        wtr.write_record([
            row.0.to_string(),
            row.1,
            row.2,
            row.3,
            row.4,
            row.5.unwrap_or_default(),
            row.6.map(|v| v.to_string()).unwrap_or_default(),
            row.7.map(|v| v.to_string()).unwrap_or_default(),
            row.8,
        ])?;
        count += 1;
    }

    wtr.flush()?;
    Ok(count)
}

/// Export all photos + tags as JSON
pub fn export_json(conn: &Connection, output_path: &str) -> Result<usize> {
    let mut stmt = conn.prepare(
        "SELECT p.id, p.path, p.filename, p.folder, p.status, p.provider_used,
                p.gps_lat, p.gps_lon, p.created_at, p.tagged_at
         FROM photos p ORDER BY p.folder, p.filename"
    )?;

    let mut photos = Vec::new();

    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, Option<String>>(5)?,
            row.get::<_, Option<f64>>(6)?,
            row.get::<_, Option<f64>>(7)?,
            row.get::<_, String>(8)?,
            row.get::<_, Option<String>>(9)?,
        ))
    })?;

    for row in rows.flatten() {
        // Fetch tags for this photo
        let mut tag_stmt = conn.prepare_cached(
            "SELECT tag, confidence, source FROM tags WHERE photo_id = ?1"
        )?;
        let tags: Vec<serde_json::Value> = tag_stmt
            .query_map(rusqlite::params![row.0], |r| {
                Ok(serde_json::json!({
                    "tag": r.get::<_, String>(0)?,
                    "confidence": r.get::<_, Option<f64>>(1)?,
                    "source": r.get::<_, Option<String>>(2)?
                }))
            })?
            .filter_map(|r| r.ok())
            .collect();

        photos.push(serde_json::json!({
            "id": row.0,
            "path": row.1,
            "filename": row.2,
            "folder": row.3,
            "status": row.4,
            "provider": row.5,
            "gps": if row.6.is_some() { serde_json::json!({"lat": row.6, "lon": row.7}) } else { serde_json::Value::Null },
            "created_at": row.8,
            "tagged_at": row.9,
            "tags": tags
        }));
    }

    let count = photos.len();
    let json = serde_json::json!({
        "exported_at": chrono::Utc::now().to_rfc3339(),
        "total_photos": count,
        "photos": photos
    });

    std::fs::write(output_path, serde_json::to_string_pretty(&json)?)?;
    Ok(count)
}
