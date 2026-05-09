use anyhow::{Context, Result};
use std::path::Path;

/// All data needed to write a comprehensive XMP sidecar.
pub struct XmpData {
    pub photo_path: String,
    pub tags: Vec<String>,
    pub rating: i32,                    // 0=unrated, 1-5 stars, -1=rejected
    pub favorite: bool,
    pub description: Option<String>,    // AI-generated description
    pub location: Option<String>,       // Estimated location string
    pub img_width: u32,
    pub img_height: u32,
    pub faces: Vec<XmpFace>,
}

pub struct XmpFace {
    pub name: String,
    /// Normalised centre-X  (0.0–1.0)
    pub cx: f32,
    /// Normalised centre-Y  (0.0–1.0)
    pub cy: f32,
    /// Normalised width     (0.0–1.0)
    pub w: f32,
    /// Normalised height    (0.0–1.0)
    pub h: f32,
}

/// Write a rich XMP sidecar next to the original photo.
///
/// Sidecar path: `<stem>.xmp`  (e.g. `IMG_0001.jpg` → `IMG_0001.xmp`)
/// This is the standard expected by Lightroom, Bridge, DigiKam, etc.
///
/// Metadata written:
///  - Keywords / Tags  (dc:subject, lr:hierarchicalSubject, IPTC)
///  - Person names     (added to keywords so any app sees them)
///  - Rating           (xmp:Rating – 1-5 stars; -1 = rejected)
///  - Favourite        (xmp:Label = "Red" – appears as colour label)
///  - AI description   (dc:description)
///  - Location         (Iptc4xmpCore:Location)
///  - Face regions     (MWG mwg-rs:Regions – DigiKam, some Android apps)
pub fn write_xmp_full(data: &XmpData) -> Result<String> {
    let path = Path::new(&data.photo_path);
    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
    let parent = path.parent().unwrap_or(Path::new(""));
    let xmp_path = parent.join(format!("{}.xmp", stem));
    let xmp_path_str = xmp_path.to_string_lossy().to_string();

    let filename = path.file_name().unwrap_or_default().to_string_lossy();

    // ── Build tag list (tags + person names, deduplicated) ───────────────────
    let mut all_tags = data.tags.clone();
    for face in &data.faces {
        let n = face.name.trim().to_string();
        if !n.is_empty() && !all_tags.iter().any(|t| t.eq_ignore_ascii_case(&n)) {
            all_tags.push(n);
        }
    }
    let tag_xml = all_tags.iter()
        .map(|t| format!("          <rdf:li>{}</rdf:li>", xml(t)))
        .collect::<Vec<_>>()
        .join("\n");

    // ── Rating ───────────────────────────────────────────────────────────────
    let rating_xml = match data.rating {
        r if r > 0 => format!("\n      <xmp:Rating>{}</xmp:Rating>", r.min(5)),
        -1          => "\n      <xmp:Rating>-1</xmp:Rating>".to_string(),
        _           => String::new(),
    };

    // ── Favourite → Lightroom colour label ───────────────────────────────────
    let label_xml = if data.favorite {
        "\n      <xmp:Label>Red</xmp:Label>".to_string()
    } else {
        String::new()
    };

    // ── AI description ───────────────────────────────────────────────────────
    let desc_xml = match &data.description {
        Some(d) if !d.trim().is_empty() => format!(
            "\n      <dc:description>\
             \n        <rdf:Alt>\
             \n          <rdf:li xml:lang=\"x-default\">{}</rdf:li>\
             \n        </rdf:Alt>\
             \n      </dc:description>",
            xml(d)
        ),
        _ => String::new(),
    };

    // ── Location ─────────────────────────────────────────────────────────────
    let loc_xml = match &data.location {
        Some(l) if !l.trim().is_empty() => format!(
            "\n      <Iptc4xmpCore:Location>{}</Iptc4xmpCore:Location>",
            xml(l)
        ),
        _ => String::new(),
    };

    // ── Face regions (MWG standard) ──────────────────────────────────────────
    let faces_xml = if !data.faces.is_empty() && data.img_width > 0 && data.img_height > 0 {
        let items: String = data.faces.iter()
            .map(|f| format!(
                "        <rdf:li>\n\
                 \x20         <rdf:Description mwg-rs:Name=\"{}\" mwg-rs:Type=\"Face\">\n\
                 \x20           <mwg-rs:Area>\n\
                 \x20             <rdf:Description\n\
                 \x20               stArea:x=\"{:.6}\"\n\
                 \x20               stArea:y=\"{:.6}\"\n\
                 \x20               stArea:w=\"{:.6}\"\n\
                 \x20               stArea:h=\"{:.6}\"\n\
                 \x20               stArea:unit=\"normalized\"/>\n\
                 \x20           </mwg-rs:Area>\n\
                 \x20         </rdf:Description>\n\
                 \x20       </rdf:li>",
                xml(&f.name), f.cx, f.cy, f.w, f.h
            ))
            .collect::<Vec<_>>()
            .join("\n");

        format!(
            "\n      <mwg-rs:Regions>\n\
             \x20       <rdf:Description>\n\
             \x20         <mwg-rs:AppliedToDimensions>\n\
             \x20           <rdf:Description stDim:w=\"{}\" stDim:h=\"{}\" stDim:unit=\"pixel\"/>\n\
             \x20         </mwg-rs:AppliedToDimensions>\n\
             \x20         <mwg-rs:RegionList>\n\
             \x20           <rdf:Bag>\n\
             {}\n\
             \x20           </rdf:Bag>\n\
             \x20         </mwg-rs:RegionList>\n\
             \x20       </rdf:Description>\n\
             \x20     </mwg-rs:Regions>",
            data.img_width, data.img_height, items
        )
    } else {
        String::new()
    };

    // ── Assemble final XMP ───────────────────────────────────────────────────
    let xmp = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/" x:xmptk="RetinaTag 1.0">
  <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
    <rdf:Description rdf:about="{filename}"
      xmlns:dc="http://purl.org/dc/elements/1.1/"
      xmlns:xmp="http://ns.adobe.com/xap/1.0/"
      xmlns:lr="http://ns.adobe.com/lightroom/1.0/"
      xmlns:Iptc4xmpCore="http://iptc.org/std/Iptc4xmpCore/1.0/xmlns/"
      xmlns:mwg-rs="http://www.metadataworkinggroup.com/schemas/regions/"
      xmlns:stArea="http://ns.adobe.com/xmp/sType/Area#"
      xmlns:stDim="http://ns.adobe.com/xmp/sType/Dimensions#">

      <!-- Keywords / Tags (+ person names) -->
      <dc:subject>
        <rdf:Bag>
{tag_xml}
        </rdf:Bag>
      </dc:subject>
      <lr:hierarchicalSubject>
        <rdf:Bag>
{tag_xml}
        </rdf:Bag>
      </lr:hierarchicalSubject>
      <Iptc4xmpCore:Keywords>
        <rdf:Bag>
{tag_xml}
        </rdf:Bag>
      </Iptc4xmpCore:Keywords>{rating_xml}{label_xml}{desc_xml}{loc_xml}{faces_xml}
    </rdf:Description>
  </rdf:RDF>
</x:xmpmeta>
"#
    );

    std::fs::write(&xmp_path, &xmp).context("Failed to write XMP sidecar")?;
    Ok(xmp_path_str)
}

