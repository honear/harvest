//! `harvest` — command-line front end for the Harvest ingest engine.
//!
//! The actual work lives in `harvest_core::run_harvest`; this binary only parses
//! arguments and renders progress.
//!
//! Examples:
//!   harvest copy /path/to/SDCARD ./backupA ./backupB --hash xxh64
//!   harvest copy ./project ./archive --no-verify
//!   harvest copy /path/to/SDCARD ./backupA --resume

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use indicatif::{ProgressBar, ProgressStyle};

use harvest_core::{run_harvest, Filter, HarvestConfig, HarvestEvent, HashAlgo};

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
        /// Destination path template, e.g. "{project}/{YYYY}-{MM}-{DD}/{filename}".
        /// Tokens: {filename} {name} {ext} {reldir} {relpath} {project}
        /// {YYYY} {YY} {MM} {DD} (job date) {fYYYY} {fMM} {fDD} (file date).
        #[arg(long, value_name = "TEMPLATE")]
        dest_template: Option<String>,
        /// Project name available to templates as {project}.
        #[arg(long, value_name = "NAME", default_value = "")]
        project: String,
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
            dest_template,
            project,
            resume,
            journal,
            manifest,
            no_manifest,
        } => {
            let filter = Filter::build(
                include_ext.as_deref(),
                exclude_ext.as_deref(),
                min_size.as_deref(),
                max_size.as_deref(),
                newer_than.as_deref(),
                older_than.as_deref(),
            )?;
            let algo = HashAlgo::parse(&hash)
                .with_context(|| format!("unknown hash algorithm '{hash}' (expected xxh64, xxh3, or md5)"))?;
            let cfg = HarvestConfig {
                source,
                dests,
                algo,
                verify: !no_verify,
                resume,
                filter,
                dest_template,
                project,
                write_manifest: !no_manifest,
                journal_path: journal,
                manifest_path: manifest,
            };
            run_copy(cfg)
        }
    }
}

fn run_copy(cfg: HarvestConfig) -> Result<()> {
    println!(
        "Harvesting {} -> {} destination(s). Hash: {}. Verify: {}.",
        cfg.source.display(),
        cfg.dests.len(),
        cfg.algo.name(),
        if cfg.verify { "read-back" } else { "off" }
    );

    let bar: Mutex<Option<ProgressBar>> = Mutex::new(None);
    let planned_files = AtomicU64::new(0);
    let start = Instant::now();

    let outcome = run_harvest(&cfg, |event| match event {
        HarvestEvent::Planned { total_scanned, kept, to_copy, skipped, copy_bytes } => {
            if kept != total_scanned {
                println!("Filter kept {kept} of {total_scanned} scanned files.");
            }
            println!(
                "{to_copy} file(s) to copy ({}), {skipped} already verified.",
                human_bytes(copy_bytes)
            );
            planned_files.store(to_copy as u64, Ordering::Relaxed);
            if to_copy > 0 {
                let b = ProgressBar::new(copy_bytes);
                b.set_style(
                    ProgressStyle::with_template(
                        "{bar:40.cyan/blue} {bytes}/{total_bytes} ({bytes_per_sec}, ETA {eta}) {msg}",
                    )
                    .unwrap()
                    .progress_chars("##-"),
                );
                *bar.lock().unwrap() = Some(b);
            }
        }
        HarvestEvent::FileDone { done_files, done_bytes, .. } => {
            if let Some(b) = bar.lock().unwrap().as_ref() {
                b.set_position(done_bytes);
                b.set_message(format!("{done_files}/{}", planned_files.load(Ordering::Relaxed)));
            }
        }
    })?;

    if let Some(b) = bar.lock().unwrap().take() {
        b.finish_and_clear();
    }

    let secs = start.elapsed().as_secs_f64();
    if outcome.copied_bytes > 0 {
        let rate = if secs > 0.0 { outcome.copied_bytes as f64 / secs } else { 0.0 };
        println!(
            "\nDone in {:.1}s — {} copied at {}/s",
            secs,
            human_bytes(outcome.copied_bytes),
            human_bytes(rate as u64)
        );
    }

    if !outcome.success() {
        for e in &outcome.errors {
            eprintln!("ERROR: {e}");
        }
        for f in &outcome.verify_failures {
            eprintln!("VERIFY FAILED: {f}");
        }
        eprintln!(
            "\nProgress saved to {} — re-run with --resume to continue.",
            outcome.journal_path.display()
        );
        bail!(
            "{} error(s), {} verification failure(s) — manifest not written",
            outcome.errors.len(),
            outcome.verify_failures.len()
        );
    }

    println!(
        "All {} file(s) {} OK ({} copied, {} already done).",
        outcome.copied + outcome.skipped,
        if cfg.verify { "verified" } else { "written (not verified)" },
        outcome.copied,
        outcome.skipped
    );
    if let Some(path) = &outcome.manifest_path {
        println!("Manifest: {}", path.display());
    }
    Ok(())
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
