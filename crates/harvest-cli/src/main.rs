//! `harvest` — command-line front end for the Harvest ingest engine.
//!
//! Example:
//!   harvest copy /path/to/SDCARD ./backupA ./backupB --hash xxh3
//!   harvest copy ./project ./archive --no-verify

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use indicatif::{ProgressBar, ProgressStyle};

use harvest_core::{harvest_files, scan, HarvestOptions, HashAlgo};

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
        /// Checksum algorithm: xxh3 (fast, default) or md5 (interop).
        #[arg(long, default_value = "xxh3")]
        hash: String,
        /// Skip read-back verification (faster, less safe).
        #[arg(long)]
        no_verify: bool,
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
        } => run_copy(source, dests, &hash, !no_verify),
    }
}

fn run_copy(source: PathBuf, dests: Vec<PathBuf>, hash: &str, verify: bool) -> Result<()> {
    let algo = HashAlgo::parse(hash)
        .with_context(|| format!("unknown hash algorithm '{hash}' (expected xxh3 or md5)"))?;

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
    let rate = if secs > 0.0 {
        total_bytes as f64 / secs
    } else {
        0.0
    };

    println!(
        "\nDone in {:.1}s — {} copied at {}/s",
        secs,
        human_bytes(total_bytes),
        human_bytes(rate as u64)
    );

    if errors.is_empty() && verify_failures.is_empty() {
        println!("All {total_files} files copied and {} OK.", if verify { "verified" } else { "written (not verified)" });
        Ok(())
    } else {
        for e in &errors {
            eprintln!("ERROR: {e}");
        }
        for f in &verify_failures {
            eprintln!("{f}");
        }
        bail!(
            "{} error(s), {} verification failure(s)",
            errors.len(),
            verify_failures.len()
        );
    }
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
