//! End-to-end harvest orchestration shared by the CLI and the GUI.
//!
//! [`run_harvest`] performs the whole job — scan, filter, template destination
//! paths, resume from a journal, copy with verification, and write a manifest —
//! reporting progress through a callback so any front end can drive it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use anyhow::{bail, Context, Result};
use rayon::prelude::*;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use crate::{
    harvest_files, hash_file, journal, render_template, scan_with, to_mhl, to_sidecar, Filter,
    HarvestOptions, HashAlgo, Journal, JournalHeader, JournalRecord, ManifestEntry, RenderCtx,
    SourceFile, JOURNAL_VERSION,
};

pub const JOURNAL_NAME: &str = ".harvest-journal.jsonl";

/// Everything needed to run one harvest.
#[derive(Debug, Clone)]
pub struct HarvestConfig {
    pub source: PathBuf,
    pub dests: Vec<PathBuf>,
    pub algo: HashAlgo,
    pub verify: bool,
    pub resume: bool,
    /// Skip files already present at every destination (matched by path, size,
    /// and modification time) — incremental copy without needing a journal.
    pub skip_existing: bool,
    pub filter: Filter,
    /// Destination path template (e.g. `"{project}/{YYYY}-{MM}-{DD}/{filename}"`).
    pub dest_template: Option<String>,
    pub project: String,
    pub write_manifest: bool,
    /// Override the journal location; `None` uses `<first-dest>/.harvest-journal.jsonl`.
    pub journal_path: Option<PathBuf>,
    /// Override the manifest location; `None` uses `<first-dest>/harvest-manifest.{ext}`.
    pub manifest_path: Option<PathBuf>,
}

/// Progress notifications emitted during a run.
#[derive(Debug, Clone)]
pub enum HarvestEvent {
    /// Emitted once after scanning/filtering/partitioning, before copying.
    Planned {
        total_scanned: usize,
        kept: usize,
        to_copy: usize,
        skipped: usize,
        copy_bytes: u64,
    },
    /// A file finished copying (and verifying, if enabled).
    FileDone {
        rel: String,
        dest: String,
        bytes: u64,
        done_files: usize,
        done_bytes: u64,
        ok: bool,
    },
}

/// Summary returned when a run finishes.
#[derive(Debug, Clone, Default)]
pub struct HarvestOutcome {
    pub copied: usize,
    pub skipped: usize,
    /// Files that couldn't be read during the source scan (permission denied,
    /// locked, unreadable media) and were left out of the transfer.
    pub unreadable: usize,
    pub copied_bytes: u64,
    pub verify_failures: Vec<String>,
    pub errors: Vec<String>,
    pub manifest_path: Option<PathBuf>,
    pub journal_path: PathBuf,
    /// True if the run was cancelled before all files were processed.
    pub cancelled: bool,
}

impl HarvestOutcome {
    pub fn success(&self) -> bool {
        self.errors.is_empty() && self.verify_failures.is_empty() && !self.cancelled
    }
}

/// Render a path with forward slashes for portable, stable keys.
pub fn forward_slash(rel: &Path) -> String {
    rel.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn date_parts(ns: i128) -> (i32, u8, u8) {
    match OffsetDateTime::from_unix_timestamp_nanos(ns) {
        Ok(dt) => (dt.year(), u8::from(dt.month()), dt.day()),
        Err(_) => (1970, 1, 1),
    }
}

fn now_iso() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "unknown".into())
}

/// True if `target_rel` already exists at every destination root with a
/// matching size and (within ~2s) modification time.
fn present_at_all(dests: &[PathBuf], target_rel: &Path, size: u64, mtime_ns: i128) -> bool {
    dests.iter().all(|root| {
        match std::fs::metadata(root.join(target_rel)) {
            Ok(m) if m.is_file() && m.len() == size => {
                (crate::scan::mtime_ns(&m) - mtime_ns).abs() <= 2_000_000_000
            }
            _ => false,
        }
    })
}

fn default_journal_path(dests: &[PathBuf]) -> PathBuf {
    let root = dests.first().cloned().unwrap_or_else(|| PathBuf::from("."));
    root.join(JOURNAL_NAME)
}

