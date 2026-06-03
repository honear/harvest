//! Crash-safe transfer journal enabling stop & resume.
//!
//! Layout: a JSON-lines file whose first line is a [`JournalHeader`] describing
//! the job (algorithm, verify mode, destination set), followed by one
//! [`JournalRecord`] per file that was successfully copied (and verified, if
//! verification was on). Each record is flushed immediately, so an interrupted
//! run loses at most the single file that was in flight.
//!
//! On resume, a file is skipped only if its journal record still matches the
//! current source by size and modification time — a changed source is re-copied.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const JOURNAL_VERSION: u32 = 1;

/// First line of a journal: identifies the job so we only resume a compatible one.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct JournalHeader {
    pub v: u32,
    pub algo: String,
    pub verify: bool,
    pub dests: Vec<String>,
}

impl JournalHeader {
    /// Whether a run with these settings may safely resume from this journal.
    /// Destinations are compared as a set (order-independent).
    pub fn compatible_with(&self, algo: &str, verify: bool, dests: &[String]) -> bool {
        self.v == JOURNAL_VERSION
            && self.algo == algo
            && self.verify == verify
            && same_set(&self.dests, dests)
    }
}

/// One completed file.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct JournalRecord {
    pub rel: String,
    pub size: u64,
    pub mtime_ns: i128,
    pub hash: String,
}

/// A parsed journal.
pub struct LoadedJournal {
    pub header: JournalHeader,
    pub done: HashMap<String, JournalRecord>,
}

fn same_set(a: &[String], b: &[String]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut a: Vec<&String> = a.iter().collect();
    let mut b: Vec<&String> = b.iter().collect();
    a.sort();
    b.sort();
    a == b
}

/// Load an existing journal, or `None` if it's missing or its header is invalid.
/// Malformed record lines are skipped rather than failing the whole load.
pub fn load(path: &Path) -> Option<LoadedJournal> {
    let file = File::open(path).ok()?;
    let mut lines = BufReader::new(file).lines();
    let header_line = lines.next()?.ok()?;
    let header: JournalHeader = serde_json::from_str(&header_line).ok()?;

    let mut done = HashMap::new();
    for line in lines.map_while(Result::ok) {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(rec) = serde_json::from_str::<JournalRecord>(&line) {
            done.insert(rec.rel.clone(), rec);
        }
    }
    Some(LoadedJournal { header, done })
}

/// A journal open for writing. Thread-safe: `record` may be called concurrently
/// from worker threads; each call flushes to disk.
pub struct Journal {
    writer: Mutex<BufWriter<File>>,
}

impl Journal {
    /// Create a fresh journal (truncating any existing file) and write the header.
    pub fn create(path: &Path, header: &JournalHeader) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let file =
            File::create(path).with_context(|| format!("creating journal {}", path.display()))?;
        let mut writer = BufWriter::new(file);
        writeln!(writer, "{}", serde_json::to_string(header)?)?;
        writer.flush()?;
        Ok(Self {
            writer: Mutex::new(writer),
        })
    }

    /// Open an existing journal for appending (assumes a header is already present).
    pub fn append(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .append(true)
            .open(path)
            .with_context(|| format!("opening journal {} for append", path.display()))?;
        Ok(Self {
            writer: Mutex::new(BufWriter::new(file)),
        })
    }

    /// Append one record and flush it to disk.
    pub fn record(&self, rec: &JournalRecord) -> Result<()> {
        let line = serde_json::to_string(rec)?;
        let mut w = self.writer.lock().unwrap();
        writeln!(w, "{line}")?;
        w.flush()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn temp_path() -> std::path::PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("harvest_journal_{}_{n}.jsonl", std::process::id()))
    }

    fn header() -> JournalHeader {
        JournalHeader {
            v: JOURNAL_VERSION,
            algo: "xxh64".into(),
            verify: true,
            dests: vec!["A".into(), "B".into()],
        }
    }

    #[test]
    fn write_then_load_roundtrips() {
        let path = temp_path();
        let j = Journal::create(&path, &header()).unwrap();
        j.record(&JournalRecord { rel: "a/b.mov".into(), size: 10, mtime_ns: 123, hash: "ff".into() }).unwrap();
        j.record(&JournalRecord { rel: "c.txt".into(), size: 3, mtime_ns: 456, hash: "ee".into() }).unwrap();
        drop(j);

        let loaded = load(&path).expect("should load");
        assert_eq!(loaded.done.len(), 2);
        assert_eq!(loaded.done["a/b.mov"].hash, "ff");
        assert!(loaded.header.compatible_with("xxh64", true, &["B".into(), "A".into()]));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn incompatible_settings_are_rejected() {
        let h = header();
        assert!(!h.compatible_with("md5", true, &["A".into(), "B".into()]), "different algo");
        assert!(!h.compatible_with("xxh64", false, &["A".into(), "B".into()]), "different verify");
        assert!(!h.compatible_with("xxh64", true, &["A".into()]), "different dest set");
    }

    #[test]
    fn missing_journal_loads_as_none() {
        assert!(load(&temp_path()).is_none());
    }
}
