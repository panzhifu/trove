//! XMP sidecar export: write an asset's library metadata next to its media
//! file so other tools (Lightroom, Bridge, IMatch, …) can read it.
//!
//! The sidecar is a standards-shaped XMP packet (ISO 16684-1): the Dublin
//! Core fields carry title / description / tags, `xmp:Rating` carries the
//! star rating. Sidecars are the safe transport — the original file is
//! never rewritten, which matters for linked assets (the file belongs to
//! the user) and for formats without an XMP segment (PNG, WebP, …).
//!
//! Regenerating a sidecar overwrites the previous one wholesale. Trove does
//! not merge fields written by other tools; the sidecar reflects what the
//! library currently knows.

use std::path::{Path, PathBuf};

use crate::error::Result;

/// The library metadata exported into one XMP sidecar.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct XmpData {
    pub title: Option<String>,
    pub description: Option<String>,
    /// Tag names, in display order.
    pub tags: Vec<String>,
    /// 0–5 star rating (`xmp:Rating`); `None` omits the field.
    pub rating: Option<u8>,
}

/// The packet prolog/epilog. The `xpacket` padding wrapper is what makes
/// tools treat the file as an XMP packet rather than loose XML.
const PACKET_BEGIN: &str = concat!(
    "<?xpacket begin=\"\u{feff}\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\n",
    "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\">\n",
    " <rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\n"
);
const PACKET_END: &str = " </rdf:RDF>\n</x:xmpmeta>\n<?xpacket end=\"w\"?>\n";

/// Build the XMP packet for `data`. Fields the library does not have are
/// simply absent; an asset with no metadata at all still produces a valid
/// (empty-description) packet.
pub fn build_xmp(data: &XmpData) -> String {
    let mut out = String::from(PACKET_BEGIN);
    out.push_str(
        "  <rdf:Description rdf:about=\"\"\n\
         \x20   xmlns:dc=\"http://purl.org/dc/elements/1.1/\"\n\
         \x20   xmlns:xmp=\"http://ns.adobe.com/xap/1.0/\">\n",
    );
    if let Some(title) = non_empty(&data.title) {
        out.push_str(&alt_property("dc:title", &xml_escape(title)));
    }
    if let Some(description) = non_empty(&data.description) {
        out.push_str(&alt_property("dc:description", &xml_escape(description)));
    }
    if !data.tags.is_empty() {
        out.push_str("   <dc:subject>\n    <rdf:Bag>\n");
        for tag in &data.tags {
            out.push_str(&format!("     <rdf:li>{}</rdf:li>\n", xml_escape(tag)));
        }
        out.push_str("    </rdf:Bag>\n   </dc:subject>\n");
    }
    if let Some(rating) = data.rating {
        let clamped = rating.min(5);
        out.push_str(&format!("   <xmp:Rating>{clamped}</xmp:Rating>\n"));
    }
    out.push_str("  </rdf:Description>\n");
    out.push_str(PACKET_END);
    out
}

/// The sidecar file for `target`: same directory, same stem, `.xmp`.
pub fn sidecar_path(target: &Path) -> PathBuf {
    target.with_extension("xmp")
}

/// Write the sidecar for `target` atomically (temp file in the same
/// directory, then rename) and return its path. An existing sidecar is
/// replaced — see the module docs on merging.
pub fn write_sidecar(target: &Path, data: &XmpData) -> Result<PathBuf> {
    let out = sidecar_path(target);
    let tmp = out.with_extension(format!("xmp.tmp-{}", uuid::Uuid::new_v4()));
    std::fs::write(&tmp, build_xmp(data))?;
    match std::fs::rename(&tmp, &out) {
        Ok(()) => Ok(out),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e.into())
        }
    }
}

fn non_empty(s: &Option<String>) -> Option<&str> {
    s.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

/// A language-alternative property (`dc:title` / `dc:description` shape).
fn alt_property(name: &str, value: &str) -> String {
    format!(
        "   <{name}>\n    <rdf:Alt>\n     <rdf:li xml:lang=\"x-default\">{value}</rdf:li>\n    </rdf:Alt>\n   </{name}>\n"
    )
}

/// Escape text for XML element content. Applied to every user-controlled
/// string (titles come from file names, tags from user input).
fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\'' => out.push_str("&apos;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(ch),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packet_carries_every_field() {
        let xmp = build_xmp(&XmpData {
            title: Some("Sunset".into()),
            description: Some("Beach, 2024".into()),
            tags: vec!["sea".into(), "golden hour".into()],
            rating: Some(4),
        });
        assert!(xmp.starts_with("<?xpacket begin="));
        assert!(xmp.contains("<dc:title>"));
        assert!(xmp.contains("<rdf:li xml:lang=\"x-default\">Sunset</rdf:li>"));
        assert!(xmp.contains("<dc:description>"));
        assert!(xmp.contains("<rdf:li>sea</rdf:li>"));
        assert!(xmp.contains("<rdf:li>golden hour</rdf:li>"));
        assert!(xmp.contains("<xmp:Rating>4</xmp:Rating>"));
        assert!(xmp.trim_end().ends_with("<?xpacket end=\"w\"?>"));
    }

    #[test]
    fn empty_metadata_still_produces_a_valid_empty_packet() {
        let xmp = build_xmp(&XmpData::default());
        assert!(!xmp.contains("dc:title"));
        assert!(!xmp.contains("dc:subject"));
        assert!(!xmp.contains("xmp:Rating"));
        assert!(xmp.contains("rdf:Description"));
    }

    #[test]
    fn user_text_is_escaped_and_rating_is_clamped() {
        let xmp = build_xmp(&XmpData {
            title: Some("a<b>&\"c\"".into()),
            tags: vec!["tag&1".into()],
            rating: Some(9),
            ..Default::default()
        });
        assert!(xmp.contains("a&lt;b&gt;&amp;&quot;c&quot;"));
        assert!(xmp.contains("<rdf:li>tag&amp;1</rdf:li>"));
        assert!(xmp.contains("<xmp:Rating>5</xmp:Rating>"));
        assert!(!xmp.contains("<xmp:Rating>9"));
    }

    #[test]
    fn sidecar_writes_next_to_the_target_atomically() {
        let dir = std::env::temp_dir().join(format!("trove-xmp-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("0123abcd.png");
        std::fs::write(&target, b"png").unwrap();

        let out = write_sidecar(
            &target,
            &XmpData {
                title: Some("t".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(out, dir.join("0123abcd.xmp"));
        let body = std::fs::read_to_string(&out).unwrap();
        assert!(body.contains("dc:title"));

        // No temp files left behind.
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp-"))
            .collect();
        assert!(leftovers.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }
}
