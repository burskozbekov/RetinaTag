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
    /// v1.5.226 — Capture date for round-trip sync with Mac. DB stores
    /// "YYYY-MM-DD HH:MM:SS"; build_xmp_string emits the same value in
    /// three Adobe-standard tags (photoshop:DateCreated, exif:
    /// DateTimeOriginal, xmp:CreateDate) so any reader picks it up.
    /// None = omit the tags entirely (skip date fields).
    pub date_taken: Option<String>,
    /// v1.5.232 — Vault membership for the shared SMB vault. When the
    /// row's `private = 1` in the DB the writer emits
    /// `<retinatag:Private>true</retinatag:Private>` so a second
    /// machine that mounts the same library + reads the sidecar
    /// flips its own private flag without a DB round-trip.
    pub private: bool,
    /// v1.5.232 — Companion oid for the encrypted blob — written when
    /// non-empty so the second machine knows which .rtenc to fetch
    /// under `<library_root>/.retinatag-vault/objects/<oid[0:2]>/<oid>.rtenc`.
    pub vault_oid: Option<String>,
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
    // v1.5.226 — Emit the capture date in three Adobe-standard tags so
    // any XMP-aware reader picks it up. DB format is "YYYY-MM-DD HH:MM:SS";
    // XMP-correct shape is "YYYY-MM-DDTHH:MM:SS" (the T separator is what
    // exif:DateTimeOriginal expects per the XMP spec). We accept both
    // shapes from incoming sidecars but always WRITE the canonical T form.
    let date_xml = match &data.date_taken {
        Some(d) if !d.trim().is_empty() => {
            let trimmed = d.trim();
            let iso = if trimmed.len() >= 11 && trimmed.as_bytes().get(10) == Some(&b' ') {
                let mut s = trimmed.to_string();
                s.replace_range(10..11, "T");
                s
            } else {
                trimmed.to_string()
            };
            let esc = xml(&iso);
            format!(
                "\n      <photoshop:DateCreated>{e}</photoshop:DateCreated>\
                 \n      <exif:DateTimeOriginal>{e}</exif:DateTimeOriginal>\
                 \n      <xmp:CreateDate>{e}</xmp:CreateDate>",
                e = esc
            )
        }
        _ => String::new(),
    };
    // v1.5.232 — Shared-vault membership in retinatag: namespace.
    // We emit Private+VaultOid even when private=false IF the row has
    // an oid (so a future "un-vault" round-trip propagates the flip).
    let vault_xml = {
        let oid_part = match &data.vault_oid {
            Some(o) if !o.trim().is_empty() =>
                format!("\n      <retinatag:VaultOid>{}</retinatag:VaultOid>", xml(o.trim())),
            _ => String::new(),
        };
        let private_part = if data.private {
            "\n      <retinatag:Private>true</retinatag:Private>".to_string()
        } else if !oid_part.is_empty() {
            // Row had an oid before but is no longer private — explicit
            // false so the other machine flips OFF too. Without this
            // the receiver would keep its old private=1 because the
            // XmpRead.private field defaults to None.
            "\n      <retinatag:Private>false</retinatag:Private>".to_string()
        } else {
            String::new()
        };
        format!("{}{}", private_part, oid_part)
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
      xmlns:photoshop="http://ns.adobe.com/photoshop/1.0/"
      xmlns:exif="http://ns.adobe.com/exif/1.0/"
      xmlns:retinatag="http://retinatag.app/xmp/1.0/"
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
        </rdf:Bag></Iptc4xmpCore:Keywords>{rating_xml}{label_xml}{desc_xml}{loc_xml}{date_xml}{vault_xml}{faces_xml}
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

/// v1.5.268 — Extract the XMP packet that's EMBEDDED inside a JPEG's
/// APP1 segment, returning the XML as a String. This is the counterpart
/// to `embed_xmp_in_jpeg`: where that one writes, this one reads.
///
/// Many JPEGs (Facebook exports, Photoshop output, modern phone
/// camera apps) carry their capture date in xmp:CreateDate or
/// photoshop:DateCreated INSIDE the file (not just in a sidecar)
/// while leaving the traditional EXIF DateTimeOriginal blank. The
/// scanner needs this signal — without it those files fall through
/// to "no real date" and the user sees them stamped with mtime.
///
/// Returns Ok(None) for non-JPEG, no-APP1-XMP, or any parse failure.
pub fn read_xmp_from_jpeg(jpeg_path: &str) -> Result<Option<String>> {
    use img_parts::{jpeg::Jpeg, jpeg::markers};
    let path = Path::new(jpeg_path);
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
    if !matches!(ext.as_str(), "jpg" | "jpeg" | "jpe") {
        return Ok(None);
    }
    let raw = std::fs::read(path).context("read JPEG for XMP extract")?;
    let jpeg = match Jpeg::from_bytes(raw.into()) {
        Ok(j) => j,
        Err(_) => return Ok(None),
    };
    for seg in jpeg.segments() {
        if seg.marker() != markers::APP1 { continue; }
        let body = seg.contents();
        if !body.starts_with(XMP_NS_PREFIX) { continue; }
        // Strip the namespace marker (incl. its trailing NUL).
        let xmp_bytes = &body[XMP_NS_PREFIX.len()..];
        // XMP is valid XML / UTF-8; lossy decode so malformed bytes
        // don't kill the whole parse.
        let s = String::from_utf8_lossy(xmp_bytes).into_owned();
        return Ok(Some(s));
    }
    Ok(None)
}

/// Legacy wrapper — still used by write_xmp_batch (tags only).
pub fn write_xmp_sidecar(photo_path: &str, tags: &[String]) -> Result<String> {
    write_xmp_full(&XmpData {
        photo_path: photo_path.to_string(),
        date_taken: None,
        private: false,
        vault_oid: None,
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
    /// v1.5.108 — MWG (Metadata Working Group) face regions. Mac side
    /// writes these alongside keywords whenever a face has been
    /// assigned to a person. Importing them lets the People sidebar
    /// surface Mac-named photos as proper face rows, not just tag
    /// strings.
    pub faces: Vec<XmpReadFace>,
    /// v1.5.232 — Capture date from any of the three Adobe-standard
    /// tags PC's writer emits (and Mac's reader expects). First match
    /// wins, in priority order: photoshop:DateCreated → exif:DateTimeOriginal
    /// → xmp:CreateDate. Stored verbatim as the XMP wrote it (typically
    /// "YYYY-MM-DDTHH:MM:SS"); the importer normalises to the DB shape
    /// "YYYY-MM-DD HH:MM:SS" downstream.
    pub date_taken: Option<String>,
    /// v1.5.232 — `<retinatag:Private>true</retinatag:Private>` flag from
    /// the shared-vault cross-sync. When true, the importer should set
    /// photos.private = 1 so the photo lives behind the vault PIN even
    /// if it landed on this machine via a folder scan and not via
    /// vault_add_paths.
    pub private: Option<bool>,
    /// v1.5.232 — `<retinatag:VaultOid>...` companion to the Private
    /// flag. Carries the SHA-256-hex oid that names the encrypted blob
    /// under <library_root>/.retinatag-vault/objects/<oid[0:2]>/<oid>.rtenc.
    /// Importer writes this to photos.vault_oid so the gallery can
    /// fetch the right bytes through decrypt_oid_to_bytes.
    pub vault_oid: Option<String>,
}

#[derive(Debug, Clone)]
pub struct XmpReadFace {
    pub name: String,
    /// Normalised center-X, 0.0–1.0 in the AppliedToDimensions space.
    pub cx: f32,
    pub cy: f32,
    pub w: f32,
    pub h: f32,
    /// Image width/height the regions were computed against. Used to
    /// turn normalised coords back into pixel boxes for face_regions.
    /// Both fields default to 0 if the XMP omits AppliedToDimensions;
    /// callers should fall back to the live photo size in that case.
    pub applied_w: u32,
    pub applied_h: u32,
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
    let mut out = parse_xmp_xml(&xml)?;
    // v1.5.394 — Drop catalog/stock keyword DUMPS. A sidecar carrying a huge
    // keyword list is not this photo's own keywords — we observed 192-584
    // identical keywords shared across unrelated photos (coffee, soldier,
    // airplane…), and importing them polluted every search (a sepia t-shirt
    // matched "coffee"). Real per-photo sidecars carry a handful, so a set
    // larger than the cap is treated as a dump: its keywords are discarded
    // (description / rating / face regions are still kept). This is the single
    // chokepoint for every sidecar caller and stops a rescan re-adding the
    // 152977 junk tags that were cleaned out of the DB.
    const MAX_SIDECAR_KEYWORDS: usize = 25;
    if out.keywords.len() > MAX_SIDECAR_KEYWORDS {
        eprintln!("[xmp] dropping {}-keyword dump sidecar: {}", out.keywords.len(), path.display());
        out.keywords.clear();
    }
    Ok(Some(out))
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

    // v1.5.348 — RetinaTag (and Lightroom, when not using attribute
    // form) emits Rating + Label as ELEMENT children of rdf:Description:
    //   <xmp:Rating>4</xmp:Rating>
    //   <xmp:Label>Red</xmp:Label>
    // Previous parser only picked these up as attributes on the
    // Description element, so XMPs RetinaTag itself wrote could not be
    // re-read — round-trip rating/favourite via .xmp sidecar was silently
    // broken.  Track when we're inside one of these scalar elements so
    // the Text event can be captured and parsed in End.
    let mut current_scalar = String::new();
    #[derive(PartialEq)]
    enum Scalar { None, Rating, Label }
    let mut in_scalar = Scalar::None;

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
                // v1.5.348 — element-form Rating/Label.  Only arm when
                // we're not inside a section (Keywords/Description),
                // otherwise a stray <Label> inside dc:description would
                // hijack the text capture.
                if section == Section::None {
                    if ends_with(name, b"Rating") {
                        in_scalar = Scalar::Rating;
                        current_scalar.clear();
                    } else if ends_with(name, b"Label") {
                        in_scalar = Scalar::Label;
                        current_scalar.clear();
                    }
                }
            }
            Ok(Event::End(e)) => {
                let name_bytes = e.name();
                let name = name_bytes.as_ref();
                if section != Section::None && ends_with(name, b"li") {
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
                // v1.5.348 — close element-form Rating/Label.  Match on
                // the same End name to avoid grabbing a value from a
                // mis-nested tag.  `get_or_insert` so the attribute-form
                // wins if both forms exist (Lightroom-paired XMPs).
                if in_scalar == Scalar::Rating && ends_with(name, b"Rating") {
                    if let Ok(n) = current_scalar.trim().parse::<i32>() {
                        out.rating.get_or_insert(n);
                    }
                    in_scalar = Scalar::None;
                    current_scalar.clear();
                } else if in_scalar == Scalar::Label && ends_with(name, b"Label") {
                    let v = current_scalar.trim().to_string();
                    if !v.is_empty() {
                        out.label.get_or_insert(v);
                    }
                    in_scalar = Scalar::None;
                    current_scalar.clear();
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
            // v1.5.348 — capture the body of <xmp:Rating> / <xmp:Label>.
            // Separate arm from `in_li` so concurrent open states don't
            // collide; in practice only one is active at a time but the
            // disjoint guards keep that local.
            Ok(Event::Text(t)) if in_scalar != Scalar::None => {
                let s = t.unescape().unwrap_or_default();
                current_scalar.push_str(&s);
            }
            Ok(Event::Empty(_)) => {}
            Ok(Event::Eof) => break,
            Err(e) => return Err(anyhow::anyhow!("XMP parse: {}", e)),
            _ => {}
        }
        buf.clear();
    }

    // v1.5.108 — second pass for MWG face regions. Easier to do as a
    // separate scan than to weave region state into the section
    // machine above. We pull every `<rdf:Description ... mwg-rs:Name=
    // mwg-rs:Type=Face>` and its associated stArea coords +
    // AppliedToDimensions. Format reference: Metadata Working Group
    // "Guidelines for Handling Image Metadata" Regions schema.
    let (mut applied_w, mut applied_h): (u32, u32) = (0, 0);
    {
        let mut r2 = Reader::from_str(xml);
        r2.config_mut().trim_text(true);
        let mut buf2 = Vec::new();
        loop {
            match r2.read_event_into(&mut buf2) {
                Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                    let name = e.name();
                    if ends_with(name.as_ref(), b"AppliedToDimensions")
                        || (ends_with(name.as_ref(), b"Description")
                            && e.attributes().flatten().any(|a| ends_with(a.key.as_ref(), b"w")))
                    {
                        for attr in e.attributes().flatten() {
                            let key = attr.key.as_ref();
                            if ends_with(key, b"w") {
                                if let Ok(v) = attr.unescape_value() {
                                    if let Ok(n) = v.parse::<u32>() { applied_w = n; }
                                }
                            } else if ends_with(key, b"h") {
                                if let Ok(v) = attr.unescape_value() {
                                    if let Ok(n) = v.parse::<u32>() { applied_h = n; }
                                }
                            }
                        }
                    }
                }
                Ok(Event::Eof) => break,
                Err(_) => break,
                _ => {}
            }
            buf2.clear();
        }
    }
    {
        let mut r3 = Reader::from_str(xml);
        r3.config_mut().trim_text(true);
        let mut buf3 = Vec::new();
        // Track open Description's mwg-rs:Name + Type as we walk, then
        // when we hit its child rdf:Description with stArea:x/y/w/h,
        // emit a face.
        let mut pending_name: Option<String> = None;
        let mut pending_is_face = false;
        loop {
            match r3.read_event_into(&mut buf3) {
                Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                    let local = e.name();
                    if ends_with(local.as_ref(), b"Description") {
                        // Check attrs for mwg-rs:Name / Type=Face — this
                        // tags the current Description as a face region.
                        let mut nm: Option<String> = None;
                        let mut is_face = false;
                        let mut cx = None;
                        let mut cy = None;
                        let mut w = None;
                        let mut h = None;
                        for attr in e.attributes().flatten() {
                            let key = attr.key.as_ref();
                            let val = attr.unescape_value().unwrap_or_default().to_string();
                            if ends_with(key, b"Name") { nm = Some(val); }
                            else if ends_with(key, b"Type") && val.eq_ignore_ascii_case("Face") { is_face = true; }
                            else if ends_with(key, b"x") { cx = val.parse::<f32>().ok(); }
                            else if ends_with(key, b"y") { cy = val.parse::<f32>().ok(); }
                            else if ends_with(key, b"w") { w = val.parse::<f32>().ok(); }
                            else if ends_with(key, b"h") { h = val.parse::<f32>().ok(); }
                        }
                        if let Some(name) = nm.clone() {
                            if is_face {
                                pending_name = Some(name);
                                pending_is_face = true;
                            }
                        }
                        // If this Description carries x/y/w/h AND we
                        // recently saw a Face Name, emit the face. (Mac
                        // writes the area as a child rdf:Description
                        // inside the named one.)
                        if pending_is_face {
                            if let (Some(cx), Some(cy), Some(w), Some(h)) = (cx, cy, w, h) {
                                if let Some(nm) = pending_name.take() {
                                    out.faces.push(XmpReadFace {
                                        name: nm,
                                        cx, cy, w, h,
                                        applied_w,
                                        applied_h,
                                    });
                                }
                                pending_is_face = false;
                            }
                        }
                    }
                }
                Ok(Event::End(e)) => {
                    if ends_with(e.name().as_ref(), b"Description") {
                        // End of a region without coordinates yet — keep
                        // pending_name for the next nested Description
                        // (the Area one). When the OUTER named one
                        // closes without us having pushed, we drop.
                    }
                }
                Ok(Event::Eof) => break,
                Err(_) => break,
                _ => {}
            }
            buf3.clear();
        }
    }

    // Second pass for the element-form rating/label (the first pass
    // only catches the attribute form). Cheap because the doc is small.
    //
    // v1.5.232 — Same pass also captures the date tags and the
    // RetinaTag-private vault flags. They all share the
    // "<*:Name>text</*:Name>" shape so they fit the current
    // current_tag machinery; we just widen the predicate.
    //
    // Date priority (first non-empty wins): photoshop:DateCreated →
    // exif:DateTimeOriginal → xmp:CreateDate. We track them in a
    // little local map so the priority order is independent of the
    // order tags appear in the XMP.
    let mut date_candidates: std::collections::HashMap<&'static str, String> =
        std::collections::HashMap::new();
    {
        let mut reader2 = Reader::from_str(xml);
        reader2.config_mut().trim_text(true);
        let mut current_tag: Option<Vec<u8>> = None;
        let mut buf2 = Vec::new();
        loop {
            match reader2.read_event_into(&mut buf2) {
                Ok(Event::Start(e)) => {
                    let name = e.name().as_ref().to_vec();
                    let known = ends_with(&name, b"Rating")
                        || ends_with(&name, b"Label")
                        || ends_with(&name, b"DateCreated")
                        || ends_with(&name, b"DateTimeOriginal")
                        || ends_with(&name, b"CreateDate")
                        || ends_with(&name, b"Private")
                        || ends_with(&name, b"VaultOid");
                    current_tag = if known { Some(name) } else { None };
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
                        } else if ends_with(tag, b"DateCreated") {
                            if !s.is_empty() {
                                date_candidates.entry("photoshop").or_insert(s);
                            }
                        } else if ends_with(tag, b"DateTimeOriginal") {
                            if !s.is_empty() {
                                date_candidates.entry("exif").or_insert(s);
                            }
                        } else if ends_with(tag, b"CreateDate") {
                            if !s.is_empty() {
                                date_candidates.entry("xmp").or_insert(s);
                            }
                        } else if ends_with(tag, b"Private") && out.private.is_none() {
                            // Accept "true"/"false" (case-insensitive) and "1"/"0".
                            let v = s.to_ascii_lowercase();
                            out.private = Some(v == "true" || v == "1");
                        } else if ends_with(tag, b"VaultOid") && out.vault_oid.is_none() {
                            if !s.is_empty() {
                                out.vault_oid = Some(s);
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

    // Resolve the date by priority: photoshop > exif > xmp.
    if out.date_taken.is_none() {
        for key in ["photoshop", "exif", "xmp"] {
            if let Some(v) = date_candidates.remove(key) {
                out.date_taken = Some(v);
                break;
            }
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
                // This particular sidecar (5205.xmp, resolved from 5205.MP4)
                // has all three keyword bags present but EMPTY, with only
                // dc:description populated — see the
                // `parses_sidecar_with_empty_keyword_bags` inline test for
                // the exact on-disk shape. The old assertion demanded
                // keywords that simply aren't in the file, so it always
                // failed even though the parser was correct. What we
                // actually want to verify against a real file is that the
                // parser extracts *some* real content rather than silently
                // returning an empty struct.
                assert!(
                    r.description.is_some() || !r.keywords.is_empty(),
                    "Expected real sidecar to yield a description or keywords"
                );
            }
            Ok(None) => {
                println!("No sidecar at {} (skipping)", photo);
            }
            Err(e) => panic!("Sidecar parse error: {}", e),
        }
    }

    #[test]
    fn parses_mwg_face_regions() {
        // Real fixture taken from a Mac-written sidecar
        // (D:\Fotograflar\14-18.11.2014 Paris\IMG_0025.xmp): two faces,
        // both with normalised area and AppliedToDimensions. Windows
        // imports these into face_regions so the People sidebar surfaces
        // Mac-named photos as proper faces (with bounding boxes), not
        // just keyword tags.
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/" x:xmptk="RetinaTag 1.0">
  <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
    <rdf:Description rdf:about="IMG_0025.jpg"
      xmlns:dc="http://purl.org/dc/elements/1.1/"
      xmlns:mwg-rs="http://www.metadataworkinggroup.com/schemas/regions/"
      xmlns:stArea="http://ns.adobe.com/xmp/sType/Area#"
      xmlns:stDim="http://ns.adobe.com/xmp/sType/Dimensions#">
      <mwg-rs:Regions>
        <rdf:Description>
          <mwg-rs:AppliedToDimensions>
            <rdf:Description stDim:w="640" stDim:h="480" stDim:unit="pixel"/>
          </mwg-rs:AppliedToDimensions>
          <mwg-rs:RegionList>
            <rdf:Bag>
              <rdf:li>
                <rdf:Description mwg-rs:Name="Buğra" mwg-rs:Type="Face">
                  <mwg-rs:Area>
                    <rdf:Description
                      stArea:x="0.535156"
                      stArea:y="0.288542"
                      stArea:w="0.295312"
                      stArea:h="0.539583"
                      stArea:unit="normalized"/>
                  </mwg-rs:Area>
                </rdf:Description>
              </rdf:li>
              <rdf:li>
                <rdf:Description mwg-rs:Name="Lara" mwg-rs:Type="Face">
                  <mwg-rs:Area>
                    <rdf:Description
                      stArea:x="0.771094"
                      stArea:y="0.375000"
                      stArea:w="0.235938"
                      stArea:h="0.441667"
                      stArea:unit="normalized"/>
                  </mwg-rs:Area>
                </rdf:Description>
              </rdf:li>
            </rdf:Bag>
          </mwg-rs:RegionList>
        </rdf:Description>
      </mwg-rs:Regions>
    </rdf:Description>
  </rdf:RDF>
</x:xmpmeta>"#;
        let r = parse_xmp_xml(xml).unwrap();
        assert_eq!(r.faces.len(), 2, "expected 2 faces; got {:?}", r.faces);
        let buğra = r.faces.iter().find(|f| f.name == "Buğra").expect("Buğra missing");
        assert!((buğra.cx - 0.535156).abs() < 1e-4);
        assert!((buğra.w - 0.295312).abs() < 1e-4);
        assert_eq!(buğra.applied_w, 640);
        assert_eq!(buğra.applied_h, 480);
        let lara = r.faces.iter().find(|f| f.name == "Lara").expect("Lara missing");
        assert!((lara.cy - 0.375).abs() < 1e-4);
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

    /// Regression test for the real on-disk sidecar
    /// `D:\Fotograflar\2024-10-09\5205.xmp` (resolved from the `5205.MP4`
    /// video via the Lightroom stem-name convention). RetinaTag wrote it
    /// with all three keyword bags present but EMPTY, and only
    /// `dc:description` populated. `parses_real_user_sidecar` used to assert
    /// keywords were present and failed on this file — but the parser was
    /// right: there are no keywords to find. This captures the exact shape
    /// the file exposes, with no file I/O, so it also runs on CI.
    #[test]
    fn parses_sidecar_with_empty_keyword_bags() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/" x:xmptk="RetinaTag 1.0">
  <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
    <rdf:Description rdf:about="5205.MP4"
      xmlns:dc="http://purl.org/dc/elements/1.1/"
      xmlns:xmp="http://ns.adobe.com/xap/1.0/"
      xmlns:lr="http://ns.adobe.com/lightroom/1.0/"
      xmlns:Iptc4xmpCore="http://iptc.org/std/Iptc4xmpCore/1.0/xmlns/">
      <dc:subject>
        <rdf:Bag>
        </rdf:Bag>
      </dc:subject>
      <lr:hierarchicalSubject>
        <rdf:Bag>
        </rdf:Bag>
      </lr:hierarchicalSubject>
      <Iptc4xmpCore:Keywords>
        <rdf:Bag>
        </rdf:Bag>
      </Iptc4xmpCore:Keywords>
      <dc:description>
        <rdf:Alt>
          <rdf:li xml:lang="x-default">A man and a woman are playing electric guitars together in a cheerful kitchen setting, smiling and enjoying themselves.</rdf:li>
        </rdf:Alt>
      </dc:description>
    </rdf:Description>
  </rdf:RDF>
</x:xmpmeta>"#;
        let r = parse_xmp_xml(xml).unwrap();
        assert!(
            r.keywords.is_empty(),
            "empty keyword bags must yield zero keywords, got {:?}",
            r.keywords
        );
        assert_eq!(
            r.description.as_deref(),
            Some("A man and a woman are playing electric guitars together in a cheerful kitchen setting, smiling and enjoying themselves.")
        );
    }
}
