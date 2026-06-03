//! `harvest` — command-line front end for the Harvest ingest engine.
//!
//! Examples:
//!   harvest copy /path/to/SDCARD ./backupA ./backupB --hash xxh64
//!   harvest copy ./project ./archive --no-verify
//!   harvest copy /path/to/SDCARD ./backupA --resume   # continue an interrupted run

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Instant, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use indicatif::{ProgressBar, ProgressStyle};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use harvest_core::{
    harvest_files, journal, scan, to_mhl, to_sidecar, HarvestOptions, HashAlgo, Journal,
    JournalHeader, JournalRecord, ManifestEntry, SourceFile, JOURNAL_VERSION,
};

const TOOL: &str = concat!("Harvest ", env!("CARGO_PKG_VERSION"));
const JOURNAL_NAME: &str = ".harvest-journal.jsonl";

#[derive(Parser)]
#[command(name = "harvest", version, about = "Verified media ingest")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Copy a source file or folder to one or more destinations, verifying each.
    Copy {
        /// Source file or directory (e.g. an SD card mount point).
        source: PathBuf,
        /// One or more destination directories.
        #[arg(required = true)]
        dests: Vec<PathBuf>,
        /// Checksum algorithm: xxh64 (fast, MHL-standard, default), xxh3, or md5.
        #[arg(long, default_value = "xxh64")]
        hash: String,
        /// Skip read-back verification (faster, less safe).
        #[arg(long)]
        no_verify: bool,
        /// Resume an interrupted run, skipping files already verified.
        #[arg(long)]
        resume: bool,
        /// Journal file location (default: <first-dest>/.harvest-journal.jsonl).
        #[arg(long, value_name = "FILE")]
        journal: Option<PathBuf>,
        /// Write the manifest to this exact path instead of the default location.
        #[arg(long, value_name = "FILE")]
        manifest: Option<PathBuf>,
        /// Do not write a transfer manifest.
        #[arg(long)]
        no_manifest: bool,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Copy {
            source,
            dests,
            hash,
            no_verify,
            resume,
            journal,
            manifest,
            no_manifest,
        } => run_copy(RunArgs {
            source,
            dests,
            hash,
            verify: !no_verify,
            resume,
            journal_path: journal,
            manifest_path: manifest,
            no_manifest,
        }),
    }
}

struct RunArgs {
    source: PathBuf,
    dests: Vec<PathBuf>,
    hash: String,
    verify: bool,
    resume: bool,
    journal_path: Option<PathBuf>,
    manifest_path: Option<PathBuf>,
    no_manifest: bool,
}

