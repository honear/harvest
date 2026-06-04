//! Harvest desktop backend.
//!
//! Thin Tauri command layer over `harvest_core`: it builds a `HarvestConfig`
//! from the UI's request, runs the harvest on a background thread, and streams
//! progress to the front end as events. Presets are stored as JSON in the app
//! config directory.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_notification::NotificationExt;

use harvest_core::{plan, run_harvest, run_verify, Filter, HarvestConfig, HarvestEvent, HashAlgo};

/// Shared cancel flag, set by `cancel_harvest`, watched by the running transfer.
#[derive(Default)]
struct Cancel(Arc<AtomicBool>);

/// Caches the most-recently scanned source's file list so the live Sow readout
/// can recompute the filtered total in-memory instead of re-walking the disk.
#[derive(Default)]
struct ScanCache(std::sync::Mutex<Option<(String, Vec<harvest_core::SourceFile>)>>);

/// Free bytes, total bytes, and removable flag of the drive containing `path`.
fn drive_space(path: &str) -> (u64, u64, bool) {
    let disks = sysinfo::Disks::new_with_refreshed_list();
    let needle = path.to_lowercase();
    let mut best: Option<(usize, u64, u64, bool)> = None;
    for d in disks.iter() {
        let mount = d.mount_point().to_string_lossy().to_lowercase();
        if needle.starts_with(&mount) && best.map_or(true, |(len, ..)| mount.len() > len) {
            best = Some((mount.len(), d.available_space(), d.total_space(), d.is_removable()));
        }
    }
    best.map(|(_, f, t, r)| (f, t, r)).unwrap_or((0, 0, false))
}

/// The mount point (volume root) that contains `path`.
fn mount_for(path: &str) -> String {
    let disks = sysinfo::Disks::new_with_refreshed_list();
    let needle = path.to_lowercase();
    let mut best: Option<(usize, String)> = None;
    for d in disks.iter() {
        let mount = d.mount_point().to_string_lossy().to_string();
        let ml = mount.to_lowercase();
        if needle.starts_with(&ml) && best.as_ref().map_or(true, |(len, _)| ml.len() > *len) {
            best = Some((ml.len(), mount));
        }
    }
    best.map(|(_, m)| m).unwrap_or_else(|| path.to_string())
}

/// Eject/unmount the removable drive that contains `path` (best-effort).
#[tauri::command]
fn eject_drive(path: String) -> Result<(), String> {
    let mount = mount_for(&path);
    #[cfg(target_os = "macos")]
    {
        let ok = std::process::Command::new("diskutil")
            .arg("eject")
            .arg(&mount)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        return if ok { Ok(()) } else { Err(format!("could not eject {mount}")) };
    }
    #[cfg(target_os = "windows")]
    {
        // Use the Shell "Eject" verb on the drive (e.g. E:\).
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let ps = format!(
            "(New-Object -ComObject Shell.Application).Namespace(17).ParseName('{}').InvokeVerb('Eject')",
            mount.replace('\'', "")
        );
        let ok = std::process::Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", &ps])
            .creation_flags(CREATE_NO_WINDOW)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        return if ok { Ok(()) } else { Err(format!("could not eject {mount}")) };
    }
    #[allow(unreachable_code)]
    Err("eject not supported on this platform".into())
}

/// Summary of a chosen source/destination folder.
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct PathInfo {
    files: u64,
    bytes: u64,
    /// Free space on the containing drive (0 if unknown).
    free_space: u64,
    /// Total size of the containing drive (0 if unknown).
    drive_total: u64,
    /// Whether the containing drive is removable (SD card, USB, etc.).
    removable: bool,
}

/// Count the files/bytes under a folder and report the containing drive's
/// free/total space. Runs off the UI thread.
#[tauri::command]
async fn inspect_path(path: String) -> PathInfo {
    tauri::async_runtime::spawn_blocking(move || {
        let p = PathBuf::from(&path);
        let (mut files, mut bytes) = (0u64, 0u64);
        if let Ok(list) = harvest_core::scan(&p) {
            files = list.len() as u64;
            bytes = list.iter().map(|f| f.size).sum();
        }
        let (free_space, drive_total, removable) = drive_space(&path);
        PathInfo { files, bytes, free_space, drive_total, removable }
    })
    .await
    .unwrap_or(PathInfo { files: 0, bytes: 0, free_space: 0, drive_total: 0, removable: false })
}