/// Scan the source, apply filters, and render templated destination paths.
/// Shared by [`run_harvest`] and [`plan`].
fn scan_filter_template(cfg: &HarvestConfig) -> Result<(Vec<SourceFile>, usize)> {
    static NEVER: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    let (mut all_files, unreadable) =
        scan_with(&cfg.source, &NEVER, &mut |_, _| {}).context("scanning source")?;
    if !cfg.filter.is_empty() {
        all_files.retain(|f| cfg.filter.accepts(f));
    }
    if let Some(tmpl) = &cfg.dest_template {
        let (jy, jm, jd) = date_parts(OffsetDateTime::now_utc().unix_timestamp_nanos());
        for f in &mut all_files {
            let (fy, fm, fd) = date_parts(f.mtime_ns);
            let dr = {
                let ctx = RenderCtx {
                    rel: &f.rel,
                    project: &cfg.project,
                    job_year: jy,
                    job_month: jm,
                    job_day: jd,
                    file_year: fy,
                    file_month: fm,
                    file_day: fd,
                };
                render_template(tmpl, &ctx)
            };
            f.dest_rel = Some(dr);
        }
    }
    Ok((all_files, unreadable as usize))
}

/// A read-only pre-flight summary: what a harvest *would* do, without copying.
#[derive(Debug, Clone, Default)]
pub struct HarvestPlan {
    pub total: usize,
    /// Files not present at any destination.
    pub new: usize,
    /// Files already present at every destination (matching size + mtime).
    pub present: usize,
    /// Files present at a destination but with different size/mtime (would overwrite).
    pub conflict: usize,
    /// Bytes that would actually be copied (new + conflict).
    pub copy_bytes: u64,
}

/// Compare the (filtered, templated) source against the destinations without
/// copying anything — powers the pre-flight confirmation.
pub fn plan(cfg: &HarvestConfig) -> Result<HarvestPlan> {
    if !cfg.source.exists() {
        bail!("source does not exist: {}", cfg.source.display());
    }
    let (files, _) = scan_filter_template(cfg)?;
    let mut p = HarvestPlan { total: files.len(), ..Default::default() };
    for f in &files {
        let target_rel = f.dest_rel.as_deref().unwrap_or(&f.rel);
        let mut all_match = true;
        let mut any_conflict = false;
        for root in &cfg.dests {
            match std::fs::metadata(root.join(target_rel)) {
                Ok(m) if m.is_file() => {
                    let same = m.len() == f.size
                        && (crate::scan::mtime_ns(&m) - f.mtime_ns).abs() <= 2_000_000_000;
                    if !same {
                        any_conflict = true;
                        all_match = false;
                    }
                }
                _ => all_match = false,
            }
        }
        if all_match {
            p.present += 1;
        } else if any_conflict {
            p.conflict += 1;
            p.copy_bytes += f.size;
        } else {
            p.new += 1;
            p.copy_bytes += f.size;
        }
    }
    Ok(p)
}

fn default_manifest_path(dests: &[PathBuf], ext: &str) -> PathBuf {
    let root = dests.first().cloned().unwrap_or_else(|| PathBuf::from("."));
    root.join(format!("harvest-manifest.{ext}"))
}

