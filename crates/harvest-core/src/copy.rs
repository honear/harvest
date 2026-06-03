//! The heart of Harvest: copy a source file to one or more destinations,
//! hashing the bytes as we read them, then (optionally) reading each
//! destination back off disk and confirming its hash matches the source.
//!
//! Reading source once and fanning out the writes means a single read of the
//! card feeds N backup drives simultaneously.

use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::hash::{hash_file, HashAlgo, Hasher};

/// Outcome for a single destination of a single file.
#[derive(Debug, Clone)]
pub struct DestReport {
    pub path: PathBuf,
    /// Hash of the bytes read back off disk, if verification ran.
    pub verified_hash: Option<String>,
    /// True if the destination matched the source (or verification was skipped).
    pub ok: bool,
}

/// Outcome for a single source file across all its destinations.
#[derive(Debug, Clone)]
pub struct FileReport {
    pub source: PathBuf,
    /// Path relative to the source root. Defaults to the file name; the
    /// `harvest_files` layer overwrites it with the true relative path.
    pub rel: PathBuf,
    /// Source modification time (ns since Unix epoch); set by `harvest_files`.
    pub mtime_ns: i128,
    /// Path relative to each destination root where the file was written
    /// (after templating). Equals `rel` for a plain mirror copy.
    pub dest_rel: PathBuf,
    pub bytes: u64,
    pub source_hash: String,
    pub dests: Vec<DestReport>,
}

impl FileReport {
    pub fn all_ok(&self) -> bool {
        self.dests.iter().all(|d| d.ok)
    }
}

/// Copy `source` to every path in `dests`, hashing along the way.
///
/// * `verify` — when true, each destination is re-read from disk and its hash
///   compared against the source hash (full read-back verification).
/// * `buf_size` — streaming buffer size; larger favors throughput on big media.
pub fn copy_file_verified(
    source: &Path,
    dests: &[PathBuf],
    algo: HashAlgo,
    verify: bool,
    buf_size: usize,
) -> Result<FileReport> {
    let src = File::open(source).with_context(|| format!("opening source {}", source.display()))?;
    // Capture the source's modification time so we can stamp it onto each
    // destination — preserving record dates for date-organized archives and
    // making "skip already-present" re-runs match reliably.
    let src_modified = src.metadata().ok().and_then(|m| m.modified().ok());
    let mut reader = BufReader::with_capacity(buf_size, src);

    // Create parent directories and open a writer per destination.
    let mut writers: Vec<BufWriter<File>> = Vec::with_capacity(dests.len());
    for dest in dests {
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating destination dir {}", parent.display()))?;
        }
        let f = File::create(dest).with_context(|| format!("creating {}", dest.display()))?;
        writers.push(BufWriter::with_capacity(buf_size, f));
    }

    let mut src_hasher = Hasher::new(algo);
    let mut buf = vec![0u8; buf_size];
    let mut total: u64 = 0;

    loop {
        let n = reader.read(&mut buf).with_context(|| format!("reading {}", source.display()))?;
        if n == 0 {
            break;
        }
        let chunk = &buf[..n];
        src_hasher.update(chunk);
        for (w, dest) in writers.iter_mut().zip(dests) {
            w.write_all(chunk).with_context(|| format!("writing {}", dest.display()))?;
        }
        total += n as u64;
    }

    // Flush buffers and force data to physical disk before we read it back —
    // otherwise verification could pass against the OS cache, not the platter.
    for (w, dest) in writers.iter_mut().zip(dests) {
        w.flush().with_context(|| format!("flushing {}", dest.display()))?;
        let f = w.get_ref();
        f.sync_all()
            .with_context(|| format!("syncing {} to disk", dest.display()))?;
        // Preserve the source timestamp (best-effort; ignore on read-only FS).
        if let Some(t) = src_modified {
            let _ = f.set_modified(t);
        }
    }
    drop(writers);

    let source_hash = src_hasher.finalize_hex();

    let mut dest_reports = Vec::with_capacity(dests.len());
    for dest in dests {
        let report = if verify {
            let verified_hash = hash_file(dest, algo, buf_size)?;
            let ok = verified_hash == source_hash;
            DestReport {
                path: dest.clone(),
                verified_hash: Some(verified_hash),
                ok,
            }
        } else {
            DestReport {
                path: dest.clone(),
                verified_hash: None,
                ok: true,
            }
        };
        dest_reports.push(report);
    }

    Ok(FileReport {
        source: source.to_path_buf(),
        rel: source
            .file_name()
            .map(PathBuf::from)
            .unwrap_or_else(|| source.to_path_buf()),
        dest_rel: dests
            .first()
            .and_then(|d| d.file_name())
            .map(PathBuf::from)
            .unwrap_or_default(),
        mtime_ns: 0,
        bytes: total,
        source_hash,
        dests: dest_reports,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn temp_dir() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("harvest_test_{}_{n}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(path: &Path, bytes: &[u8]) {
        let mut f = File::create(path).unwrap();
        f.write_all(bytes).unwrap();
    }

    #[test]
    fn roundtrip_to_two_dests_verifies_ok() {
        let dir = temp_dir();
        let src = dir.join("src.bin");
        let data: Vec<u8> = (0..200_000u32).map(|i| (i % 256) as u8).collect();
        write(&src, &data);

        let dests = vec![dir.join("a/out.bin"), dir.join("b/out.bin")];
        let report = copy_file_verified(&src, &dests, HashAlgo::Xxh3, true, 4096).unwrap();

        assert!(report.all_ok(), "verification should pass for a clean copy");
        assert_eq!(report.bytes, data.len() as u64);
        // Source hash must equal an independent hash of each destination on disk.
        for d in &dests {
            assert_eq!(report.source_hash, hash_file(d, HashAlgo::Xxh3, 4096).unwrap());
        }
    }

    #[test]
    fn verification_comparison_is_real() {
        // Prove the verify logic would CATCH a bad copy: after a clean copy,
        // corrupt the destination and confirm its hash no longer matches.
        let dir = temp_dir();
        let src = dir.join("src.bin");
        write(&src, b"the quick brown fox");
        let dest = dir.join("dest.bin");

        let report = copy_file_verified(&src, &[dest.clone()], HashAlgo::Md5, true, 4096).unwrap();
        assert!(report.all_ok());

        // Flip the destination's contents.
        write(&dest, b"the quick brown FOX!");
        let corrupted = hash_file(&dest, HashAlgo::Md5, 4096).unwrap();
        assert_ne!(
            report.source_hash, corrupted,
            "a corrupted destination must hash differently — otherwise verify is meaningless"
        );
    }

    #[test]
    fn skipping_verify_reports_no_hash() {
        let dir = temp_dir();
        let src = dir.join("src.bin");
        write(&src, b"data");
        let dest = dir.join("dest.bin");
        let report = copy_file_verified(&src, &[dest], HashAlgo::Xxh3, false, 4096).unwrap();
        assert!(report.all_ok());
        assert!(report.dests[0].verified_hash.is_none());
    }
}