/// Files + bytes that a transfer would include for one source, after all
/// filters and exclusions — drives the live Sow readout.
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct SizeInfo {
    files: u64,
    bytes: u64,
}

#[tauri::command]
async fn transfer_size(req: CopyRequest, cache: tauri::State<'_, ScanCache>) -> Result<SizeInfo, String> {
    let cfg = build_config(req).map_err(|e| format!("{e:#}"))?;
    let key = cfg.source.display().to_string();
    let sum = |list: &[harvest_core::SourceFile]| {
        let mut files = 0u64;
        let mut bytes = 0u64;
        for f in list {
            if cfg.filter.accepts(f) {
                files += 1;
                bytes += f.size;
            }
        }
        SizeInfo { files, bytes }
    };
    // Cache hit: recompute from the in-memory list (no disk walk).
    {
        let guard = cache.0.lock().unwrap();
        if let Some((s, list)) = guard.as_ref() {
            if *s == key {
                return Ok(sum(list));
            }
        }
    }
    // Miss: walk once, cache it, then sum.
    let src = cfg.source.clone();
    let scanned = tauri::async_runtime::spawn_blocking(move || harvest_core::scan(&src).unwrap_or_default())
        .await
        .map_err(|e| e.to_string())?;
    let result = sum(&scanned);
    *cache.0.lock().unwrap() = Some((key, scanned));
    Ok(result)
}

/// File-type category index (0=video,1=audio,2=image,3=other).
fn category(ext: &str) -> usize {
    const VIDEO: &[&str] = &["mov", "mp4", "mxf", "avi", "mts", "m4v", "braw", "r3d", "mkv", "wmv"];
    const AUDIO: &[&str] = &["wav", "aif", "aiff", "mp3", "flac", "m4a", "aac"];
    const IMAGE: &[&str] = &[
        "jpg", "jpeg", "png", "cr3", "cr2", "arw", "dng", "nef", "tif", "tiff", "heic", "raf", "gpr", "gif",
    ];
    if VIDEO.contains(&ext) {
        0
    } else if AUDIO.contains(&ext) {
        1
    } else if IMAGE.contains(&ext) {
        2
    } else {
        3
    }
}

/// One nested child of a folder (its own immediate child), for the in-tile
/// mini-treemap labels.
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct Child {
    name: String,
    size: u64,
    cat: u8,
}

/// One immediate child of the scanned folder — feeds the Sow treemap/list.
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct DirEntry {
    name: String,
    path: String,
    size: u64,
    is_dir: bool,
    ext: String,
    /// Modification time in milliseconds since the Unix epoch (for date filters).
    mtime_ms: i64,
    /// For folders: nested immediate children (size + type), largest first,
    /// capped — used to draw labeled sub-rectangles inside the folder tile.
    children: Vec<Child>,
}