fn run_copy(args: RunArgs) -> Result<()> {
    let RunArgs {
        source,
        dests,
        hash,
        verify,
        resume,
        journal_path,
        manifest_path,
        no_manifest,
    } = args;

    let algo = HashAlgo::parse(&hash)
        .with_context(|| format!("unknown hash algorithm '{hash}' (expected xxh64, xxh3, or md5)"))?;
    if !source.exists() {
        bail!("source does not exist: {}", source.display());
    }

    println!("Scanning {} ...", source.display());
    let all_files = scan(&source).context("scanning source")?;
    if all_files.is_empty() {
        println!("No files found. Nothing to do.");
        return Ok(());
    }

    let dest_strings: Vec<String> = dests.iter().map(|d| d.display().to_string()).collect();
    let journal_file = journal_path.unwrap_or_else(|| default_journal_path(&dests));

    // Load a prior journal when resuming and it matches this job's settings.
    let mut done: HashMap<String, JournalRecord> = HashMap::new();
    let mut appending = false;
    if resume {
        match journal::load(&journal_file) {
            Some(loaded) if loaded.header.compatible_with(algo.name(), verify, &dest_strings) => {
                done = loaded.done;
                appending = true;
                println!("Resuming from {} ({} files recorded).", journal_file.display(), done.len());
            }
            Some(_) => println!("Existing journal is incompatible with these settings — starting fresh."),
            None => println!("No prior journal found — starting fresh."),
        }
    }

    // Partition into work to do vs. files already verified (and unchanged).
    let mut to_copy: Vec<SourceFile> = Vec::new();
    let mut skipped: Vec<ManifestEntry> = Vec::new();
    for f in &all_files {
        let rel_fwd = forward_slash(&f.rel);
        let mt = fs::metadata(&f.abs).ok().map(|m| mtime_ns(&m)).unwrap_or(0);
        match done.get(&rel_fwd) {
            Some(rec) if rec.size == f.size && rec.mtime_ns == mt => {
                skipped.push(ManifestEntry { rel: f.rel.clone(), size: rec.size, hash: rec.hash.clone() });
            }
            _ => to_copy.push(f.clone()),
        }
    }

    let copy_bytes: u64 = to_copy.iter().map(|f| f.size).sum();
    let copy_count = to_copy.len();
    println!(
        "{} file(s) to copy ({}), {} already verified. Hash: {}. Verify: {}. Dests: {}.",
        copy_count,
        human_bytes(copy_bytes),
        skipped.len(),
        algo.name(),
        if verify { "read-back" } else { "off" },
        dests.len(),
    );

    let opts = HarvestOptions { algo, verify, buf_size: harvest_core::DEFAULT_BUF_SIZE };

    // Open the journal for writing (append to resume, else create fresh w/ header).
    let journal = if appending {
        Journal::append(&journal_file)?
    } else {
        let header = JournalHeader {
            v: JOURNAL_VERSION,
            algo: algo.name().to_string(),
            verify,
            dests: dest_strings.clone(),
        };
        Journal::create(&journal_file, &header)?
    };

    let start_iso = now_iso();
    let start = Instant::now();
    let mut results = Vec::new();

    if copy_count > 0 {
        let bar = ProgressBar::new(copy_bytes);
        bar.set_style(
            ProgressStyle::with_template(
                "{bar:40.cyan/blue} {bytes}/{total_bytes} ({bytes_per_sec}, ETA {eta}) {msg}",
            )
            .unwrap()
            .progress_chars("##-"),
        );

        let done_bytes = AtomicU64::new(0);
        let done_files = AtomicU64::new(0);
        let failures: Mutex<Vec<String>> = Mutex::new(Vec::new());

        results = harvest_files(&to_copy, &dests, &opts, |report| {
            done_bytes.fetch_add(report.bytes, Ordering::Relaxed);
            let n = done_files.fetch_add(1, Ordering::Relaxed) + 1;
            bar.set_position(done_bytes.load(Ordering::Relaxed));
            bar.set_message(format!("{n}/{copy_count}"));

            if report.all_ok() {
                // Persist progress immediately so an interruption is resumable.
                let mt = fs::metadata(&report.source).ok().map(|m| mtime_ns(&m)).unwrap_or(0);
                let _ = journal.record(&JournalRecord {
                    rel: forward_slash(&report.rel),
                    size: report.bytes,
                    mtime_ns: mt,
                    hash: report.source_hash.clone(),
                });
            } else {
                for d in report.dests.iter().filter(|d| !d.ok) {
                    failures.lock().unwrap().push(format!("VERIFY FAILED: {}", d.path.display()));
                }
            }
        });
        bar.finish_and_clear();

        // Report I/O errors and verification failures; withhold the manifest if any.
        let mut errors: Vec<String> = Vec::new();
        for r in &results {
            if let Err(e) = r {
                errors.push(format!("{e:#}"));
            }
        }
        let verify_failures = failures.into_inner().unwrap();

        let secs = start.elapsed().as_secs_f64();
        let rate = if secs > 0.0 { copy_bytes as f64 / secs } else { 0.0 };
        println!("\nDone in {:.1}s — {} copied at {}/s", secs, human_bytes(copy_bytes), human_bytes(rate as u64));

        if !errors.is_empty() || !verify_failures.is_empty() {
            for e in &errors {
                eprintln!("ERROR: {e}");
            }
            for f in &verify_failures {
                eprintln!("{f}");
            }
            eprintln!(
                "\nProgress saved to {} — re-run with --resume to continue.",
                journal_file.display()
            );
            bail!(
                "{} error(s), {} verification failure(s) — manifest not written",
                errors.len(),
                verify_failures.len()
            );
        }
    }

    println!(
        "All {} file(s) {} OK ({} copied, {} already done).",
        copy_count + skipped.len(),
        if verify { "verified" } else { "written (not verified)" },
        copy_count,
        skipped.len(),
    );

    // Build the manifest from everything proven good: skipped + freshly copied.
    if !no_manifest {
        let mut entries = skipped;
        for r in &results {
            if let Ok(report) = r {
                entries.push(ManifestEntry {
                    rel: report.rel.clone(),
                    size: report.bytes,
                    hash: report.source_hash.clone(),
                });
            }
        }
        let finish_iso = now_iso();
        let (path, contents) = match to_mhl(&entries, algo, TOOL, &start_iso, &finish_iso) {
            Some(mhl) => (manifest_path.unwrap_or_else(|| default_manifest_path(&dests, "mhl")), mhl),
            None => (
                manifest_path.unwrap_or_else(|| default_manifest_path(&dests, "txt")),
                to_sidecar(&entries, algo),
            ),
        };
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).ok();
        }
        fs::write(&path, contents).with_context(|| format!("writing manifest {}", path.display()))?;
        println!("Manifest: {}", path.display());
    }

    Ok(())
}

/// Default journal location: inside the first destination root.
fn default_journal_path(dests: &[PathBuf]) -> PathBuf {
    let root = dests.first().cloned().unwrap_or_else(|| PathBuf::from("."));
    root.join(JOURNAL_NAME)
}

/// Default manifest location: inside the first destination root.
fn default_manifest_path(dests: &[PathBuf], ext: &str) -> PathBuf {
    let root = dests.first().cloned().unwrap_or_else(|| PathBuf::from("."));
    root.join(format!("harvest-manifest.{ext}"))
}

/// Render a relative path with forward slashes for portable, stable keys.
fn forward_slash(rel: &Path) -> String {
    rel.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// Source modification time as nanoseconds relative to the Unix epoch
/// (signed, so pre-1970 timestamps still round-trip).
fn mtime_ns(meta: &fs::Metadata) -> i128 {
    match meta.modified() {
        Ok(t) => match t.duration_since(UNIX_EPOCH) {
            Ok(d) => d.as_nanos() as i128,
            Err(e) => -(e.duration().as_nanos() as i128),
        },
        Err(_) => 0,
    }
}

/// Current time as an RFC-3339 string (UTC), for manifest timestamps.
fn now_iso() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "unknown".into())
}

fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    let mut size = n as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} {}", UNITS[unit])
    } else {
        format!("{size:.2} {}", UNITS[unit])
    }
}
