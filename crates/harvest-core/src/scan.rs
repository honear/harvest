//! Walk a source (file or directory) into a flat list of files, each tagged
//! with its path relative to the source root so the tree can be mirrored.

use std::path::{Path, PathBuf};

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
}

/// Enumerate all regular files under `source`. If `source` is a single file,
/// the result is that one file (relative path = its file name).
pub fn scan(source: &Path) -> Result<Vec<SourceFile>> {
    let mut files = Vec::new();

    if source.is_file() {
        let size = source.metadata()?.len();
        let name = source
            .file_name()
            .ok_or_else(|| anyhow!("source {} has no file name", source.display()))?;
        files.push(SourceFile {
            abs: source.to_path_buf(),
            rel: PathBuf::from(name),
            size,
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
            let size = entry.metadata()?.len();
            files.push(SourceFile { abs, rel, size });
        }
    }

    Ok(files)
}
