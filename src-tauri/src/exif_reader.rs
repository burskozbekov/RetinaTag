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

    let date_taken = exif
        .get_field(Tag::DateTimeOriginal, In::PRIMARY)
        .map(|f| f.display_value().to_string().trim_matches('"').to_string());

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

/// Compute a perceptual hash for an image (for duplicate detection)
/// Uses average hash: resize to 8x8, grayscale, compare to mean
pub fn compute_phash(path: &str) -> Result<String> {
    let img = image::open(path)?;
    let small = img.resize_exact(8, 8, image::imageops::FilterType::Lanczos3);
    let gray = small.to_luma8();

    let pixels: Vec<u8> = gray.pixels().map(|p| p.0[0]).collect();
    let mean: f64 = pixels.iter().map(|&p| p as f64).sum::<f64>() / 64.0;

    let mut hash = 0u64;
    for (i, &pixel) in pixels.iter().enumerate() {
        if pixel as f64 > mean {
            hash |= 1 << i;
        }
    }

    Ok(format!("{:016x}", hash))
}
