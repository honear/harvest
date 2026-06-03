//! Proof-of-transfer manifests.
//!
//! Two formats:
//! * **MHL** (Media Hash List, mediahashlist.org) — the XML standard understood
//!   by other media tools. Only emitted for algorithms MHL defines an element
//!   for (xxHash64, MD5).
//! * **Sidecar** — a simple `<hash> *<relative-path>` text file (the familiar
//!   `md5sum`/`xxhsum` layout), always available regardless of algorithm.

use std::path::PathBuf;

use crate::hash::HashAlgo;

/// One file's verified result, with its path relative to the source root.
#[derive(Debug, Clone)]
pub struct ManifestEntry {
    pub rel: PathBuf,
    pub size: u64,
    pub hash: String,
}

/// Render a relative path with forward slashes (manifests are portable across
/// platforms; backslashes are not).
fn rel_str(entry: &ManifestEntry) -> String {
    entry
        .rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(ch),
        }
    }
    out
}

/// Build a classic MHL (v1.1) document, or `None` if the algorithm has no MHL
/// element (e.g. xxHash3 — use [`to_sidecar`] for those).
///
/// `start`/`finish` are ISO-8601 timestamps supplied by the caller.
pub fn to_mhl(
    entries: &[ManifestEntry],
    algo: HashAlgo,
    tool: &str,
    start: &str,
    finish: &str,
) -> Option<String> {
    let element = algo.mhl_element()?;
    let mut s = String::new();
    s.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    s.push_str("<hashlist version=\"1.1\">\n");
    s.push_str("    <creatorinfo>\n");
    s.push_str(&format!("        <tool>{}</tool>\n", xml_escape(tool)));
    s.push_str(&format!("        <startdate>{}</startdate>\n", xml_escape(start)));
    s.push_str(&format!("        <finishdate>{}</finishdate>\n", xml_escape(finish)));
    s.push_str("    </creatorinfo>\n");
    for e in entries {
        s.push_str("    <hash>\n");
        s.push_str(&format!("        <file>{}</file>\n", xml_escape(&rel_str(e))));
        s.push_str(&format!("        <size>{}</size>\n", e.size));
        s.push_str(&format!("        <{element}>{}</{element}>\n", e.hash));
        s.push_str("    </hash>\n");
    }
    s.push_str("</hashlist>\n");
    Some(s)
}

/// Build a simple sidecar manifest: one `<hash> *<relative-path>` line per file.
/// The first line is a `;`-prefixed comment recording the algorithm.
pub fn to_sidecar(entries: &[ManifestEntry], algo: HashAlgo) -> String {
    let mut s = format!("; harvest manifest ({})\n", algo.name());
    for e in entries {
        s.push_str(&format!("{} *{}\n", e.hash, rel_str(e)));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries() -> Vec<ManifestEntry> {
        vec![
            ManifestEntry { rel: PathBuf::from("clips/A & B/take 1.mov"), size: 1024, hash: "deadbeef".into() },
            ManifestEntry { rel: PathBuf::from("root.dat"), size: 7, hash: "cafef00d".into() },
        ]
    }

    #[test]
    fn mhl_has_expected_elements_and_escapes_paths() {
        let mhl = to_mhl(&entries(), HashAlgo::Xxh64, "Harvest 0.1.0", "S", "F").unwrap();
        assert!(mhl.contains("<hashlist version=\"1.1\">"));
        assert!(mhl.contains("<xxhash64be>deadbeef</xxhash64be>"));
        assert!(mhl.contains("<size>1024</size>"));
        // Path uses forward slashes and XML-escapes the ampersand.
        assert!(mhl.contains("<file>clips/A &amp; B/take 1.mov</file>"));
        // MD5 uses a different element name.
        let md5 = to_mhl(&entries(), HashAlgo::Md5, "t", "s", "f").unwrap();
        assert!(md5.contains("<md5>deadbeef</md5>"));
    }

    #[test]
    fn xxh3_has_no_mhl_but_has_sidecar() {
        assert!(to_mhl(&entries(), HashAlgo::Xxh3, "t", "s", "f").is_none());
        let side = to_sidecar(&entries(), HashAlgo::Xxh3);
        assert!(side.contains("deadbeef *clips/A & B/take 1.mov"));
        assert!(side.contains("cafef00d *root.dat"));
    }
}