/// Run a complete harvest, reporting progress via `on_event`.
///
/// Returns an [`HarvestOutcome`]. Per-file I/O errors and verification failures
/// are collected into the outcome (not returned as `Err`); only setup failures
/// (missing source, unreadable scan, journal creation) return `Err`.
pub fn run_harvest(
    cfg: &HarvestConfig,
    cancel: &std::sync::atomic::AtomicBool,
    on_event: impl Fn(HarvestEvent) + Sync,
) -> Result<HarvestOutcome> {
    if !cfg.source.exists() {
        bail!("source does not exist: {}", cfg.source.display());
    }
    if cfg.dests.is_empty() {
        bail!("at least one destination is required");
    }

    let (all_files, unreadable) = scan_filter_template(cfg)?;
    let total_scanned = all_files.len();
    let kept = all_files.len();

    let dest_strings: Vec<String> = cfg.dests.iter().map(|d| d.display().to_string()).collect();
    let journal_file = cfg
        .journal_path
        .clone()
        .unwrap_or_else(|| default_journal_path(&cfg.dests));

    // Load a prior journal when resuming and it matches these settings.
    let mut done: HashMap<String, JournalRecord> = HashMap::new();
    let mut appending = false;
    if cfg.resume {
        if let Some(loaded) = journal::load(&journal_file) {
            if loaded.header.compatible_with(cfg.algo.name(), cfg.verify, &dest_strings) {
                done = loaded.done;
                appending = true;
            }
        }
    }

    // Partition into work vs. already done. A file is skipped if either the
    // journal already recorded it (resume), or it is already present at every
    // destination with a matching size + mtime (skip_existing).
    let mut to_copy: Vec<SourceFile> = Vec::new();
    let mut skipped: Vec<ManifestEntry> = Vec::new();
    let mut existing_skipped: usize = 0;
    for f in &all_files {
        let rel_fwd = forward_slash(&f.rel);
        if let Some(rec) = done.get(&rel_fwd) {
            if rec.size == f.size && rec.mtime_ns == f.mtime_ns {
                let dest = if rec.dest.is_empty() { rec.rel.clone() } else { rec.dest.clone() };
                skipped.push(ManifestEntry { rel: PathBuf::from(dest), size: rec.size, hash: rec.hash.clone() });
                continue;
            }
        }
        let target_rel = f.dest_rel.as_deref().unwrap_or(&f.rel);
        if cfg.skip_existing && present_at_all(&cfg.dests, target_rel, f.size, f.mtime_ns) {
            existing_skipped += 1;
            continue;
        }
        to_copy.push(f.clone());
    }

    let total_skipped = skipped.len() + existing_skipped;
    let copy_bytes: u64 = to_copy.iter().map(|f| f.size).sum();
    on_event(HarvestEvent::Planned {
        total_scanned,
        kept,
        to_copy: to_copy.len(),
        skipped: total_skipped,
        copy_bytes,
    });

    let opts = HarvestOptions {
        algo: cfg.algo,
        verify: cfg.verify,
        buf_size: crate::DEFAULT_BUF_SIZE,
    };

    // Open the journal (append to resume, else fresh with header).
    let journal = if appending {
        Journal::append(&journal_file)?
    } else {
        let header = JournalHeader {
            v: JOURNAL_VERSION,
            algo: cfg.algo.name().to_string(),
            verify: cfg.verify,
            dests: dest_strings.clone(),
        };
        Journal::create(&journal_file, &header)?
    };

    let start_iso = now_iso();
    let done_bytes = AtomicU64::new(0);
    let done_files = AtomicU64::new(0);
    let failures: Mutex<Vec<String>> = Mutex::new(Vec::new());

    let results = harvest_files(&to_copy, &cfg.dests, &opts, cancel, |report| {
        let db = done_bytes.fetch_add(report.bytes, Ordering::Relaxed) + report.bytes;
        let df = done_files.fetch_add(1, Ordering::Relaxed) + 1;
        if report.all_ok() {
            let _ = journal.record(&JournalRecord {
                rel: forward_slash(&report.rel),
                size: report.bytes,
                mtime_ns: report.mtime_ns,
                hash: report.source_hash.clone(),
                dest: forward_slash(&report.dest_rel),
            });
        } else {
            for d in report.dests.iter().filter(|d| !d.ok) {
                failures.lock().unwrap().push(d.path.display().to_string());
            }
        }
        on_event(HarvestEvent::FileDone {
            rel: forward_slash(&report.rel),
            dest: forward_slash(&report.dest_rel),
            bytes: report.bytes,
            done_files: df as usize,
            done_bytes: db,
            ok: report.all_ok(),
        });
    });

    let mut errors = Vec::new();
    for r in &results {
        if let Err(e) = r {
            errors.push(format!("{e:#}"));
        }
    }
    let verify_failures = failures.into_inner().unwrap();

    let mut outcome = HarvestOutcome {
        copied: results.iter().filter(|r| r.is_ok()).count(),
        skipped: total_skipped,
        unreadable,
        copied_bytes: done_bytes.load(Ordering::Relaxed),
        verify_failures,
        errors,
        manifest_path: None,
        journal_path: journal_file,
        cancelled: cancel.load(Ordering::Acquire),
    };

    // Write the manifest only when everything succeeded.
    if cfg.write_manifest && outcome.success() {
        let mut entries = skipped;
        for r in &results {
            if let Ok(report) = r {
                entries.push(ManifestEntry {
                    rel: report.dest_rel.clone(),
                    size: report.bytes,
                    hash: report.source_hash.clone(),
                });
            }
        }
        let finish_iso = now_iso();
        let tool = format!("Harvest {}", env!("CARGO_PKG_VERSION"));
        let (path, contents) = match to_mhl(&entries, cfg.algo, &tool, &start_iso, &finish_iso) {
            Some(mhl) => (
                cfg.manifest_path.clone().unwrap_or_else(|| default_manifest_path(&cfg.dests, "mhl")),
                mhl,
            ),
            None => (
                cfg.manifest_path.clone().unwrap_or_else(|| default_manifest_path(&cfg.dests, "txt")),
                to_sidecar(&entries, cfg.algo),
            ),
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        std::fs::write(&path, contents).with_context(|| format!("writing manifest {}", path.display()))?;
        outcome.manifest_path = Some(path);
    }

    Ok(outcome)
}

/// Re-verify existing destination copies against the source without copying.
/// For each (filtered, templated) file, hashes the source and each destination
/// and compares. Mismatches and missing destination files become
/// `verify_failures`; `copied` reports how many verified OK.
pub fn run_verify(
    cfg: &HarvestConfig,
    cancel: &std::sync::atomic::AtomicBool,
    on_event: impl Fn(HarvestEvent) + Sync,
) -> Result<HarvestOutcome> {
    if !cfg.source.exists() {
        bail!("source does not exist: {}", cfg.source.display());
    }
    if cfg.dests.is_empty() {
        bail!("at least one destination is required");
    }

    let (files, unreadable) = scan_filter_template(cfg)?;
    let total = files.len();
    let total_bytes: u64 = files.iter().map(|f| f.size).sum();
    on_event(HarvestEvent::Planned {
        total_scanned: total,
        kept: total,
        to_copy: total,
        skipped: 0,
        copy_bytes: total_bytes,
    });

    let buf = crate::DEFAULT_BUF_SIZE;
    let done_files = AtomicU64::new(0);
    let done_bytes = AtomicU64::new(0);
    let failures: Mutex<Vec<String>> = Mutex::new(Vec::new());

    let results: Vec<bool> = files
        .par_iter()
        .filter_map(|f| {
            if cancel.load(Ordering::Acquire) {
                return None;
            }
            let target_rel = f.dest_rel.as_deref().unwrap_or(&f.rel);
            let src_hash = hash_file(&f.abs, cfg.algo, buf).ok();
            let mut ok = true;
            for root in &cfg.dests {
                let dp = root.join(target_rel);
                match (&src_hash, hash_file(&dp, cfg.algo, buf).ok()) {
                    (Some(s), Some(d)) if *s == d => {}
                    (_, None) => {
                        failures.lock().unwrap().push(format!("missing: {}", dp.display()));
                        ok = false;
                    }
                    _ => {
                        failures.lock().unwrap().push(format!("mismatch: {}", dp.display()));
                        ok = false;
                    }
                }
            }
            let db = done_bytes.fetch_add(f.size, Ordering::Relaxed) + f.size;
            let df = done_files.fetch_add(1, Ordering::Relaxed) + 1;
            on_event(HarvestEvent::FileDone {
                rel: forward_slash(&f.rel),
                dest: forward_slash(target_rel),
                bytes: f.size,
                done_files: df as usize,
                done_bytes: db,
                ok,
            });
            Some(ok)
        })
        .collect();

    Ok(HarvestOutcome {
        copied: results.iter().filter(|x| **x).count(),
        skipped: 0,
        unreadable,
        copied_bytes: done_bytes.load(Ordering::Relaxed),
        verify_failures: failures.into_inner().unwrap(),
        errors: Vec::new(),
        manifest_path: None,
        journal_path: PathBuf::new(),
        cancelled: cancel.load(Ordering::Acquire),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn temp_dir() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("harvest_run_{}_{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn cfg(source: PathBuf, dest: PathBuf) -> HarvestConfig {
        HarvestConfig {
            source,
            dests: vec![dest],
            algo: HashAlgo::Xxh64,
            verify: true,
            resume: false,
            skip_existing: true,
            filter: Filter::default(),
            dest_template: None,
            project: String::new(),
            write_manifest: false,
            journal_path: None,
            manifest_path: None,
        }
    }

    fn no_cancel() -> std::sync::atomic::AtomicBool {
        std::sync::atomic::AtomicBool::new(false)
    }

    #[test]
    fn rerun_skips_files_already_present() {
        let root = temp_dir();
        let src = root.join("src");
        std::fs::create_dir_all(src.join("clips")).unwrap();
        std::fs::write(src.join("clips/a.bin"), vec![1u8; 5000]).unwrap();
        std::fs::write(src.join("b.txt"), b"hello").unwrap();
        let dest = root.join("dest");

        let c = cfg(src, dest);
        let first = run_harvest(&c, &no_cancel(), |_| {}).unwrap();
        assert_eq!(first.copied, 2, "first run copies both files");
        assert_eq!(first.skipped, 0);
        assert!(first.success());

        // A plan now sees both as present.
        let pl = plan(&c).unwrap();
        assert_eq!(pl.present, 2);
        assert_eq!(pl.new, 0);
        assert_eq!(pl.copy_bytes, 0);

        // Second run: mtime was preserved, so both files are recognized.
        let second = run_harvest(&c, &no_cancel(), |_| {}).unwrap();
        assert_eq!(second.copied, 0, "nothing should be re-copied");
        assert_eq!(second.skipped, 2, "both files recognized as already present");
    }

    #[test]
    fn skip_existing_off_recopies() {
        let root = temp_dir();
        let src = root.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("a.bin"), vec![7u8; 1234]).unwrap();
        let dest = root.join("dest");

        let mut c = cfg(src, dest);
        run_harvest(&c, &no_cancel(), |_| {}).unwrap();
        c.skip_existing = false;
        let again = run_harvest(&c, &no_cancel(), |_| {}).unwrap();
        assert_eq!(again.copied, 1, "with skip_existing off, the file is copied again");
        assert_eq!(again.skipped, 0);
    }

    #[test]
    fn verify_detects_match_and_corruption() {
        let root = temp_dir();
        let src = root.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("a.bin"), vec![3u8; 400]).unwrap();
        std::fs::write(src.join("b.bin"), vec![4u8; 400]).unwrap();
        let dest = root.join("dest");
        let c = cfg(src, dest.clone());
        run_harvest(&c, &no_cancel(), |_| {}).unwrap();

        // Clean copy verifies OK.
        let v1 = run_verify(&c, &no_cancel(), |_| {}).unwrap();
        assert_eq!(v1.copied, 2);
        assert!(v1.verify_failures.is_empty());

        // Corrupt one destination file → caught.
        std::fs::write(dest.join("b.bin"), vec![9u8; 400]).unwrap();
        let v2 = run_verify(&c, &no_cancel(), |_| {}).unwrap();
        assert_eq!(v2.copied, 1);
        assert_eq!(v2.verify_failures.len(), 1);
        assert!(v2.verify_failures[0].contains("mismatch"));
    }

    #[test]
    fn plan_classifies_new_present_conflict() {
        let root = temp_dir();
        let src = root.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("a.bin"), vec![1u8; 100]).unwrap();
        std::fs::write(src.join("b.bin"), vec![2u8; 200]).unwrap();
        let dest = root.join("dest");
        let c = cfg(src.clone(), dest.clone());

        // Nothing copied yet → all new.
        let p0 = plan(&c).unwrap();
        assert_eq!(p0.new, 2);
        assert_eq!(p0.present, 0);

        run_harvest(&c, &no_cancel(), |_| {}).unwrap();
        // Now make b differ at the destination → conflict; a stays present.
        std::fs::write(dest.join("b.bin"), vec![9u8; 999]).unwrap();
        let p1 = plan(&c).unwrap();
        assert_eq!(p1.present, 1);
        assert_eq!(p1.conflict, 1);
        assert_eq!(p1.new, 0);
    }
}