/// Total bytes + file count for one extension, for the breakdown panel.
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct ExtStat {
    ext: String,
    bytes: u64,
    files: u64,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct DirListing {
    path: String,
    total: u64,
    entries: Vec<DirEntry>,
    /// Extension breakdown across everything under `path`, largest first.
    exts: Vec<ExtStat>,
}

#[derive(Default)]
struct ImmAcc {
    size: u64,
    is_dir: bool,
    ext: String,
    mtime_ms: i64,
    nested: HashMap<String, (u64, [u64; 4])>, // grandchild -> (bytes, cat bytes)
}

/// Scan a folder in a single recursive walk, deriving: immediate children with
/// sizes, each folder's nested children (for labeled mini-treemaps), and the
/// extension breakdown for the whole subtree. Drill by calling again on a child.
#[tauri::command]
async fn scan_dir(path: String) -> Result<DirListing, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let base = PathBuf::from(&path);
        let list = harvest_core::scan(&base).map_err(|e| e.to_string())?;
        let mut total = 0u64;
        let mut imm: HashMap<String, ImmAcc> = HashMap::new();
        let mut exts: HashMap<String, (u64, u64)> = HashMap::new();

        for f in &list {
            total += f.size;
            let ext = f.rel.extension().map(|x| x.to_string_lossy().to_lowercase()).unwrap_or_default();
            let cat = category(&ext);
            let et = exts.entry(ext.clone()).or_insert((0, 0));
            et.0 += f.size;
            et.1 += 1;

            let mut comps = f.rel.components();
            let Some(c0) = comps.next() else { continue };
            let name = c0.as_os_str().to_string_lossy().to_string();
            let acc = imm.entry(name).or_default();
            acc.size += f.size;
            if let Some(c1) = comps.next() {
                acc.is_dir = true;
                let g = acc.nested.entry(c1.as_os_str().to_string_lossy().to_string()).or_insert((0, [0u64; 4]));
                g.0 += f.size;
                g.1[cat] += f.size;
            } else {
                acc.ext = ext;
                acc.mtime_ms = (f.mtime_ns / 1_000_000) as i64;
            }
        }

        let mut entries: Vec<DirEntry> = imm
            .into_iter()
            .map(|(name, acc)| {
                let mut children: Vec<Child> = acc
                    .nested
                    .into_iter()
                    .map(|(cn, (sz, cat))| {
                        let dom = (0..4).max_by_key(|&i| cat[i]).unwrap_or(3) as u8;
                        Child { name: cn, size: sz, cat: dom }
                    })
                    .collect();
                children.sort_by(|a, b| b.size.cmp(&a.size));
                children.truncate(24);
                DirEntry {
                    path: base.join(&name).display().to_string(),
                    name,
                    size: acc.size,
                    is_dir: acc.is_dir,
                    ext: acc.ext,
                    mtime_ms: acc.mtime_ms,
                    children,
                }
            })
            .collect();
        entries.sort_by(|a, b| b.size.cmp(&a.size));

        let mut exts: Vec<ExtStat> = exts
            .into_iter()
            .map(|(ext, (bytes, files))| ExtStat { ext, bytes, files })
            .collect();
        exts.sort_by(|a, b| b.bytes.cmp(&a.bytes));
        exts.truncate(14);

        Ok(DirListing { path, total, entries, exts })
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Copy request sent from the UI.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CopyRequest {
    pub source: String,
    pub dests: Vec<String>,
    pub hash: String,
    pub verify: bool,
    pub resume: bool,
    pub skip_existing: bool,
    pub include_ext: Option<String>,
    pub exclude_ext: Option<String>,
    pub min_size: Option<String>,
    pub max_size: Option<String>,
    pub newer_than: Option<String>,
    pub older_than: Option<String>,
    #[serde(default)]
    pub exclude_paths: Vec<String>,
    #[serde(default)]
    pub owner_only: bool,
    pub dest_template: Option<String>,
    pub project: Option<String>,
    pub write_manifest: bool,
}

/// The current OS account name (for the "only files I own" filter).
fn current_user() -> Option<String> {
    std::env::var("USERNAME").or_else(|_| std::env::var("USER")).ok().filter(|s| !s.is_empty())
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct PlannedPayload {
    total_scanned: usize,
    kept: usize,
    to_copy: usize,
    skipped: usize,
    copy_bytes: u64,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct ProgressPayload {
    rel: String,
    dest: String,
    bytes: u64,
    done_files: usize,
    done_bytes: u64,
    ok: bool,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct DonePayload {
    success: bool,
    copied: usize,
    skipped: usize,
    copied_bytes: u64,
    verify_failures: Vec<String>,
    errors: Vec<String>,
    manifest_path: Option<String>,
    journal_path: String,
    cancelled: bool,
    ejected: bool,
}

/// One past run, persisted to history.json.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HistoryEntry {
    when: u64, // unix seconds
    source: String,
    dests: Vec<String>,
    copied: usize,
    skipped: usize,
    bytes: u64,
    success: bool,
    cancelled: bool,
}

/// A saved set of options (everything except the source/destinations the user
/// picks per run).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Preset {
    pub name: String,
    #[serde(default)]
    pub sources: Vec<String>,
    #[serde(default)]
    pub dests: Vec<String>,
    pub hash: String,
    pub verify: bool,
    #[serde(default = "default_true")]
    pub skip_existing: bool,
    #[serde(default)]
    pub include_ext: Option<String>,
    #[serde(default)]
    pub exclude_ext: Option<String>,
    #[serde(default)]
    pub min_size: Option<String>,
    #[serde(default)]
    pub max_size: Option<String>,
    #[serde(default)]
    pub newer_than: Option<String>,
    #[serde(default)]
    pub older_than: Option<String>,
    #[serde(default)]
    pub exclude_paths: Vec<String>,
    #[serde(default)]
    pub owner_only: bool,
    #[serde(default)]
    pub dest_template: Option<String>,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default = "default_true")]
    pub write_manifest: bool,
}

