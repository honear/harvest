//! Selection rules for which scanned files to ingest.
//!
//! All active criteria must pass (logical AND). Extension matching is
//! case-insensitive; an explicit exclude always beats an include.

use std::collections::HashSet;
use std::path::Path;

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
}

impl Filter {
    /// True when no criteria are set (keeps everything).
    pub fn is_empty(&self) -> bool {
        self.include_ext.is_none()
            && self.exclude_ext.is_empty()
            && self.min_size.is_none()
            && self.max_size.is_none()
            && self.newer_than_ns.is_none()
            && self.older_than_ns.is_none()
    }

    /// Whether a file passes every active rule.
    pub fn accepts(&self, f: &SourceFile) -> bool {
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
