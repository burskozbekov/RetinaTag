use anyhow::{Context, Result};
use rusqlite::Connection;

/// Export all photos + tags as CSV
pub fn export_csv(conn: &Connection, output_path: &str) -> Result<usize> {
    export_csv_with_options(conn, output_path, false)
}

/// CSV export that optionally scrubs GPS coordinates. Useful when the
/// user is sharing a tag dump publicly and doesn't want every photo's
/// location history attached to it.
pub fn export_csv_with_options(conn: &Connection, output_path: &str, strip_gps: bool) -> Result<usize> {
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
        let (lat_s, lon_s) = if strip_gps {
            (String::new(), String::new())
        } else {
            (
                row.6.map(|v| v.to_string()).unwrap_or_default(),
                row.7.map(|v| v.to_string()).unwrap_or_default(),
            )
        };
        wtr.write_record([
            row.0.to_string(),
            row.1,
            row.2,
            row.3,
            row.4,
            row.5.unwrap_or_default(),
            lat_s,
            lon_s,
            row.8,
        ])?;
        count += 1;
    }

    wtr.flush()?;
    Ok(count)
}

/// Export all photos + tags as JSON
pub fn export_json(conn: &Connection, output_path: &str) -> Result<usize> {
    export_json_with_options(conn, output_path, false)
}

pub fn export_json_with_options(conn: &Connection, output_path: &str, strip_gps: bool) -> Result<usize> {
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

        let gps_val = if strip_gps || row.6.is_none() {
            serde_json::Value::Null
        } else {
            serde_json::json!({"lat": row.6, "lon": row.7})
        };
        photos.push(serde_json::json!({
            "id": row.0,
            "path": row.1,
            "filename": row.2,
            "folder": row.3,
            "status": row.4,
            "provider": row.5,
            "gps": gps_val,
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

/// Markdown export: human-readable report grouped by folder. Each photo gets
/// a heading with status + provider, tag list as inline code, and optional
/// GPS link to Google Maps. Also includes a tag-frequency summary at the top
/// so the user can see at a glance what the library is heavy in — useful for
/// pasting into a Notion/Obsidian vault or a GitHub issue.
pub fn export_markdown(conn: &Connection, output_path: &str, strip_gps: bool) -> Result<usize> {
    use std::collections::BTreeMap;

    // Tag frequency first — cheap count query.
    let mut freq: BTreeMap<String, i64> = BTreeMap::new();
    {
        let mut s = conn.prepare("SELECT tag, COUNT(*) FROM tags GROUP BY tag")?;
        let rows = s.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)))?;
        for r in rows.flatten() { freq.insert(r.0, r.1); }
    }
    // Sort tags by frequency desc, then name
    let mut freq_vec: Vec<(String, i64)> = freq.into_iter().collect();
    freq_vec.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));

    let mut stmt = conn.prepare(
        "SELECT p.id, p.path, p.filename, p.folder, p.status, p.provider_used,
                p.gps_lat, p.gps_lon, p.rating, p.favorite,
                COALESCE((SELECT GROUP_CONCAT(tag, ', ') FROM tags WHERE photo_id = p.id), '') AS all_tags
         FROM photos p ORDER BY p.folder, p.filename"
    )?;

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
            row.get::<_, Option<i64>>(8)?,
            row.get::<_, Option<i64>>(9)?,
            row.get::<_, String>(10)?,
        ))
    })?;

    let mut out = String::new();
    out.push_str("# RetinaTag Export\n\n");
    out.push_str(&format!("_Exported {}_\n\n", chrono::Utc::now().to_rfc3339()));

    // Tag frequency summary (top 50) — avoids dumping thousands of rare tags.
    if !freq_vec.is_empty() {
        out.push_str("## Top Tags\n\n");
        for (tag, n) in freq_vec.iter().take(50) {
            out.push_str(&format!("- `{}` — **{}**\n", tag, n));
        }
        out.push('\n');
    }

    out.push_str("## Photos\n\n");
    let mut cur_folder = String::new();
    let mut count = 0usize;
    for row in rows.flatten() {
        let (_id, path, filename, folder, status, provider, lat, lon, rating, favorite, tags) = row;
        if folder != cur_folder {
            out.push_str(&format!("### 📁 {}\n\n", folder));
            cur_folder = folder;
        }
        let fav = if favorite.unwrap_or(0) != 0 { " ❤️" } else { "" };
        let stars = rating.unwrap_or(0);
        let star_str = if stars > 0 && stars <= 5 {
            format!(" {}", "★".repeat(stars as usize))
        } else { String::new() };
        out.push_str(&format!("#### {}{}{}\n\n", filename, fav, star_str));
        out.push_str(&format!("- **Path:** `{}`\n", path));
        out.push_str(&format!("- **Status:** {}", status));
        if let Some(p) = provider { if !p.is_empty() { out.push_str(&format!(" · _{}_", p)); } }
        out.push('\n');
        if !strip_gps {
            if let (Some(la), Some(lo)) = (lat, lon) {
                out.push_str(&format!(
                    "- **GPS:** [{:.5}, {:.5}](https://www.google.com/maps?q={:.5},{:.5})\n",
                    la, lo, la, lo
                ));
            }
        }
        if !tags.is_empty() {
            let chips = tags
                .split(", ")
                .map(|t| format!("`{}`", t))
                .collect::<Vec<_>>()
                .join(" ");
            out.push_str(&format!("- **Tags:** {}\n", chips));
        }
        out.push('\n');
        count += 1;
    }
    std::fs::write(output_path, out)?;
    Ok(count)
}
