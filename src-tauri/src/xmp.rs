use anyhow::{Context, Result};
use std::path::Path;

/// Write tags as an XMP sidecar file next to the original photo.
/// This creates a `.xmp` file (e.g., `photo.jpg` → `photo.jpg.xmp`)
/// that can be read by Lightroom, Capture One, Bridge, etc.
pub fn write_xmp_sidecar(photo_path: &str, tags: &[String]) -> Result<String> {
    let xmp_path = format!("{}.xmp", photo_path);

    let tag_items: String = tags
        .iter()
        .map(|t| format!("          <rdf:li>{}</rdf:li>", escape_xml(t)))
        .collect::<Vec<_>>()
        .join("\n");

    let filename = Path::new(photo_path)
        .file_name()
        .unwrap_or_default()
        .to_string_lossy();

    let xmp_content = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/" x:xmptk="RetinaTag 1.0">
  <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
    <rdf:Description rdf:about="{filename}"
      xmlns:dc="http://purl.org/dc/elements/1.1/"
      xmlns:lr="http://ns.adobe.com/lightroom/1.0/"
      xmlns:Iptc4xmpCore="http://iptc.org/std/Iptc4xmpCore/1.0/xmlns/"
      xmlns:photoshop="http://ns.adobe.com/photoshop/1.0/">

      <!-- Dublin Core Subject (standard tags) -->
      <dc:subject>
        <rdf:Bag>
{tag_items}
        </rdf:Bag>
      </dc:subject>

      <!-- Lightroom Hierarchical Subject -->
      <lr:hierarchicalSubject>
        <rdf:Bag>
{tag_items}
        </rdf:Bag>
      </lr:hierarchicalSubject>

      <!-- IPTC Keywords -->
      <Iptc4xmpCore:Keywords>
        <rdf:Bag>
{tag_items}
        </rdf:Bag>
      </Iptc4xmpCore:Keywords>

      <!-- Description -->
      <dc:description>
        <rdf:Alt>
          <rdf:li xml:lang="x-default">Tagged by RetinaTag AI</rdf:li>
        </rdf:Alt>
      </dc:description>
    </rdf:Description>
  </rdf:RDF>
</x:xmpmeta>"#
    );

    std::fs::write(&xmp_path, xmp_content).context("Failed to write XMP sidecar")?;
    Ok(xmp_path)
}

/// Write XMP sidecars for all tagged photos in batch
pub fn write_xmp_batch(
    photos: &[(String, Vec<String>)],
) -> Vec<(String, std::result::Result<String, String>)> {
    photos
        .iter()
        .map(|(path, tags)| {
            let result = write_xmp_sidecar(path, tags)
                .map_err(|e| e.to_string());
            (path.clone(), result)
        })
        .collect()
}

fn escape_xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}
