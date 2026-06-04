//! Harvest desktop backend.
//!
//! Thin Tauri command layer over `harvest_core`: it builds a `HarvestConfig`
//! from the UI's request, runs the harvest on a background thread, and streams
//! progress to the front end as events. Presets are stored as JSON in the app
//! config directory.

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
        let ps = format!(
            "(New-Object -ComObject Shell.Application).Namespace(17).ParseName('{}').InvokeVerb('Eject')",
            mount.replace('\'', "")
        );
        let ok = std::process::Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", &ps])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        return if ok { Ok(()) } else { Err(format!("could not eject {mount}")) };
    }
    #[allow(unreachable_code)]
    Err("eject not supported on this platform".into())
}

/// A mounted drive/volume shown in the center "Disks" panel.
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct DriveInfo {
    /// Volume label (may be empty).
    name: String,
    /// Mount point / drive root (e.g. `C:\` or `/Volumes/SD`).
    mount: String,
    total: u64,
    available: u64,
    removable: bool,
    /// "SSD", "HDD", or "Unknown".
    kind: String,
}

/// Enumerate mounted drives/volumes (cross-platform via `sysinfo`).
#[tauri::command]
fn list_drives() -> Vec<DriveInfo> {
    let disks = sysinfo::Disks::new_with_refreshed_list();
    let mut out: Vec<DriveInfo> = disks
        .iter()
        .map(|d| DriveInfo {
            name: d.name().to_string_lossy().to_string(),
            mount: d.mount_point().to_string_lossy().to_string(),
            total: d.total_space(),
            available: d.available_space(),
            removable: d.is_removable(),
            kind: format!("{:?}", d.kind()),
        })
        .collect();
    out.sort_by(|a, b| a.mount.cmp(&b.mount));
    out
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

/// One immediate child of a folder, with its recursive total size — feeds the
/// Sow treemap.
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
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct DirListing {
    path: String,
    total: u64,
    entries: Vec<DirEntry>,
}

fn dir_size(p: &std::path::Path) -> u64 {
    harvest_core::scan(p)
        .map(|list| list.iter().map(|f| f.size).sum())
        .unwrap_or(0)
}

/// List a folder's immediate children, each with its total recursive size,
/// largest first. Drives the treemap; drill down by calling again on a subdir.
#[tauri::command]
async fn scan_dir(path: String) -> Result<DirListing, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let base = PathBuf::from(&path);
        let mut entries: Vec<DirEntry> = Vec::new();
        let mut total = 0u64;
        let read = std::fs::read_dir(&base).map_err(|e| e.to_string())?;
        for e in read.flatten() {
            let p = e.path();
            let Ok(md) = e.metadata() else { continue };
            let mtime_ms = (harvest_core::mtime_ns(&md) / 1_000_000) as i64;
            if md.is_dir() {
                let size = dir_size(&p);
                total += size;
                entries.push(DirEntry {
                    name: e.file_name().to_string_lossy().to_string(),
                    path: p.display().to_string(),
                    size,
                    is_dir: true,
                    ext: String::new(),
                    mtime_ms,
                });
            } else if md.is_file() {
                total += md.len();
                let ext = p
                    .extension()
                    .map(|x| x.to_string_lossy().to_lowercase())
                    .unwrap_or_default();
                entries.push(DirEntry {
                    name: e.file_name().to_string_lossy().to_string(),
                    path: p.display().to_string(),
                    size: md.len(),
                    is_dir: false,
                    ext,
                    mtime_ms,
                });
            }
        }
        entries.sort_by(|a, b| b.size.cmp(&a.size));
        Ok(DirListing { path, total, entries })
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

    // Fresh cancel flag for this run.
    let flag = cancel.0.clone();
    flag.store(false, Ordering::Release);

    std::thread::spawn(move || {
        // Keep the machine awake for the duration of the transfer (best-effort).
        let _awake = keepawake::Builder::default()
            .display(false)
            .idle(true)
            .sleep(true)
            .reason("Harvesting media")
            .app_name("Harvest")
            .create()
            .ok();

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
                let body = if outcome.cancelled {
                    format!("Cancelled — {} files copied", outcome.copied)
                } else if outcome.success() {
                    format!("{} copied, {} already present", outcome.copied, outcome.skipped)
                } else {
                    format!("Finished with {} problem(s)", outcome.errors.len() + outcome.verify_failures.len())
                };
                let _ = app.notification().builder().title("Harvest").body(body).show();
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
        .manage(Cancel::default())
        .invoke_handler(tauri::generate_handler![
            list_drives,
            inspect_path,
            scan_dir,
            plan_harvest,
            start_harvest,
            verify_harvest,
            eject_drive,
            cancel_harvest,
            list_history,
            clear_history,
            list_presets,
            save_preset,
            delete_preset
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
