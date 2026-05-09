use anyhow::Result;
use exif::{In, Tag};

use crate::models::PhotoExif;

/// Read EXIF metadata from an image file
pub fn read_exif(path: &str) -> Result<PhotoExif> {
    let file = std::fs::File::open(path)?;
    let mut buf_reader = std::io::BufReader::new(&file);
    let exif_reader = exif::Reader::new();
    let exif = exif_reader.read_from_container(&mut buf_reader)?;

    let camera = exif
        .get_field(Tag::Model, In::PRIMARY)
        .map(|f| f.display_value().to_string().trim_matches('"').to_string());

    let lens = exif
        .get_field(Tag::LensModel, In::PRIMARY)
        .map(|f| f.display_value().to_string().trim_matches('"').to_string());

    let focal_length = exif
        .get_field(Tag::FocalLength, In::PRIMARY)
        .map(|f| f.display_value().to_string());

    let aperture = exif
        .get_field(Tag::FNumber, In::PRIMARY)
        .map(|f| f.display_value().to_string());

    let shutter_speed = exif
        .get_field(Tag::ExposureTime, In::PRIMARY)
        .map(|f| f.display_value().to_string());

    let iso = exif
        .get_field(Tag::PhotographicSensitivity, In::PRIMARY)
        .map(|f| f.display_value().to_string());

    // v1.5.46 — Pick the OLDEST plausible EXIF date (DateTimeOriginal,
    // DateTimeDigitized, or DateTime) so a re-saved photo doesn't surface
    // its 2022 save date as the "Date Taken" when 2010 is also written
    // in DateTimeDigitized. Detail-panel display goes through this path,
    // so the user sees the same oldest-date logic the scanner uses.
    let date_taken = oldest_exif_date(&exif);

    let (gps_lat, gps_lon, gps_alt) = extract_gps(&exif);

    Ok(PhotoExif {
        camera,
        lens,
        focal_length,
        aperture,
        shutter_speed,
        iso,
        date_taken,
        gps_lat,
        gps_lon,
        gps_alt,
    })
}

/// v1.5.46/47 — Return the oldest plausible date (after 1990, before now)
/// among the standard EXIF date tags. The kamadak-exif crate's
/// `display_value()` does NOT always emit the canonical "YYYY:MM:DD
/// HH:MM:SS" form — depending on the camera's EXIF dialect it can come
/// back as "YYYY-MM-DD HH:MM:SS" or even DD/MM/YYYY style. Previously
/// we only accepted the colon-separated form, which silently dropped
/// every dash-formatted tag and let the file mtime (often a recent
/// re-save date) win. Now we try every common variant.
fn oldest_exif_date(exif: &exif::Exif) -> Option<String> {
    use chrono::NaiveDateTime;
    const FORMATS: &[&str] = &[
        "%Y:%m:%d %H:%M:%S",
        "%Y-%m-%d %H:%M:%S",
        "%Y/%m/%d %H:%M:%S",
        "%d/%m/%Y %H:%M:%S",
        "%m/%d/%Y %H:%M:%S",
        "%Y:%m:%d %H:%M",
        "%Y-%m-%d %H:%M",
        "%d/%m/%Y %H:%M",
        "%m/%d/%Y %H:%M",
    ];
    let earliest = chrono::NaiveDate::from_ymd_opt(1990, 1, 1)
        .unwrap()
        .and_hms_opt(0, 0, 0)
        .unwrap();
    let latest = chrono::Utc::now().naive_utc();

    let mut best: Option<(NaiveDateTime, String)> = None;
    for tag in [Tag::DateTimeOriginal, Tag::DateTimeDigitized, Tag::DateTime] {
        if let Some(field) = exif.get_field(tag, In::PRIMARY) {
            let raw = field.display_value().to_string();
            let raw = raw.trim_matches('"').trim().to_string();
            let dt_opt = FORMATS.iter()
                .find_map(|f| NaiveDateTime::parse_from_str(&raw, f).ok());
            if let Some(dt) = dt_opt {
                if dt < earliest || dt > latest { continue; }
                if best.as_ref().map_or(true, |(b, _)| dt < *b) {
                    best = Some((dt, raw));
                }
            }
        }
    }
    best.map(|(_, s)| s)
}