fn default_true() -> bool {
    true
}
fn default_hash() -> String {
    "xxh64".into()
}

/// Global, app-wide settings (defaults for new transfers + behaviors).
/// Distinct from per-transfer Options, which live on each preset.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    #[serde(default = "default_hash")]
    pub default_hash: String,
    #[serde(default = "default_true")]
    pub default_verify: bool,
    #[serde(default = "default_true")]
    pub default_skip_existing: bool,
    #[serde(default = "default_true")]
    pub default_write_manifest: bool,
    #[serde(default)]
    pub default_exclude_ext: String,
    #[serde(default = "default_true")]
    pub confirm_before_harvest: bool,
    #[serde(default = "default_true")]
    pub notify: bool,
    #[serde(default)]
    pub auto_eject: bool,
    #[serde(default = "default_true")]
    pub keep_awake: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            default_hash: default_hash(),
            default_verify: true,
            default_skip_existing: true,
            default_write_manifest: true,
            default_exclude_ext: String::new(),
            confirm_before_harvest: true,
            notify: true,
            auto_eject: false,
            keep_awake: true,
        }
    }
}

fn settings_path(app: &AppHandle) -> Option<PathBuf> {
    let dir = app.path().app_config_dir().ok()?;
    std::fs::create_dir_all(&dir).ok();
    Some(dir.join("settings.json"))
}
fn read_settings(app: &AppHandle) -> Settings {
    settings_path(app)
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

#[tauri::command]
fn get_settings(app: AppHandle) -> Settings {
    read_settings(&app)
}

#[tauri::command]
fn save_settings(app: AppHandle, settings: Settings) -> Result<(), String> {
    let path = settings_path(&app).ok_or("no config dir")?;
    let json = serde_json::to_string_pretty(&settings).map_err(|e| e.to_string())?;
    std::fs::write(path, json).map_err(|e| e.to_string())
}

fn build_config(req: CopyRequest) -> anyhow::Result<HarvestConfig> {
    let algo = HashAlgo::parse(&req.hash)
        .ok_or_else(|| anyhow::anyhow!("unknown hash algorithm '{}'", req.hash))?;
    let mut filter = Filter::build(
        req.include_ext.as_deref(),
        req.exclude_ext.as_deref(),
        req.min_size.as_deref(),
        req.max_size.as_deref(),
        req.newer_than.as_deref(),
        req.older_than.as_deref(),
    )?;
    filter.exclude_paths = req.exclude_paths.iter().map(PathBuf::from).collect();
    if req.owner_only {
        filter.owner = current_user();
    }
    Ok(HarvestConfig {
        source: PathBuf::from(req.source),
        dests: req.dests.into_iter().map(PathBuf::from).collect(),
        algo,
        verify: req.verify,
        resume: req.resume,
        skip_existing: req.skip_existing,
        filter,
        dest_template: req.dest_template.filter(|s| !s.trim().is_empty()),
        project: req.project.unwrap_or_default(),
        write_manifest: req.write_manifest,
        journal_path: None,
        manifest_path: None,
    })
}

/// Pre-flight compare: what a harvest would do, plus the destination's free
/// space and whether the copy fits. Does not write anything.
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct PlanResult {
    total: usize,
    new: usize,
    present: usize,
    conflict: usize,
    copy_bytes: u64,
    dest_free: u64,
    fits: bool,
}

#[tauri::command]
async fn plan_harvest(req: CopyRequest) -> Result<PlanResult, String> {
    let first_dest = req.dests.first().cloned().unwrap_or_default();
    let cfg = build_config(req).map_err(|e| format!("{e:#}"))?;
    let p = tauri::async_runtime::spawn_blocking(move || plan(&cfg))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("{e:#}"))?;
    let (dest_free, _, _) = drive_space(&first_dest);
    let fits = dest_free == 0 || dest_free >= p.copy_bytes;
    Ok(PlanResult {
        total: p.total,
        new: p.new,
        present: p.present,
        conflict: p.conflict,
        copy_bytes: p.copy_bytes,
        dest_free,
        fits,
    })
}