// ─── v1.5.58 — XMP STRING BUILDER ────────────────────────────────────────────
//
// Same XMP body as `write_xmp_full`, but returns the XML as a String
// without touching the filesystem. Used by `embed_xmp_in_jpeg` so the
// in-file APP1 segment carries the same metadata as the sidecar.

pub fn build_xmp_string(data: &XmpData) -> String {
    let path = Path::new(&data.photo_path);
    let filename = path.file_name().unwrap_or_default().to_string_lossy();
    let mut all_tags = data.tags.clone();
    for face in &data.faces {
        let n = face.name.trim().to_string();
        if !n.is_empty() && !all_tags.iter().any(|t| t.eq_ignore_ascii_case(&n)) {
            all_tags.push(n);
        }
    }
    let tag_xml = all_tags.iter()
        .map(|t| format!("          <rdf:li>{}</rdf:li>", xml(t)))
        .collect::<Vec<_>>()
        .join("\n");
    let rating_xml = match data.rating {
        r if r > 0 => format!("\n      <xmp:Rating>{}</xmp:Rating>", r.min(5)),
        -1          => "\n      <xmp:Rating>-1</xmp:Rating>".to_string(),
        _           => String::new(),
    };
    let label_xml = if data.favorite {
        "\n      <xmp:Label>Red</xmp:Label>".to_string()
    } else { String::new() };
    let desc_xml = match &data.description {
        Some(d) if !d.trim().is_empty() => format!(
            "\n      <dc:description>\n        <rdf:Alt>\n          <rdf:li xml:lang=\"x-default\">{}</rdf:li>\n        </rdf:Alt>\n      </dc:description>",
            xml(d)
        ),
        _ => String::new(),
    };
    let loc_xml = match &data.location {
        Some(l) if !l.trim().is_empty() => format!(
            "\n      <Iptc4xmpCore:Location>{}</Iptc4xmpCore:Location>", xml(l)
        ),
        _ => String::new(),
    };
    let faces_xml = if !data.faces.is_empty() && data.img_width > 0 && data.img_height > 0 {
        let items: String = data.faces.iter().map(|f| format!(
            "        <rdf:li>\n          <rdf:Description mwg-rs:Name=\"{}\" mwg-rs:Type=\"Face\">\n            <mwg-rs:Area>\n              <rdf:Description\n                stArea:x=\"{:.6}\"\n                stArea:y=\"{:.6}\"\n                stArea:w=\"{:.6}\"\n                stArea:h=\"{:.6}\"\n                stArea:unit=\"normalized\"/>\n            </mwg-rs:Area>\n          </rdf:Description>\n        </rdf:li>",
            xml(&f.name), f.cx, f.cy, f.w, f.h
        )).collect::<Vec<_>>().join("\n");
        format!(
            "\n      <mwg-rs:Regions>\n        <rdf:Description>\n          <mwg-rs:AppliedToDimensions>\n            <rdf:Description stDim:w=\"{}\" stDim:h=\"{}\" stDim:unit=\"pixel\"/>\n          </mwg-rs:AppliedToDimensions>\n          <mwg-rs:RegionList>\n            <rdf:Bag>\n{}\n            </rdf:Bag>\n          </mwg-rs:RegionList>\n        </rdf:Description>\n      </mwg-rs:Regions>",
            data.img_width, data.img_height, items
        )
    } else { String::new() };
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/" x:xmptk="RetinaTag 1.0">
  <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
    <rdf:Description rdf:about="{filename}"
      xmlns:dc="http://purl.org/dc/elements/1.1/"
      xmlns:xmp="http://ns.adobe.com/xap/1.0/"
      xmlns:lr="http://ns.adobe.com/lightroom/1.0/"
      xmlns:Iptc4xmpCore="http://iptc.org/std/Iptc4xmpCore/1.0/xmlns/"
      xmlns:mwg-rs="http://www.metadataworkinggroup.com/schemas/regions/"
      xmlns:stArea="http://ns.adobe.com/xmp/sType/Area#"
      xmlns:stDim="http://ns.adobe.com/xmp/sType/Dimensions#">

      <dc:subject><rdf:Bag>
{tag_xml}
        </rdf:Bag></dc:subject>
      <lr:hierarchicalSubject><rdf:Bag>
{tag_xml}
        </rdf:Bag></lr:hierarchicalSubject>
      <Iptc4xmpCore:Keywords><rdf:Bag>
{tag_xml}
        </rdf:Bag></Iptc4xmpCore:Keywords>{rating_xml}{label_xml}{desc_xml}{loc_xml}{faces_xml}
    </rdf:Description>
  </rdf:RDF>
</x:xmpmeta>"#
    )
}

