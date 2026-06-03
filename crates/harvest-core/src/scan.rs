//! Walk a source (file or directory) into a flat list of files, each tagged
//! with its path relative to the source root so the tree can be mirrored.

use std::fs::Metadata;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::{anyhow, Context, Result};
use walkdir::WalkDir;

/// A file discovered under the source root.
#[derive(Debug, Clone)]
pub struct SourceFile {
    /// Absolute path on disk.
    pub abs: PathBuf,
    /// Path relative to the source root (what gets mirrored into destinations).
    pub rel: PathBuf,
    pub size: u64,
    /// Modification time as nanoseconds relative to the Unix epoch (signed, so
    /// pre-1970 timestamps round-trip). 0 if unavailable.
    pub mtime_ns: i128,
    /// Where to write this file relative to each destination root. `None`
    /// mirrors the source tree (`rel`); `Some` is set by template rendering.
    pub dest_rel: Option<PathBuf>,
}

/// Modification time of a file's metadata as nanoseconds since the Unix epoch.
pub fn mtime_ns(meta: &Metadata) -> i128 {
    match meta.modified() {
        Ok(t) => match t.duration_since(UNIX_EPOCH) {
            Ok(d) => d.as_nanos() as i128,
            Err(e) => -(e.duration().as_nanos() as i128),
        },
        Err(_) => 0,
    }
}

/// Enumerate all regular files under `source`. If `source` is a single file,
/// the result is that one file (relative path = its file name).
pub fn scan(source: &Path) -> Result<Vec<SourceFile>> {
    let mut files = Vec::new();

    if source.is_file() {
        let meta = source.metadata()?;
        let name = source
            .file_name()
            .ok_or_else(|| anyhow!("source {} has no file name", source.display()))?;
        files.push(SourceFile {
            abs: source.to_path_buf(),
            rel: PathBuf::from(name),
            size: meta.len(),
            mtime_ns: mtime_ns(&meta),
            dest_rel: None,
        });
        return Ok(files);
    }

    for entry in WalkDir::new(source) {
        let entry = entry.with_context(|| format!("walking {}", source.display()))?;
        if entry.file_type().is_file() {
            let abs = entry.path().to_path_buf();
            let rel = abs
                .strip_prefix(source)
                .with_context(|| format!("relativizing {}", abs.display()))?
                .to_path_buf();
            let meta = entry.metadata()?;
            files.push(SourceFile {
                abs,
                rel,
                size: meta.len(),
                mtime_ns: mtime_ns(&meta),
                dest_rel: None,
            });
        }
    }

    Ok(files)
}
