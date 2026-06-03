//! Destination path templating for rename-on-ingest and folder structuring.
//!
//! A template is a string with `{token}` placeholders, e.g.
//! `"{project}/{YYYY}-{MM}-{DD}/{filename}"`. Rendering produces a relative
//! path that is mirrored under each destination root. Unknown tokens are left
//! literally so typos are visible. The result is sanitized: `.`/`..` and any
//! absolute prefix are stripped, so a template can never escape the dest root.
//!
//! This module is intentionally free of date libraries — callers pass already
//! computed calendar fields (so the core stays dependency-light).

use std::path::{Path, PathBuf};

/// Inputs available to a template for one file.
pub struct RenderCtx<'a> {
    /// Original path relative to the source root.
    pub rel: &'a Path,
    /// User-supplied project name (may be empty).
    pub project: &'a str,
    /// Job date (when the harvest started).
    pub job_year: i32,
    pub job_month: u8,
    pub job_day: u8,
    /// Source file's modification date.
    pub file_year: i32,
    pub file_month: u8,
    pub file_day: u8,
}

impl<'a> RenderCtx<'a> {
    fn lookup(&self, token: &str) -> Option<String> {
        let name = self.rel.file_name().map(|s| s.to_string_lossy().to_string());
        Some(match token {
            "filename" => name?,
            "name" => self
                .rel
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())?,
            "ext" => self
                .rel
                .extension()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default(),
            "reldir" => self
                .rel
                .parent()
                .map(to_fwd)
                .filter(|s| !s.is_empty())
                .unwrap_or_default(),
            "relpath" => to_fwd(self.rel),
            "project" => self.project.to_string(),
            "YYYY" => format!("{:04}", self.job_year),
            "YY" => format!("{:02}", (self.job_year % 100).abs()),
            "MM" => format!("{:02}", self.job_month),
            "DD" => format!("{:02}", self.job_day),
            "fYYYY" => format!("{:04}", self.file_year),
            "fYY" => format!("{:02}", (self.file_year % 100).abs()),
            "fMM" => format!("{:02}", self.file_month),
            "fDD" => format!("{:02}", self.file_day),
            _ => return None,
        })
    }
}

fn to_fwd(p: &Path) -> String {
    p.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// Substitute `{token}` placeholders. Unknown tokens are preserved verbatim.
fn substitute(template: &str, ctx: &RenderCtx) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        rest = &rest[open..];
        if let Some(close) = rest.find('}') {
            let token = &rest[1..close];
            match ctx.lookup(token) {
                Some(val) => out.push_str(&val),
                None => {
                    // Unknown token: keep it literally, braces included.
                    out.push_str(&rest[..=close]);
                }
            }
            rest = &rest[close + 1..];
        } else {
            // Unbalanced '{' — emit the remainder literally.
            out.push_str(rest);
            rest = "";
        }
    }
    out.push_str(rest);
    out
}

/// Render a template to a sanitized relative destination path.
pub fn render(template: &str, ctx: &RenderCtx) -> PathBuf {
    let rendered = substitute(template, ctx);
    let mut path = PathBuf::new();
    for part in rendered.split(['/', '\\']) {
        let part = part.trim();
        // Drop empties and traversal/absolute components so we stay rooted.
        if part.is_empty() || part == "." || part == ".." {
            continue;
        }
        path.push(part);
    }
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx<'a>(rel: &'a Path) -> RenderCtx<'a> {
        RenderCtx {
            rel,
            project: "Sunset Shoot",
            job_year: 2026,
            job_month: 6,
            job_day: 3,
            file_year: 2025,
            file_month: 11,
            file_day: 9,
        }
    }

    #[test]
    fn dated_folder_with_original_filename() {
        let rel = PathBuf::from("DCIM/100CANON/IMG_0042.CR3");
        let out = render("{project}/{YYYY}-{MM}-{DD}/{filename}", &ctx(&rel));
        assert_eq!(out, PathBuf::from("Sunset Shoot/2026-06-03/IMG_0042.CR3"));
    }

    #[test]
    fn file_date_and_parts() {
        let rel = PathBuf::from("clip.MOV");
        let out = render("{fYYYY}/{fMM}/{name}.{ext}", &ctx(&rel));
        assert_eq!(out, PathBuf::from("2025/11/clip.MOV"));
    }

    #[test]
    fn unknown_tokens_are_preserved() {
        let rel = PathBuf::from("a.txt");
        let out = render("{bogus}/{filename}", &ctx(&rel));
        assert_eq!(out, PathBuf::from("{bogus}/a.txt"));
    }

    #[test]
    fn path_traversal_is_stripped() {
        let rel = PathBuf::from("a.txt");
        // A malicious or sloppy template cannot escape the destination root.
        let out = render("../../etc/{filename}", &ctx(&rel));
        assert_eq!(out, PathBuf::from("etc/a.txt"));
    }
}
