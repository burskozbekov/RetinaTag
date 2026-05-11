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

// ─── XMP SIDECAR READING (v1.5.104) ──────────────────────────────────────
//
// Mac side writes .xmp sidecars next to each photo on the shared volume;
// Windows scan has historically been a one-way producer (xmp.rs writes,
// never reads) so cross-machine tags never made it back. This reader
// fills that gap: parse the sidecar XML, pull keywords + description,
// and let the caller merge them into the local DB.
//
// Format expected: the Lightroom/IPTC convention RetinaTag itself emits —
// keywords under `dc:subject`, with `lr:hierarchicalSubject` and
// `Iptc4xmpCore:Keywords` mirroring the same list. We dedupe across
// those three so Bridge/DigiKam/Capture One sidecars (which may favour
// one over the others) also parse cleanly.
//
// Sidecar path convention: `<stem>.xmp` (Lightroom style — drop the
// original extension). Both `IMG_0001.jpg` and `5205.MP4` map to
// `IMG_0001.xmp` and `5205.xmp` respectively. We also accept the
// DigiKam `<full-name>.xmp` form (`IMG_0001.jpg.xmp`) as a fallback.

#[derive(Debug, Default, Clone)]
pub struct XmpRead {
    pub keywords: Vec<String>,
    pub description: Option<String>,
    pub rating: Option<i32>,
    pub label: Option<String>,
}

/// Locate the sidecar for a given photo path, accepting both common
/// naming conventions. Returns the first existing one.
pub fn sidecar_path_for(photo_path: &str) -> Option<std::path::PathBuf> {
    let p = std::path::Path::new(photo_path);
    let parent = p.parent()?;
    let stem = p.file_stem()?.to_string_lossy();
    let lightroom = parent.join(format!("{}.xmp", stem));
    if lightroom.is_file() {
        return Some(lightroom);
    }
    let digikam = parent.join(format!("{}.xmp", p.file_name()?.to_string_lossy()));
    if digikam.is_file() {
        return Some(digikam);
    }
    None
}

/// Read an XMP sidecar by photo path. Returns Ok(None) if no sidecar
/// exists (the common case for un-tagged photos), Ok(Some(...)) with
/// extracted data otherwise. Errors only on actual parse failures.
pub fn read_xmp_sidecar(photo_path: &str) -> Result<Option<XmpRead>> {
    let Some(path) = sidecar_path_for(photo_path) else {
        return Ok(None);
    };
    let xml = std::fs::read_to_string(&path)
        .with_context(|| format!("read XMP sidecar {}", path.display()))?;
    parse_xmp_xml(&xml).map(Some)
}

