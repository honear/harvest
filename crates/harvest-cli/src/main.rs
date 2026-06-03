//! `harvest` — command-line front end for the Harvest ingest engine.
//!
//! Examples:
//!   harvest copy /path/to/SDCARD ./backupA ./backupB --hash xxh64
//!   harvest copy ./project ./archive --no-verify
//!   harvest copy /path/to/SDCARD ./backupA --resume   # continue an interrupted run

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use indicatif::{ProgressBar, ProgressStyle};
use time::format_description::well_known::Rfc3339;
use time::{Date, Month, OffsetDateTime, Time};

use harvest_core::{
    harvest_files, journal, scan, to_mhl, to_sidecar, Filter, HarvestOptions, HashAlgo, Journal,
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
        /// Only copy these extensions (comma-separated, e.g. "mov,mxf,wav").
        #[arg(long, value_name = "EXTS")]
        include_ext: Option<String>,
        /// Never copy these extensions (comma-separated, e.g. "tmp,thm").
        #[arg(long, value_name = "EXTS")]
        exclude_ext: Option<String>,
        /// Skip files smaller than this size (e.g. 10MB, 500K).
        #[arg(long, value_name = "SIZE")]
        min_size: Option<String>,
        /// Skip files larger than this size (e.g. 4GB).
        #[arg(long, value_name = "SIZE")]
        max_size: Option<String>,
        /// Only files modified on or after this date (YYYY-MM-DD, UTC).
        #[arg(long, value_name = "DATE")]
        newer_than: Option<String>,
        /// Only files modified before this date (YYYY-MM-DD, UTC).
        #[arg(long, value_name = "DATE")]
        older_than: Option<String>,
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
            include_ext,
            exclude_ext,
            min_size,
            max_size,
            newer_than,
            older_than,
            resume,
            journal,
            manifest,
            no_manifest,
        } => {
            let filter = build_filter(
                include_ext,
                exclude_ext,
                min_size,
                max_size,
                newer_than,
                older_than,
            )?;
            run_copy(RunArgs {
                source,
                dests,
                hash,
                verify: !no_verify,
                filter,
                resume,
                journal_path: journal,
                manifest_path: manifest,
                no_manifest,
            })
        }
    }
}

struct RunArgs {
    source: PathBuf,
    dests: Vec<PathBuf>,
    hash: String,
    verify: bool,
    filter: Filter,
    resume: bool,
    journal_path: Option<PathBuf>,
    manifest_path: Option<PathBuf>,
    no_manifest: bool,
}

/// Translate the CLI's string filter flags into a core [`Filter`].
fn build_filter(
    include_ext: Option<String>,
    exclude_ext: Option<String>,
    min_size: Option<String>,
    max_size: Option<String>,
    newer_than: Option<String>,
    older_than: Option<String>,
) -> Result<Filter> {
    let parse_exts = |s: String| -> HashSet<String> {
        s.split(',')
            .map(|e| e.trim().trim_start_matches('.').to_lowercase())
            .filter(|e| !e.is_empty())
            .collect()
    };
    Ok(Filter {
        include_ext: include_ext.map(parse_exts).filter(|s| !s.is_empty()),
        exclude_ext: exclude_ext.map(parse_exts).unwrap_or_default(),
        min_size: min_size.map(|s| parse_size(&s)).transpose()?,
        max_size: max_size.map(|s| parse_size(&s)).transpose()?,
        newer_than_ns: newer_than.map(|s| parse_date_ns(&s)).transpose()?,
        older_than_ns: older_than.map(|s| parse_date_ns(&s)).transpose()?,
    })
}

fn run_copy(args: RunArgs) -> Result<()> {
    let RunArgs {
        source,
        dests,
        hash,
        verify,
        filter,
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
    let mut all_files = scan(&source).context("scanning source")?;
    let scanned = all_files.len();
    if !filter.is_empty() {
        all_files.retain(|f| filter.accepts(f));
        let removed = scanned - all_files.len();
        println!("Filter kept {} of {scanned} files ({removed} excluded).", all_files.len());
    }
    if all_files.is_empty() {
        println!("No files to copy after scanning/filtering. Nothing to do.");
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
        match done.get(&rel_fwd) {
            Some(rec) if rec.size == f.size && rec.mtime_ns == f.mtime_ns => {
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
                let _ = journal.record(&JournalRecord {
                    rel: forward_slash(&report.rel),
                    size: report.bytes,
                    mtime_ns: report.mtime_ns,
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

/// Parse a human size like "10MB", "500K", "1.5G", or a plain byte count.
/// Units are binary (1 K = 1024).
fn parse_size(s: &str) -> Result<u64> {
    let s = s.trim();
    let split = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let value: f64 = num
        .parse()
        .with_context(|| format!("invalid size '{s}'"))?;
    let mult: f64 = match unit.trim().to_ascii_uppercase().as_str() {
        "" | "B" => 1.0,
        "K" | "KB" => 1024.0,
        "M" | "MB" => 1024.0 * 1024.0,
        "G" | "GB" => 1024.0 * 1024.0 * 1024.0,
        "T" | "TB" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        other => bail!("unknown size unit '{other}' in '{s}'"),
    };
    Ok((value * mult) as u64)
}

/// Parse a `YYYY-MM-DD` date as nanoseconds since the Unix epoch (UTC midnight).
fn parse_date_ns(s: &str) -> Result<i128> {
    let parts: Vec<&str> = s.trim().split('-').collect();
    if parts.len() != 3 {
        bail!("invalid date '{s}' (expected YYYY-MM-DD)");
    }
    let year: i32 = parts[0].parse().with_context(|| format!("invalid year in '{s}'"))?;
    let month: u8 = parts[1].parse().with_context(|| format!("invalid month in '{s}'"))?;
    let day: u8 = parts[2].parse().with_context(|| format!("invalid day in '{s}'"))?;
    let date = Date::from_calendar_date(year, Month::try_from(month)?, day)
        .with_context(|| format!("invalid date '{s}'"))?;
    Ok(date.with_time(Time::MIDNIGHT).assume_utc().unix_timestamp_nanos())
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
