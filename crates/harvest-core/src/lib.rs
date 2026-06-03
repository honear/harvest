//! Harvest core engine: verified, parallel file ingest.
//!
//! This crate is deliberately UI-agnostic so it can be driven by the CLI now
//! and the Tauri GUI later. It does the genuinely hard part — proving that
//! bytes copied off a card land intact on every destination drive.

pub mod copy;
pub mod filter;
pub mod hash;
pub mod journal;
pub mod manifest;
pub mod scan;

use std::path::{Path, PathBuf};

use anyhow::Result;
use rayon::prelude::*;

pub use copy::{copy_file_verified, DestReport, FileReport};
pub use filter::Filter;
pub use hash::{hash_file, HashAlgo, Hasher};
pub use journal::{Journal, JournalHeader, JournalRecord, JOURNAL_VERSION};
pub use manifest::{to_mhl, to_sidecar, ManifestEntry};
pub use scan::{mtime_ns, scan, SourceFile};

/// 8 MiB streaming buffer — favors throughput on large media files.
pub const DEFAULT_BUF_SIZE: usize = 8 * 1024 * 1024;

/// Options for a harvest run.
#[derive(Debug, Clone)]
pub struct HarvestOptions {
    pub algo: HashAlgo,
    pub verify: bool,
    pub buf_size: usize,
}

impl Default for HarvestOptions {
    fn default() -> Self {
        Self {
            algo: HashAlgo::Xxh64,
            verify: true,
            buf_size: DEFAULT_BUF_SIZE,
        }
    }
}

/// Copy a pre-scanned set of files into each destination root in parallel,
/// preserving the relative tree. `on_done` fires once per completed file
/// (from worker threads, so it must be `Sync`).
pub fn harvest_files(
    files: &[SourceFile],
    dest_roots: &[PathBuf],
    opts: &HarvestOptions,
    on_done: impl Fn(&FileReport) + Sync,
) -> Vec<Result<FileReport>> {
    files
        .par_iter()
        .map(|f| {
            let dests: Vec<PathBuf> = dest_roots.iter().map(|root| root.join(&f.rel)).collect();
            let result =
                copy_file_verified(&f.abs, &dests, opts.algo, opts.verify, opts.buf_size);
            match result {
                Ok(mut report) => {
                    // Carry source identity the copy layer didn't know about.
                    report.rel = f.rel.clone();
                    report.mtime_ns = f.mtime_ns;
                    on_done(&report);
                    Ok(report)
                }
                Err(e) => Err(e),
            }
        })
        .collect()
}

/// Convenience: scan `source` then harvest every file into the destinations.
pub fn harvest_tree(
    source: &Path,
    dest_roots: &[PathBuf],
    opts: &HarvestOptions,
    on_done: impl Fn(&FileReport) + Sync,
) -> Result<Vec<Result<FileReport>>> {
    let files = scan(source)?;
    Ok(harvest_files(&files, dest_roots, opts, on_done))
}