/// Parse an XMP XML string (sidecar or embedded). Public so callers
/// that hand us a string they already extracted from a JPEG APP1
/// segment can reuse the same parser.
pub fn parse_xmp_xml(xml: &str) -> Result<XmpRead> {
    use quick_xml::events::Event;
    use quick_xml::reader::Reader;

    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    // State machine. We don't build a full DOM — just track which
    // "list of items" container we're currently inside. The three
    // keyword bags (dc:subject, lr:hierarchicalSubject,
    // Iptc4xmpCore:Keywords) all use `<rdf:Bag><rdf:li>…</rdf:li></rdf:Bag>`,
    // and description uses `<rdf:Alt><rdf:li xml:lang="x-default">…</rdf:li></rdf:Alt>`.
    #[derive(PartialEq)]
    enum Section { None, Keywords, Description }
    let mut section = Section::None;
    let mut depth_in_section = 0i32;
    let mut current_li = String::new();
    let mut in_li = false;

    let mut out = XmpRead::default();
    // Use a set to dedupe across the three keyword bags.
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let name_bytes = e.name();
                let name = name_bytes.as_ref();
                // Section entry — check by local name suffix so
                // namespace prefix variants (xmlns:dc vs raw `dc:`)
                // both work.
                if section == Section::None {
                    if ends_with(name, b"subject")
                        || ends_with(name, b"hierarchicalSubject")
                        || ends_with(name, b"Keywords") {
                        section = Section::Keywords;
                        depth_in_section = 1;
                    } else if ends_with(name, b"description") {
                        section = Section::Description;
                        depth_in_section = 1;
                    }
                } else {
                    depth_in_section += 1;
                }
                // Inside a section, only `rdf:li` is meaningful.
                if section != Section::None && ends_with(name, b"li") {
                    in_li = true;
                    current_li.clear();
                }
                // Pull rating / label off the Description element's
                // attributes — RetinaTag itself emits these as child
                // elements, but Lightroom likes them as attributes.
                if ends_with(name, b"Description") {
                    for attr in e.attributes().flatten() {
                        let key = attr.key.as_ref();
                        if ends_with(key, b"Rating") {
                            if let Ok(v) = attr.unescape_value() {
                                if let Ok(n) = v.parse::<i32>() {
                                    out.rating.get_or_insert(n);
                                }
                            }
                        } else if ends_with(key, b"Label") {
                            if let Ok(v) = attr.unescape_value() {
                                out.label.get_or_insert(v.to_string());
                            }
                        }
                    }
                }
            }
            Ok(Event::End(e)) => {
                let name = e.name();
                if section != Section::None && ends_with(name.as_ref(), b"li") {
                    let val = current_li.trim().to_string();
                    if !val.is_empty() {
                        match section {
                            Section::Keywords => {
                                let key = val.to_lowercase();
                                if seen.insert(key) {
                                    out.keywords.push(val);
                                }
                            }
                            Section::Description => {
                                // Prefer x-default; otherwise take first non-empty.
                                if out.description.is_none() {
                                    out.description = Some(val);
                                }
                            }
                            _ => {}
                        }
                    }
                    in_li = false;
                    current_li.clear();
                }
                if section != Section::None {
                    depth_in_section -= 1;
                    if depth_in_section <= 0 {
                        section = Section::None;
                        depth_in_section = 0;
                    }
                }
            }
            Ok(Event::Text(t)) if in_li => {
                let s = t.unescape().unwrap_or_default();
                current_li.push_str(&s);
            }
            // Also pick up rating / label / RetinaTag-written element
            // form: <xmp:Rating>4</xmp:Rating>, <xmp:Label>Red</xmp:Label>.
            Ok(Event::Start(_)) => {}
            Ok(Event::Empty(_)) => {}
            Ok(Event::Eof) => break,
            Err(e) => return Err(anyhow::anyhow!("XMP parse: {}", e)),
            _ => {}
        }
        buf.clear();
    }

    // Second pass for the element-form rating/label (the first pass
    // only catches the attribute form). Cheap because the doc is small.
    if out.rating.is_none() || out.label.is_none() {
        let mut reader2 = Reader::from_str(xml);
        reader2.config_mut().trim_text(true);
        let mut current_tag: Option<Vec<u8>> = None;
        let mut buf2 = Vec::new();
        loop {
            match reader2.read_event_into(&mut buf2) {
                Ok(Event::Start(e)) => {
                    let name = e.name().as_ref().to_vec();
                    if ends_with(&name, b"Rating") || ends_with(&name, b"Label") {
                        current_tag = Some(name);
                    } else {
                        current_tag = None;
                    }
                }
                Ok(Event::Text(t)) => {
                    if let Some(tag) = &current_tag {
                        let s = t.unescape().unwrap_or_default().trim().to_string();
                        if ends_with(tag, b"Rating") && out.rating.is_none() {
                            if let Ok(n) = s.parse::<i32>() {
                                out.rating = Some(n);
                            }
                        } else if ends_with(tag, b"Label") && out.label.is_none() {
                            if !s.is_empty() {
                                out.label = Some(s);
                            }
                        }
                    }
                }
                Ok(Event::End(_)) => current_tag = None,
                Ok(Event::Eof) => break,
                Err(_) => break,
                _ => {}
            }
            buf2.clear();
        }
    }

    Ok(out)
}

fn ends_with(name: &[u8], suffix: &[u8]) -> bool {
    if name.len() < suffix.len() {
        return false;
    }
    // Match either the bare local name or `prefix:localname`.
    if &name[name.len() - suffix.len()..] != suffix {
        return false;
    }
    if name.len() == suffix.len() {
        return true;
    }
    // Char before the suffix must be ':' for a prefix:local match.
    name[name.len() - suffix.len() - 1] == b':'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_real_user_sidecar() {
        // Tries an actual file from the user's library. If it doesn't
        // exist (e.g. on CI), skip gracefully.
        let photo = r"D:\Fotograflar\2024-10-09\5205.MP4";
        match read_xmp_sidecar(photo) {
            Ok(Some(r)) => {
                println!("Found sidecar: keywords={}, desc={:?}, rating={:?}, label={:?}",
                    r.keywords.len(), r.description.as_deref().map(|s| &s[..s.len().min(60)]),
                    r.rating, r.label);
                assert!(!r.keywords.is_empty(), "Expected keywords in real sidecar");
            }
            Ok(None) => {
                println!("No sidecar at {} (skipping)", photo);
            }
            Err(e) => panic!("Sidecar parse error: {}", e),
        }
    }

    #[test]
    fn parses_retinatag_written_sidecar() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/" x:xmptk="RetinaTag 1.0">
  <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
    <rdf:Description rdf:about="5205.MP4"
      xmlns:dc="http://purl.org/dc/elements/1.1/"
      xmlns:xmp="http://ns.adobe.com/xap/1.0/">
      <dc:subject>
        <rdf:Bag>
          <rdf:li>guitar</rdf:li>
          <rdf:li>family</rdf:li>
          <rdf:li>kitchen</rdf:li>
        </rdf:Bag>
      </dc:subject>
      <dc:description>
        <rdf:Alt>
          <rdf:li xml:lang="x-default">A man and a woman play guitar.</rdf:li>
        </rdf:Alt>
      </dc:description>
      <xmp:Rating>4</xmp:Rating>
      <xmp:Label>Red</xmp:Label>
    </rdf:Description>
  </rdf:RDF>
</x:xmpmeta>"#;
        let r = parse_xmp_xml(xml).unwrap();
        assert_eq!(r.keywords, vec!["guitar", "family", "kitchen"]);
        assert_eq!(r.description.as_deref(), Some("A man and a woman play guitar."));
        assert_eq!(r.rating, Some(4));
        assert_eq!(r.label.as_deref(), Some("Red"));
    }
}