// ─── v1.5.58 — JPEG XMP EMBED ────────────────────────────────────────────────
//
// Inject the same metadata into the JPEG's APP1 (XMP) segment so the
// tags travel with the file when it's copied to iCloud, sent over
// Messenger, uploaded to a website, etc. Atomic: write to a temp file
// in the same folder, fsync, then rename over the original. If anything
// fails, the original is untouched.
//
// Only handles plain JPEG. PNG/HEIC/RAW silently skipped (caller can
// still write the sidecar). Errors are returned but RetinaTag's batch
// callers swallow them — embed is best-effort.

const XMP_NS_PREFIX: &[u8] = b"http://ns.adobe.com/xap/1.0/\0";

/// Embed (or replace) an XMP APP1 segment in a JPEG file. Atomic write
/// preserves the original on any failure path.
pub fn embed_xmp_in_jpeg(jpeg_path: &str, xmp_str: &str) -> Result<()> {
    use img_parts::{jpeg::Jpeg, jpeg::markers, Bytes};
    let path = Path::new(jpeg_path);
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
    if !matches!(ext.as_str(), "jpg" | "jpeg") {
        // Only JPEG for now. PNG iTXt and HEIC metadata-box use other
        // libraries we'd need to pull in separately.
        return Ok(());
    }

    let raw = std::fs::read(path).context("read JPEG for XMP embed")?;
    let mut jpeg = Jpeg::from_bytes(raw.into())
        .map_err(|e| anyhow::anyhow!("Not a valid JPEG: {}", e))?;

    // Drop ALL existing APP1 segments whose payload starts with the
    // Adobe XMP namespace marker. (Other APP1 segments — eg. EXIF —
    // stay intact.)
    let mut keep: Vec<img_parts::jpeg::JpegSegment> = Vec::new();
    for seg in jpeg.segments() {
        let drop_it = seg.marker() == markers::APP1
            && seg.contents().starts_with(XMP_NS_PREFIX);
        if !drop_it {
            keep.push(seg.clone());
        }
    }
    jpeg.segments_mut().clear();
    for s in keep { jpeg.segments_mut().push(s); }

    // Build the new APP1 payload: namespace marker + XMP bytes.
    let mut payload = Vec::with_capacity(XMP_NS_PREFIX.len() + xmp_str.len());
    payload.extend_from_slice(XMP_NS_PREFIX);
    payload.extend_from_slice(xmp_str.as_bytes());
    let segment = img_parts::jpeg::JpegSegment::new_with_contents(
        markers::APP1,
        Bytes::from(payload),
    );

    // Insert the XMP APP1 right after the SOI / JFIF (img-parts puts
    // segments in array order; we want XMP early so readers find it).
    // The simplest is: push at front by inserting at position 0 of the
    // segments vec (image data goes through other markers anyway).
    jpeg.segments_mut().insert(0, segment);

    // Atomic write: temp file in same dir → fsync → rename over.
    let parent = path.parent().unwrap_or(Path::new(""));
    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
    let tmp = parent.join(format!(".{}.xmpwrite.tmp", stem));
    {
        let mut f = std::fs::File::create(&tmp).context("create tmp for atomic XMP write")?;
        jpeg.encoder().write_to(&mut f).context("write JPEG with XMP")?;
        use std::io::Write as _;
        f.flush().ok();
    }
    std::fs::rename(&tmp, path).context("atomic rename over original")?;
    Ok(())
}

/// Legacy wrapper — still used by write_xmp_batch (tags only).
pub fn write_xmp_sidecar(photo_path: &str, tags: &[String]) -> Result<String> {
    write_xmp_full(&XmpData {
        photo_path: photo_path.to_string(),
        tags: tags.to_vec(),
        rating: 0,
        favorite: false,
        description: None,
        location: None,
        img_width: 0,
        img_height: 0,
        faces: vec![],
    })
}

/// Batch write (tags only — legacy).
pub fn write_xmp_batch(
    photos: &[(String, Vec<String>)],
) -> Vec<(String, std::result::Result<String, String>)> {
    photos
        .iter()
        .map(|(path, tags)| {
            let result = write_xmp_sidecar(path, tags).map_err(|e| e.to_string());
            (path.clone(), result)
        })
        .collect()
}

fn xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}
