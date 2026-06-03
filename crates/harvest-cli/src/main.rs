//! `harvest` — command-line front end for the Harvest ingest engine.
//!
//! Example:
//!   harvest copy /path/to/SDCARD ./backupA ./backupB --hash xxh64
//!   harvest copy ./project ./archive --no-verify

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use indicatif::{ProgressBar, ProgressStyle};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use harvest_core::{
    harvest_files, scan, to_mhl, to_sidecar, HarvestOptions, HashAlgo, ManifestEntry,
};

const TOOL: &str = concat!("Harvest ", env!("CARGO_PKG_VERSION"));

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
            manifest,
            no_manifest,
        } => run_copy(source, dests, &hash, !no_verify, manifest, no_manifest),
    }
}

fn run_copy(
    source: PathBuf,
    dests: Vec<PathBuf>,
    hash: &str,
    verify: bool,
    manifest_path: Option<PathBuf>,
    no_manifest: bool,
) -> Result<()> {
    let algo = HashAlgo::parse(hash)
        .with_context(|| format!("unknown hash algorithm '{hash}' (expected xxh64, xxh3, or md5)"))?;

    if !source.exists() {
        bail!("source does not exist: {}", source.display());
    }

    println!("Scanning {} ...", source.display());
    let files = scan(&source).context("scanning source")?;
    let total_bytes: u64 = files.iter().map(|f| f.size).sum();
    let total_files = files.len();

    if total_files == 0 {
        println!("No files found. Nothing to do.");
        return Ok(());
    }

    println!(
        "{total_files} files, {} across {} destination(s). Hash: {}. Verify: {}.",
        human_bytes(total_bytes),
        dests.len(),
        algo.name(),
        if verify { "read-back" } else { "off" }
    );

    let opts = HarvestOptions {
        algo,
        verify,
        buf_size: harvest_core::DEFAULT_BUF_SIZE,
    };

    let bar = ProgressBar::new(total_bytes);
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

    let start_iso = now_iso();
    let start = Instant::now();
    let results = harvest_files(&files, &dests, &opts, |report| {
        done_bytes.fetch_add(report.bytes, Ordering::Relaxed);
        let n = done_files.fetch_add(1, Ordering::Relaxed) + 1;
        bar.set_position(done_bytes.load(Ordering::Relaxed));
        bar.set_message(format!("{n}/{total_files}"));
        if !report.all_ok() {
            for d in report.dests.iter().filter(|d| !d.ok) {
                failures
                    .lock()
                    .unwrap()
                    .push(format!("VERIFY FAILED: {}", d.path.display()));
            }
        }
    });
    let elapsed = start.elapsed();
    let finish_iso = now_iso();
    bar.finish_and_clear();

    // Surface any errors returned from worker threads (I/O failures, etc.).
    let mut errors: Vec<String> = Vec::new();
    for r in &results {
        if let Err(e) = r {
            errors.push(format!("{e:#}"));
        }
    }
    let verify_failures = failures.into_inner().unwrap();

    let secs = elapsed.as_secs_f64();
    let rate = if secs > 0.0 { total_bytes as f64 / secs } else { 0.0 };
    println!(
        "\nDone in {:.1}s — {} copied at {}/s",
        secs,
        human_bytes(total_bytes),
        human_bytes(rate as u64)
    );

    if !errors.is_empty() || !verify_failures.is_empty() {
        for e in &errors {
            eprintln!("ERROR: {e}");
        }
        for f in &verify_failures {
            eprintln!("{f}");
        }
        bail!(
            "{} error(s), {} verification failure(s) — manifest not written",
            errors.len(),
            verify_failures.len()
        );
    }

    println!(
        "All {total_files} files copied and {} OK.",
        if verify { "verified" } else { "written (not verified)" }
    );

    // Everything succeeded — write the proof-of-transfer manifest.
    if !no_manifest {
        let entries: Vec<ManifestEntry> = files
            .iter()
            .zip(&results)
            .filter_map(|(f, r)| {
                r.as_ref().ok().map(|report| ManifestEntry {
                    rel: f.rel.clone(),
                    size: report.bytes,
                    hash: report.source_hash.clone(),
                })
            })
            .collect();

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

/// Default manifest location: inside the first destination root.
fn default_manifest_path(dests: &[PathBuf], ext: &str) -> PathBuf {
    let root = dests.first().cloned().unwrap_or_else(|| PathBuf::from("."));
    root.join(format!("harvest-manifest.{ext}"))
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
