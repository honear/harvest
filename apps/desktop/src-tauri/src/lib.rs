//! Harvest desktop backend.
//!
//! Thin Tauri command layer over `harvest_core`: it builds a `HarvestConfig`
//! from the UI's request, runs the harvest on a background thread, and streams
//! progress to the front end as events. Presets are stored as JSON in the app
//! config directory.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};

use harvest_core::{run_harvest, Filter, HarvestConfig, HarvestEvent, HashAlgo};

/// Copy request sent from the UI.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CopyRequest {
    pub source: String,
    pub dests: Vec<String>,
    pub hash: String,
    pub verify: bool,
    pub resume: bool,
    pub include_ext: Option<String>,
    pub exclude_ext: Option<String>,
    pub min_size: Option<String>,
    pub max_size: Option<String>,
    pub newer_than: Option<String>,
    pub older_than: Option<String>,
    pub dest_template: Option<String>,
    pub project: Option<String>,
    pub write_manifest: bool,
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
}

/// A saved set of options (everything except the source/destinations the user
/// picks per run).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Preset {
    pub name: String,
    pub hash: String,
    pub verify: bool,
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
    let filter = Filter::build(
        req.include_ext.as_deref(),
        req.exclude_ext.as_deref(),
        req.min_size.as_deref(),
        req.max_size.as_deref(),
        req.newer_than.as_deref(),
        req.older_than.as_deref(),
    )?;
    Ok(HarvestConfig {
        source: PathBuf::from(req.source),
        dests: req.dests.into_iter().map(PathBuf::from).collect(),
        algo,
        verify: req.verify,
        resume: req.resume,
        filter,
        dest_template: req.dest_template.filter(|s| !s.trim().is_empty()),
        project: req.project.unwrap_or_default(),
        write_manifest: req.write_manifest,
        journal_path: None,
        manifest_path: None,
    })
}

/// Start a harvest. Returns immediately; progress arrives via the
/// `harvest:planned`, `harvest:progress`, `harvest:done`, and `harvest:failed`
/// events.
#[tauri::command]
fn start_harvest(app: AppHandle, req: CopyRequest) -> Result<(), String> {
    let cfg = build_config(req).map_err(|e| format!("{e:#}"))?;

    std::thread::spawn(move || {
        let emitter = app.clone();
        let result = run_harvest(&cfg, move |event| match event {
            HarvestEvent::Planned { total_scanned, kept, to_copy, skipped, copy_bytes } => {
                let _ = emitter.emit(
                    "harvest:planned",
                    PlannedPayload { total_scanned, kept, to_copy, skipped, copy_bytes },
                );
            }
            HarvestEvent::FileDone { rel, dest, bytes, done_files, done_bytes, ok } => {
                let _ = emitter.emit(
                    "harvest:progress",
                    ProgressPayload { rel, dest, bytes, done_files, done_bytes, ok },
                );
            }
        });

        match result {
            Ok(outcome) => {
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
        .invoke_handler(tauri::generate_handler![
            start_harvest,
            list_presets,
            save_preset,
            delete_preset
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