/// Extract GPS coordinates from EXIF data
fn extract_gps(exif: &exif::Exif) -> (Option<f64>, Option<f64>, Option<f64>) {
    let lat = extract_gps_coord(exif, Tag::GPSLatitude, Tag::GPSLatitudeRef);
    let lon = extract_gps_coord(exif, Tag::GPSLongitude, Tag::GPSLongitudeRef);

    let alt = exif.get_field(Tag::GPSAltitude, In::PRIMARY).and_then(|f| {
        if let exif::Value::Rational(ref v) = f.value {
            v.first().and_then(|r| {
                if r.denom == 0 { None } else { Some(r.num as f64 / r.denom as f64) }
            })
        } else {
            None
        }
    });

    (lat, lon, alt)
}

fn extract_gps_coord(exif: &exif::Exif, coord_tag: Tag, ref_tag: Tag) -> Option<f64> {
    let field = exif.get_field(coord_tag, In::PRIMARY)?;
    let ref_field = exif.get_field(ref_tag, In::PRIMARY)?;

    if let exif::Value::Rational(ref values) = field.value {
        if values.len() >= 3
            && values[0].denom != 0
            && values[1].denom != 0
            && values[2].denom != 0
        {
            let degrees = values[0].num as f64 / values[0].denom as f64;
            let minutes = values[1].num as f64 / values[1].denom as f64;
            let seconds = values[2].num as f64 / values[2].denom as f64;

            let mut coord = degrees + minutes / 60.0 + seconds / 3600.0;

            let ref_str = ref_field.display_value().to_string();
            let ref_clean = ref_str.trim_matches('"');
            if ref_clean == "S" || ref_clean == "W" {
                coord = -coord;
            }

            return Some(coord);
        }
    }
    None
}

/// Compute a true pHash (perceptual hash, DCT-based) for duplicate detection.
///
/// ## Why DCT and not simple average hash?
/// The original version of this function was *average hash* (aHash) — resize
/// to 8×8, compare each pixel to the mean. aHash is the weakest perceptual
/// hash; it produces frequent false positives on near-solid images (clouds,
/// sunsets, low-light) and near-misses on simple edits. The research
/// (Farid 2018, ScienceDirect 2023 Hamming-distribution study) consistently
/// shows pHash-DCT produces normally-distributed Hamming distances and is
/// robust to JPEG compression, brightness shifts, small crops, and the
/// kind of colour tweaks phone cameras apply between shots.
///
/// ## Algorithm
///   1. Load the image **with EXIF orientation applied** (portrait photos
///      stored landscape with orientation=6 must be rotated BEFORE hashing
///      or two copies of the same photo won't match).
///   2. Downscale to 32×32 grayscale (bigger than aHash → more signal).
///   3. Apply a 2-D Discrete Cosine Transform (Type-II).
///   4. Keep the top-left 8×8 block of DCT coefficients — the low-frequency
///      content (the "shape" of the image).
///   5. Drop the DC term (coefficient [0,0]) — it's just the average
///      brightness, irrelevant for perceptual similarity.
///   6. Compute the median of the remaining 63 coefficients.
///   7. Set bit i of the hash if the i-th coefficient > median.
///
/// Output is still 64 hex chars wide (`{:016x}`) so the DB column is
/// unchanged; existing callers work without modification. The STATISTICAL
/// meaning of hamming distance is different though — callers expecting the
/// old aHash behaviour must be re-tuned. We ship a one-time migration
/// that clears old phash values so they get recomputed with the new algo.
pub fn compute_phash(path: &str) -> Result<String> {
    // Apply EXIF rotation: landscape-stored portrait photos must be rotated
    // before hashing so two copies (one rotated, one not) match.
    let img = crate::thumbnail::open_image(path)?;
    compute_phash_from_image(&img)
}