#[tauri::command]
fn cancel_harvest(cancel: State<Cancel>) {
    cancel.0.store(true, Ordering::Release);
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn history_path(app: &AppHandle) -> anyhow::Result<PathBuf> {
    let dir = app.path().app_config_dir()?;
    std::fs::create_dir_all(&dir).ok();
    Ok(dir.join("history.json"))
}

fn append_history(app: &AppHandle, entry: HistoryEntry) {
    let Ok(path) = history_path(app) else { return };
    let mut entries: Vec<HistoryEntry> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    entries.insert(0, entry);
    entries.truncate(100);
    if let Ok(json) = serde_json::to_string_pretty(&entries) {
        let _ = std::fs::write(path, json);
    }
}

#[tauri::command]
fn list_history(app: AppHandle) -> Vec<HistoryEntry> {
    history_path(&app)
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

#[tauri::command]
fn clear_history(app: AppHandle) {
    if let Ok(path) = history_path(&app) {
        let _ = std::fs::remove_file(path);
    }
}

/// Start a harvest. Returns immediately; progress arrives via the
/// `harvest:planned`, `harvest:progress`, `harvest:done`, and `harvest:failed`
/// events.
#[tauri::command]
fn start_harvest(app: AppHandle, cancel: State<Cancel>, req: CopyRequest) -> Result<(), String> {
    let source = req.source.clone();
    let dests = req.dests.clone();
    let cfg = build_config(req).map_err(|e| format!("{e:#}"))?;
    let settings = read_settings(&app);

    // Fresh cancel flag for this run.
    let flag = cancel.0.clone();
    flag.store(false, Ordering::Release);

    std::thread::spawn(move || {
        // Keep the machine awake for the transfer (best-effort), if enabled.
        let _awake = if settings.keep_awake {
            keepawake::Builder::default()
                .display(false)
                .idle(true)
                .sleep(true)
                .reason("Harvesting media")
                .app_name("Harvest")
                .create()
                .ok()
        } else {
            None
        };

        let emitter = app.clone();
        let last = std::sync::Mutex::new(std::time::Instant::now());
        let result = run_harvest(&cfg, &flag, move |event| match event {
            HarvestEvent::Planned { total_scanned, kept, to_copy, skipped, copy_bytes } => {
                let _ = emitter.emit(
                    "harvest:planned",
                    PlannedPayload { total_scanned, kept, to_copy, skipped, copy_bytes },
                );
            }
            HarvestEvent::FileDone { rel, dest, bytes, done_files, done_bytes, ok } => {
                let mut guard = last.lock().unwrap();
                if guard.elapsed().as_millis() >= 80 {
                    *guard = std::time::Instant::now();
                    drop(guard);
                    let _ = emitter.emit(
                        "harvest:progress",
                        ProgressPayload { rel, dest, bytes, done_files, done_bytes, ok },
                    );
                }
            }
        });

        match result {
            Ok(outcome) => {
                append_history(
                    &app,
                    HistoryEntry {
                        when: now_secs(),
                        source: source.clone(),
                        dests: dests.clone(),
                        copied: outcome.copied,
                        skipped: outcome.skipped,
                        bytes: outcome.copied_bytes,
                        success: outcome.success(),
                        cancelled: outcome.cancelled,
                    },
                );
                // Auto-eject the source if it's a removable drive and enabled.
                let ejected = outcome.success()
                    && settings.auto_eject
                    && drive_space(&source).2
                    && eject_drive(source.clone()).is_ok();
                if settings.notify {
                    let body = if outcome.cancelled {
                        format!("Cancelled — {} files copied", outcome.copied)
                    } else if outcome.success() {
                        let mut b = format!("{} copied, {} already present", outcome.copied, outcome.skipped);
                        if ejected {
                            b.push_str(" · source ejected");
                        }
                        b
                    } else {
                        format!("Finished with {} problem(s)", outcome.errors.len() + outcome.verify_failures.len())
                    };
                    let _ = app.notification().builder().title("Harvest").body(body).show();
                }
                let _ = app.emit(
                    "harvest:done",
                    DonePayload {
                        success: outcome.success(),
                        copied: outcome.copied,
                        skipped: outcome.skipped,
                        copied_bytes: outcome.copied_bytes,
                        verify_failures: outcome.verify_failures,
                        errors: outcome.errors,
                        manifest_path: outcome.manifest_path.map(|p| p.display().to_string()),
                        journal_path: outcome.journal_path.display().to_string(),
                        cancelled: outcome.cancelled,
                        ejected,
                    },
                );
            }
            Err(e) => {
                let _ = app.emit("harvest:failed", format!("{e:#}"));
            }
        }
    });

    Ok(())
}

/// Verify existing destination copies against the source (no copying). Streams
/// the same harvest:planned/progress/done events; the done payload's `copied`
/// is the number of files that verified OK.
#[tauri::command]
fn verify_harvest(app: AppHandle, cancel: State<Cancel>, req: CopyRequest) -> Result<(), String> {
    let cfg = build_config(req).map_err(|e| format!("{e:#}"))?;
    let flag = cancel.0.clone();
    flag.store(false, Ordering::Release);

    std::thread::spawn(move || {
        let _awake = keepawake::Builder::default()
            .idle(true)
            .sleep(true)
            .reason("Verifying media")
            .app_name("Harvest")
            .create()
            .ok();
        let emitter = app.clone();
        let last = std::sync::Mutex::new(std::time::Instant::now());
        let result = run_verify(&cfg, &flag, move |event| match event {
            HarvestEvent::Planned { total_scanned, kept, to_copy, skipped, copy_bytes } => {
                let _ = emitter.emit(
                    "harvest:planned",
                    PlannedPayload { total_scanned, kept, to_copy, skipped, copy_bytes },
                );
            }
            HarvestEvent::FileDone { rel, dest, bytes, done_files, done_bytes, ok } => {
                let mut guard = last.lock().unwrap();
                if guard.elapsed().as_millis() >= 80 {
                    *guard = std::time::Instant::now();
                    drop(guard);
                    let _ = emitter.emit(
                        "harvest:progress",
                        ProgressPayload { rel, dest, bytes, done_files, done_bytes, ok },
                    );
                }
            }
        });
        match result {
            Ok(o) => {
                let _ = app.emit(
                    "harvest:done",
                    DonePayload {
                        success: o.success(),
                        copied: o.copied,
                        skipped: o.skipped,
                        copied_bytes: o.copied_bytes,
                        verify_failures: o.verify_failures,
                        errors: o.errors,
                        manifest_path: None,
                        journal_path: String::new(),
                        cancelled: o.cancelled,
                        ejected: false,
                    },
                );
            }
            Err(e) => {
                let _ = app.emit("harvest:failed", format!("{e:#}"));
            }
        }
    });
    Ok(())
}

fn presets_path(app: &AppHandle) -> anyhow::Result<PathBuf> {
    let dir = app.path().app_config_dir()?;
    std::fs::create_dir_all(&dir).ok();
    Ok(dir.join("presets.json"))
}

fn read_presets(app: &AppHandle) -> Vec<Preset> {
    presets_path(app)
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str::<Vec<Preset>>(&s).ok())
        .unwrap_or_default()
}

fn write_presets(app: &AppHandle, presets: &[Preset]) -> anyhow::Result<()> {
    let path = presets_path(app)?;
    std::fs::write(path, serde_json::to_string_pretty(presets)?)?;
    Ok(())
}

#[tauri::command]
fn list_presets(app: AppHandle) -> Vec<Preset> {
    read_presets(&app)
}

#[tauri::command]
fn save_preset(app: AppHandle, preset: Preset) -> Result<(), String> {
    let mut presets = read_presets(&app);
    presets.retain(|p| p.name != preset.name);
    presets.push(preset);
    presets.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    write_presets(&app, &presets).map_err(|e| format!("{e:#}"))
}

#[tauri::command]
fn delete_preset(app: AppHandle, name: String) -> Result<(), String> {
    let mut presets = read_presets(&app);
    presets.retain(|p| p.name != name);
    write_presets(&app, &presets).map_err(|e| format!("{e:#}"))
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_window_state::Builder::default().build())
        .plugin(
            tauri_plugin_log::Builder::new()
                .level(log::LevelFilter::Info)
                .target(tauri_plugin_log::Target::new(
                    tauri_plugin_log::TargetKind::LogDir { file_name: Some("harvest".into()) },
                ))
                .build(),
        )
        .manage(Cancel::default())
        .manage(ScanCache::default())
        .invoke_handler(tauri::generate_handler![
            inspect_path,
            scan_dir,
            transfer_size,
            plan_harvest,
            start_harvest,
            verify_harvest,
            eject_drive,
            cancel_harvest,
            list_history,
            clear_history,
            get_settings,
            save_settings,
            list_presets,
            save_preset,
            delete_preset
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
