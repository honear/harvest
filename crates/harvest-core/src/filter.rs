//! Selection rules for which scanned files to ingest.
//!
//! All active criteria must pass (logical AND). Extension matching is
//! case-insensitive; an explicit exclude always beats an include.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use time::{Date, Month, Time};

use crate::scan::SourceFile;

/// A set of include/exclude rules applied to each [`SourceFile`].
#[derive(Debug, Clone, Default)]
pub struct Filter {
    /// If set, only files whose extension is in this set are kept.
    pub include_ext: Option<HashSet<String>>,
    /// Files whose extension is in this set are always dropped.
    pub exclude_ext: HashSet<String>,
    /// Minimum size in bytes (inclusive).
    pub min_size: Option<u64>,
    /// Maximum size in bytes (inclusive).
    pub max_size: Option<u64>,
    /// Keep only files modified at or after this instant (ns since Unix epoch).
    pub newer_than_ns: Option<i128>,
    /// Keep only files modified strictly before this instant (ns since Unix epoch).
    pub older_than_ns: Option<i128>,
    /// Absolute paths (files or folders) to exclude. A file is dropped if any
    /// of these is a prefix of its absolute path. Populated from the UI's
    /// exclusion list / the Sow visualizer.
    pub exclude_paths: Vec<PathBuf>,
}

impl Filter {
    /// Build a filter from raw string inputs (as supplied by a CLI flag or GUI
    /// field). Extension lists are comma-separated; sizes accept units like
    /// `10MB`; dates are `YYYY-MM-DD` (UTC). Empty/`None` inputs are inactive.
    pub fn build(
        include_ext: Option<&str>,
        exclude_ext: Option<&str>,
        min_size: Option<&str>,
        max_size: Option<&str>,
        newer_than: Option<&str>,
        older_than: Option<&str>,
    ) -> Result<Self> {
        let parse_exts = |s: &str| -> HashSet<String> {
            s.split(',')
                .map(|e| e.trim().trim_start_matches('.').to_lowercase())
                .filter(|e| !e.is_empty())
                .collect()
        };
        Ok(Filter {
            include_ext: include_ext.map(parse_exts).filter(|s| !s.is_empty()),
            exclude_ext: exclude_ext.map(parse_exts).unwrap_or_default(),
            min_size: opt_str(min_size).map(parse_size).transpose()?,
            max_size: opt_str(max_size).map(parse_size).transpose()?,
            newer_than_ns: opt_str(newer_than).map(parse_date_ns).transpose()?,
            older_than_ns: opt_str(older_than).map(parse_date_ns).transpose()?,
            exclude_paths: Vec::new(),
        })
    }

    /// True when no criteria are set (keeps everything).
    pub fn is_empty(&self) -> bool {
        self.include_ext.is_none()
            && self.exclude_ext.is_empty()
            && self.min_size.is_none()
            && self.max_size.is_none()
            && self.newer_than_ns.is_none()
            && self.older_than_ns.is_none()
            && self.exclude_paths.is_empty()
    }

    /// Whether a file passes every active rule.
    pub fn accepts(&self, f: &SourceFile) -> bool {
        for ex in &self.exclude_paths {
            if f.abs.starts_with(ex) {
                return false;
            }
        }
        if let Some(min) = self.min_size {
            if f.size < min {
                return false;
            }
        }
        if let Some(max) = self.max_size {
            if f.size > max {
                return false;
            }
        }
        if let Some(after) = self.newer_than_ns {
            if f.mtime_ns < after {
                return false;
            }
        }
        if let Some(before) = self.older_than_ns {
            if f.mtime_ns >= before {
                return false;
            }
        }

        match ext_of(&f.rel) {
            Some(ext) => {
                if self.exclude_ext.contains(&ext) {
                    return false;
                }
                if let Some(inc) = &self.include_ext {
                    if !inc.contains(&ext) {
                        return false;
                    }
                }
            }
            None => {
                // A file with no extension can't satisfy an include-extension list.
                if self.include_ext.is_some() {
                    return false;
                }
            }
        }
        true
    }
}

/// Lower-cased extension (without the dot), or `None` if the file has none.
fn ext_of(rel: &Path) -> Option<String> {
    rel.extension().map(|e| e.to_string_lossy().to_lowercase())
}

/// Treat an empty/whitespace string the same as `None`.
fn opt_str(s: Option<&str>) -> Option<&str> {
    s.map(str::trim).filter(|s| !s.is_empty())
}

/// Parse a human size like "10MB", "500K", "1.5G", or a plain byte count
/// (binary units: 1 K = 1024).
pub fn parse_size(s: &str) -> Result<u64> {
    let s = s.trim();
    let split = s.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let value: f64 = num.parse().with_context(|| format!("invalid size '{s}'"))?;
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
pub fn parse_date_ns(s: &str) -> Result<i128> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn file(name: &str, size: u64, mtime_ns: i128) -> SourceFile {
        SourceFile {
            abs: PathBuf::from(name),
            rel: PathBuf::from(name),
            size,
            mtime_ns,
            dest_rel: None,
        }
    }

    #[test]
    fn empty_filter_accepts_everything() {
        let f = Filter::default();
        assert!(f.is_empty());
        assert!(f.accepts(&file("a.mov", 100, 0)));
        assert!(f.accepts(&file("noext", 0, 0)));
    }

    #[test]
    fn include_ext_is_case_insensitive_and_excludes_others() {
        let f = Filter {
            include_ext: Some(["mov", "mxf"].iter().map(|s| s.to_string()).collect()),
            ..Default::default()
        };
        assert!(f.accepts(&file("clip.MOV", 1, 0)));
        assert!(!f.accepts(&file("clip.jpg", 1, 0)));
        assert!(!f.accepts(&file("README", 1, 0))); // no extension
    }

    #[test]
    fn exclude_beats_include() {
        let f = Filter {
            include_ext: Some(["mov", "tmp"].iter().map(|s| s.to_string()).collect()),
            exclude_ext: ["tmp"].iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        };
        assert!(f.accepts(&file("a.mov", 1, 0)));
        assert!(!f.accepts(&file("a.tmp", 1, 0)));
    }

    #[test]
    fn exclude_paths_drop_subtrees() {
        let f = Filter {
            exclude_paths: vec![PathBuf::from("/card/PRIVATE"), PathBuf::from("/card/junk.tmp")],
            ..Default::default()
        };
        let mut keep = file("/card/CLIP/a.mp4", 1, 0);
        keep.abs = PathBuf::from("/card/CLIP/a.mp4");
        let mut drop_dir = file("/card/PRIVATE/x.xml", 1, 0);
        drop_dir.abs = PathBuf::from("/card/PRIVATE/x.xml");
        let mut drop_file = file("/card/junk.tmp", 1, 0);
        drop_file.abs = PathBuf::from("/card/junk.tmp");
        assert!(f.accepts(&keep));
        assert!(!f.accepts(&drop_dir));
        assert!(!f.accepts(&drop_file));
    }

    #[test]
    fn size_and_date_bounds() {
        let f = Filter {
            min_size: Some(10),
            max_size: Some(100),
            newer_than_ns: Some(1000),
            older_than_ns: Some(2000),
            ..Default::default()
        };
        assert!(f.accepts(&file("ok", 50, 1500)));
        assert!(!f.accepts(&file("too_small", 5, 1500)));
        assert!(!f.accepts(&file("too_big", 200, 1500)));
        assert!(!f.accepts(&file("too_old", 50, 500)));
        assert!(!f.accepts(&file("too_new", 50, 2000))); // older_than is exclusive
    }
}