/// Faster variant: hash an already-loaded image. Use this when you've
/// already opened a cached thumbnail and want to avoid the second file-read.
/// The cached thumbnail was created with EXIF rotation applied at scan time,
/// so this produces the same hash as `compute_phash(original_path)`.
pub fn compute_phash_from_image(img: &image::DynamicImage) -> Result<String> {
    // 32×32 grayscale — enough signal for DCT to find structure.
    let small = img.resize_exact(32, 32, image::imageops::FilterType::Triangle);
    let gray = small.to_luma8();

    // Collect as f64 matrix for the DCT.
    let mut mat = [[0.0f64; 32]; 32];
    for y in 0..32 {
        for x in 0..32 {
            mat[y][x] = gray.get_pixel(x as u32, y as u32).0[0] as f64;
        }
    }

    // 2-D DCT-II: apply 1-D DCT row-wise, then column-wise. We only need
    // the top-left 8×8 of the output so we hand-roll the summation and
    // skip the rest — this is ~O(32 × 32 × 8) work, a few microseconds.
    let dct = compute_dct_low_freq_8x8(&mat);

    // Flatten 8×8 = 64 coefficients. The DC term [0,0] is just the mean
    // brightness — noisy and non-discriminative — so we exclude it from
    // the median computation.
    let mut coeffs = Vec::with_capacity(63);
    for y in 0..8 {
        for x in 0..8 {
            if y == 0 && x == 0 { continue; }
            coeffs.push(dct[y][x]);
        }
    }
    let mut sorted = coeffs.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median = sorted[sorted.len() / 2];

    // Pack 64 bits: bit 0 is always 0 (we dropped DC), bits 1..64 are
    // "coefficient > median" for the remaining positions in row-major order.
    let mut hash: u64 = 0;
    let mut idx = 0;
    for y in 0..8 {
        for x in 0..8 {
            if y == 0 && x == 0 { idx += 1; continue; }
            if dct[y][x] > median {
                hash |= 1u64 << idx;
            }
            idx += 1;
        }
    }

    Ok(format!("{:016x}", hash))
}

/// 2-D DCT-II producing the top-left 8×8 low-frequency block from a 32×32
/// grayscale input. Uses the standard DCT-II formula:
///
/// ```text
///   X_u = sum_{n=0..N-1} x_n * cos[ π/N * (n + 1/2) * u ]
/// ```
///
/// applied row-wise, then column-wise. We compute a 32×8 intermediate,
/// then an 8×8 final — O(N²·K) where K=8 instead of O(N²·N), a 4× win.
fn compute_dct_low_freq_8x8(input: &[[f64; 32]; 32]) -> [[f64; 8]; 8] {
    // Precompute cosine table: cos_table[u][n] = cos(π/32 * (n+0.5) * u)
    // We only need u ∈ 0..8.
    let mut cos_table = [[0.0f64; 32]; 8];
    for u in 0..8 {
        for n in 0..32 {
            let angle = std::f64::consts::PI * (n as f64 + 0.5) * (u as f64) / 32.0;
            cos_table[u][n] = angle.cos();
        }
    }

    // Row-wise DCT: intermediate[y][u] = sum_x input[y][x] * cos_table[u][x]
    let mut intermediate = [[0.0f64; 8]; 32];
    for y in 0..32 {
        for u in 0..8 {
            let mut sum = 0.0;
            for x in 0..32 {
                sum += input[y][x] * cos_table[u][x];
            }
            intermediate[y][u] = sum;
        }
    }

    // Column-wise DCT on the intermediate 32×8 to get final 8×8.
    let mut out = [[0.0f64; 8]; 8];
    for v in 0..8 {
        for u in 0..8 {
            let mut sum = 0.0;
            for y in 0..32 {
                sum += intermediate[y][u] * cos_table[v][y];
            }
            out[v][u] = sum;
        }
    }
    out
}
