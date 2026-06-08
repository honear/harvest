//! Walk a source (file or directory) into a flat list of files, each tagged
//! with its path relative to the source root so the tree can be mirrored.

use std::fs::Metadata;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
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
    static NEVER: AtomicBool = AtomicBool::new(false);
    Ok(scan_with(source, &NEVER, &mut |_, _| {})?.0)
}

/// Like [`scan`], but reports running progress (files found, bytes so far) via
/// `progress` (called roughly every 512 files and once at the end) and returns
/// the number of entries skipped because they couldn't be read. If `cancel`
/// flips to true mid-walk it bails with an error. Used by the Sow visualizer
/// (live scan count + cancel) and the copy path (skipped-files reporting).
pub fn scan_with(
    source: &Path,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<(Vec<SourceFile>, u64)> {
    let mut files = Vec::new();
    let mut skipped = 0u64;
    let mut bytes = 0u64;

    if source.is_file() {
        let meta = source.metadata()?;
        let name = source
            .file_name()
            .ok_or_else(|| anyhow!("source {} has no file name", source.display()))?;
        let size = meta.len();
        files.push(SourceFile {
            abs: source.to_path_buf(),
            rel: PathBuf::from(name),
            size,
            mtime_ns: mtime_ns(&meta),
            dest_rel: None,
        });
        progress(1, size);
        return Ok((files, skipped));
    }

    for entry in WalkDir::new(source) {
        if cancel.load(Ordering::Relaxed) {
            return Err(anyhow!("scan cancelled"));
        }
        let entry = match entry {
            Ok(e) => e,
            // A failure at the root (depth 0) means the source itself is
            // unreadable — e.g. an empty card-reader slot like A:\ with no
            // media — so surface a clear error instead of "walking A:\".
            Err(err) if err.depth() == 0 => {
                let why = err
                    .io_error()
                    .map(|e| e.to_string())
                    .unwrap_or_else(|| err.to_string());
                return Err(anyhow!("cannot read {}: {}", source.display(), why));
            }
            // Deeper failures (permission denied, locked files, System Volume
            // Information, …) are skipped so one bad entry can't fail the scan.
            Err(_) => {
                skipped += 1;
                continue;
            }
        };
        if entry.file_type().is_file() {
            let abs = entry.path().to_path_buf();
            let rel = abs
                .strip_prefix(source)
                .with_context(|| format!("relativizing {}", abs.display()))?
                .to_path_buf();
            let Ok(meta) = entry.metadata() else {
                skipped += 1;
                continue;
            };
            bytes += meta.len();
            files.push(SourceFile {
                abs,
                rel,
                size: meta.len(),
                mtime_ns: mtime_ns(&meta),
                dest_rel: None,
            });
            if files.len() % 512 == 0 {
                progress(files.len() as u64, bytes);
            }
        }
    }

    progress(files.len() as u64, bytes);
    Ok((files, skipped))
}
