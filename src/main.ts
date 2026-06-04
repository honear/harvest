// Self-hosted fonts (bundled into the app — no network needed at runtime).
import "@fontsource/inter/400.css";
import "@fontsource/inter/500.css";
import "@fontsource/inter/600.css";
import "@fontsource/inter/700.css";
import "@fontsource/jetbrains-mono/400.css";
import "@fontsource/jetbrains-mono/500.css";
import "@fontsource/jetbrains-mono/700.css";

import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getVersion } from "@tauri-apps/api/app";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { open } from "@tauri-apps/plugin-dialog";
import { openPath } from "@tauri-apps/plugin-opener";

// ---- types ----------------------------------------------------------------

interface PathInfo {
  files: number;
  bytes: number;
  freeSpace: number;
  driveTotal: number;
  removable: boolean;
}
interface DirEntry {
  name: string;
  path: string;
  size: number;
  isDir: boolean;
  ext: string;
  mtimeMs: number;
}
interface DirListing {
  path: string;
  total: number;
  entries: DirEntry[];
}
interface PlanResult {
  total: number;
  new: number;
  present: number;
  conflict: number;
  copyBytes: number;
  destFree: number;
  fits: boolean;
}
interface Planned {
  copyBytes: number;
}
interface Progress {
  rel: string;
  doneFiles: number;
  doneBytes: number;
}
interface Done {
  success: boolean;
  copied: number;
  skipped: number;
  copiedBytes: number;
  verifyFailures: string[];
  errors: string[];
  manifestPath: string | null;
  journalPath: string;
  cancelled: boolean;
  ejected: boolean;
}
interface Settings {
  defaultHash: string;
  defaultVerify: boolean;
  defaultSkipExisting: boolean;
  defaultWriteManifest: boolean;
  defaultExcludeExt: string;
  confirmBeforeHarvest: boolean;
  notify: boolean;
  autoEject: boolean;
  keepAwake: boolean;
}
const DEFAULT_SETTINGS: Settings = {
  defaultHash: "xxh64",
  defaultVerify: true,
  defaultSkipExisting: true,
  defaultWriteManifest: true,
  defaultExcludeExt: "",
  confirmBeforeHarvest: true,
  notify: true,
  autoEject: false,
  keepAwake: true,
};
interface HistoryEntry {
  when: number;
  source: string;
  dests: string[];
  copied: number;
  skipped: number;
  bytes: number;
  success: boolean;
  cancelled: boolean;
}
interface Preset {
  name: string;
  sources: string[];
  dests: string[];
  hash: string;
  verify: boolean;
  skipExisting: boolean;
  includeExt?: string | null;
  excludeExt?: string | null;
  minSize?: string | null;
  maxSize?: string | null;
  newerThan?: string | null;
  olderThan?: string | null;
  excludePaths?: string[];
  ownerOnly?: boolean;
  destTemplate?: string | null;
  project?: string | null;
  writeManifest: boolean;
}

// ---- DOM helpers ----------------------------------------------------------

const $ = <T extends HTMLElement = HTMLElement>(id: string) =>
  document.getElementById(id) as T;
const val = (id: string) => ($(id) as HTMLInputElement).value.trim();
const setVal = (id: string, v: string) => (($(id) as HTMLInputElement).value = v);
const checked = (id: string) => ($(id) as HTMLInputElement).checked;
const setChecked = (id: string, v: boolean) => (($(id) as HTMLInputElement).checked = v);
const orNull = (s: string) => (s.length ? s : null);

function humanBytes(n: number): string {
  const units = ["B", "KB", "MB", "GB", "TB", "PB"];
  let size = n;
  let u = 0;
  while (size >= 1024 && u < units.length - 1) {
    size /= 1024;
    u++;
  }
  return u === 0 ? `${n} B` : `${size.toFixed(size < 10 ? 2 : 0)} ${units[u]}`;
}
function basename(p: string): string {
  const parts = p.replace(/[\\/]+$/, "").split(/[\\/]/);
  return parts[parts.length - 1] || p;
}

// ---- model ----------------------------------------------------------------

let sources: string[] = [];
let destinations: string[] = [];
let excludePaths: string[] = [];
const infoCache: Record<string, PathInfo> = {};

let settings: Settings = { ...DEFAULT_SETTINGS };

// Sow / Survey visualizer state
let sowSource = "";
let sowPath = "";
let sowListing: DirListing | null = null;
let sowMode: "sow" | "survey" = "sow";

function renderColumn(role: "source" | "dest") {
  const items = role === "source" ? sources : destinations;
  const el = $(role === "source" ? "source-list" : "dest-list");
  el.innerHTML = "";
  if (items.length === 0) {
    const li = document.createElement("li");
    li.className = "drop-add-big";
    li.setAttribute("role", "button");
    li.tabIndex = 0;
    li.innerHTML = `<div class="plus"><svg width="26" height="26" viewBox="0 0 26 26" aria-hidden="true"><path d="M13 4.5 V21.5 M4.5 13 H21.5" stroke="currentColor" stroke-width="2.4" stroke-linecap="round"/></svg></div><div class="label">Add a ${
      role === "source" ? "source" : "destination"
    }</div>`;
    const act = () => pickAndAdd(role);
    li.onclick = act;
    li.onkeydown = (e) => {
      if (e.key === "Enter" || e.key === " ") {
        e.preventDefault();
        act();
      }
    };
    el.appendChild(li);
  } else {
    items.forEach((path, i) => {
      const li = document.createElement("li");
      li.className = "drop-item";
      const info = document.createElement("div");
      info.className = "drop-item-info";
      const title = document.createElement("div");
      title.className = "drop-item-name";
      title.textContent = basename(path);
      const sub = document.createElement("div");
      sub.className = "drop-item-path muted";
      sub.textContent = path;
      info.append(title, sub);
      const ci = infoCache[path];
      if (ci) {
        const stats = document.createElement("div");
        stats.className = "drop-item-stats";
        stats.textContent = `${ci.files.toLocaleString()} files · ${humanBytes(ci.bytes)}`;
        info.appendChild(stats);
      }
      const rm = document.createElement("button");
      rm.className = "ghost danger remove";
      rm.textContent = "✕";
      rm.setAttribute("aria-label", `Remove ${basename(path)}`);
      rm.title = "Remove";
      rm.onclick = () => {
        items.splice(i, 1);
        renderColumn(role);
        refreshActionState();
      };
      li.append(info, rm);
      el.appendChild(li);
    });
    const add = document.createElement("li");
    add.className = "drop-add";
    add.setAttribute("role", "button");
    add.tabIndex = 0;
    add.textContent = "+  Add folder";
    const act = () => pickAndAdd(role);
    add.onclick = act;
    add.onkeydown = (e) => {
      if (e.key === "Enter" || e.key === " ") {
        e.preventDefault();
        act();
      }
    };
    el.appendChild(add);
  }
  renderColumnInfo(role);
}

async function pickAndAdd(role: "source" | "dest") {
  const f = await open({ directory: true, multiple: false });
  if (typeof f === "string") addPath(role, f);
}

function renderColumnInfo(role: "source" | "dest") {
  const items = role === "source" ? sources : destinations;
  const el = $(role === "source" ? "source-info" : "dest-info");
  if (items.length === 0) {
    el.textContent = "";
    return;
  }
  let files = 0;
  let bytes = 0;
  let known = 0;
  for (const p of items) {
    const ci = infoCache[p];
    if (ci) {
      files += ci.files;
      bytes += ci.bytes;
      known++;
    }
  }
  let txt = known < items.length ? "measuring…" : `${files.toLocaleString()} files · ${humanBytes(bytes)}`;
  if (role === "dest" && items.length) {
    const ci = infoCache[items[0]];
    if (ci && ci.driveTotal) txt += ` · ${humanBytes(ci.freeSpace)} free`;
  }
  el.textContent = txt;
}

async function inspect(path: string, role: "source" | "dest") {
  try {
    infoCache[path] = await invoke<PathInfo>("inspect_path", { path });
    renderColumn(role);
  } catch {
    /* preview/no-tauri */
  }
}

function addPath(role: "source" | "dest", path: string) {
  const items = role === "source" ? sources : destinations;
  if (!items.includes(path)) {
    items.push(path);
    renderColumn(role);
    refreshActionState();
    inspect(path, role);
  }
}

// ---- exclusions -----------------------------------------------------------

function renderExclusions() {
  const el = $("exclude-list");
  el.innerHTML = "";
  excludePaths.forEach((p, i) => {
    const li = document.createElement("li");
    const span = document.createElement("span");
    span.textContent = p;
    const rm = document.createElement("button");
    rm.className = "ghost danger remove";
    rm.textContent = "✕";
    rm.setAttribute("aria-label", `Remove exclusion ${p}`);
    rm.onclick = () => {
      excludePaths.splice(i, 1);
      renderExclusions();
    };
    li.append(span, rm);
    el.appendChild(li);
  });
}
function addExclusion(p: string) {
  if (p && !excludePaths.includes(p)) {
    excludePaths.push(p);
    renderExclusions();
  }
}

function isExcluded(path: string): boolean {
  return excludePaths.some((ex) => {
    if (path === ex) return true;
    const e = ex.replace(/[\\/]+$/, "");
    return path.startsWith(e + "\\") || path.startsWith(e + "/");
  });
}
function toggleExclude(path: string) {
  const i = excludePaths.indexOf(path);
  if (i >= 0) excludePaths.splice(i, 1);
  else excludePaths.push(path);
  renderExclusions();
  renderTreemap();
}

// ---- Sow visualizer (squarified treemap) ----------------------------------

interface Rect {
  x: number;
  y: number;
  w: number;
  h: number;
}
interface Placed {
  e: DirEntry;
  x: number;
  y: number;
  w: number;
  h: number;
}

function squarify(items: { area: number; e: DirEntry }[], rect: Rect): Placed[] {
  const out: Placed[] = [];
  let r = { ...rect };
  const remaining = items.slice();
  let row: { area: number; e: DirEntry }[] = [];

  const worst = (rw: typeof row, len: number) => {
    const sum = rw.reduce((s, i) => s + i.area, 0);
    const max = Math.max(...rw.map((i) => i.area));
    const min = Math.min(...rw.map((i) => i.area));
    return Math.max((len * len * max) / (sum * sum), (sum * sum) / (len * len * min));
  };
  const layoutRow = (rw: typeof row) => {
    const sum = rw.reduce((s, i) => s + i.area, 0);
    if (r.w >= r.h) {
      const stripW = sum / r.h;
      let y = r.y;
      for (const it of rw) {
        const hh = it.area / stripW;
        out.push({ e: it.e, x: r.x, y, w: stripW, h: hh });
        y += hh;
      }
      r = { x: r.x + stripW, y: r.y, w: r.w - stripW, h: r.h };
    } else {
      const stripH = sum / r.w;
      let x = r.x;
      for (const it of rw) {
        const ww = it.area / stripH;
        out.push({ e: it.e, x, y: r.y, w: ww, h: stripH });
        x += ww;
      }
      r = { x: r.x, y: r.y + stripH, w: r.w, h: r.h - stripH };
    }
  };

  while (remaining.length) {
    const len = Math.min(r.w, r.h);
    const next = remaining[0];
    if (row.length === 0 || worst(row, len) >= worst([...row, next], len)) {
      row.push(next);
      remaining.shift();
    } else {
      layoutRow(row);
      row = [];
    }
  }
  if (row.length) layoutRow(row);
  return out;
}

// Mirror the core size parser (binary units) for live treemap filtering.
function parseSizeBytes(s: string): number | null {
  s = s.trim();
  if (!s) return null;
  const m = s.match(/^([\d.]+)\s*([a-zA-Z]*)$/);
  if (!m) return null;
  const n = parseFloat(m[1]);
  const mult: Record<string, number> = {
    "": 1, b: 1, k: 1024, kb: 1024, m: 1024 ** 2, mb: 1024 ** 2,
    g: 1024 ** 3, gb: 1024 ** 3, t: 1024 ** 4, tb: 1024 ** 4,
  };
  const f = mult[m[2].toLowerCase()];
  return f ? n * f : null;
}
function parseDateMs(s: string): number | null {
  const m = s.trim().match(/^(\d{4})-(\d{2})-(\d{2})$/);
  if (!m) return null;
  return Date.UTC(+m[1], +m[2] - 1, +m[3]);
}
function extList(s: string): string[] {
  return s.split(",").map((e) => e.trim().replace(/^\./, "").toLowerCase()).filter(Boolean);
}

/// True if the current Options filters would drop this file (not for dirs).
function filterExcludes(e: DirEntry): boolean {
  if (e.isDir) return false;
  const ext = e.ext.toLowerCase();
  const inc = extList(val("include-ext"));
  const exc = extList(val("exclude-ext"));
  if (exc.includes(ext)) return true;
  if (inc.length && !inc.includes(ext)) return true;
  const min = parseSizeBytes(val("min-size"));
  if (min != null && e.size < min) return true;
  const max = parseSizeBytes(val("max-size"));
  if (max != null && e.size > max) return true;
  const after = parseDateMs(val("newer-than"));
  if (after != null && e.mtimeMs < after) return true;
  const before = parseDateMs(val("older-than"));
  if (before != null && e.mtimeMs >= before) return true;
  return false;
}

function extColor(ext: string): string {
  const video = ["mov", "mp4", "mxf", "avi", "mts", "m4v", "braw", "r3d", "mkv", "wmv"];
  const audio = ["wav", "aif", "aiff", "mp3", "flac", "m4a", "aac"];
  const image = ["jpg", "jpeg", "png", "cr3", "cr2", "arw", "dng", "nef", "tif", "tiff", "heic", "raf", "gpr", "gif"];
  if (video.includes(ext)) return "#3b9eff";
  if (audio.includes(ext)) return "#11ff99";
  if (image.includes(ext)) return "#ff801f";
  if (ext) return "#ffc53d";
  return "#7b8186";
}

function renderCrumbs() {
  const el = $("sow-crumbs");
  el.innerHTML = "";
  const add = (label: string, path: string) => {
    const b = document.createElement("button");
    b.textContent = label;
    b.onclick = () => sowOpen(path);
    el.appendChild(b);
  };
  add(basename(sowSource), sowSource);
  const sep = sowPath.includes("\\") ? "\\" : "/";
  if (sowPath !== sowSource && sowPath.startsWith(sowSource)) {
    const tail = sowPath.slice(sowSource.length);
    const parts = tail.split(/[\\/]+/).filter(Boolean);
    let acc = sowSource.replace(/[\\/]+$/, "");
    for (const part of parts) {
      acc = acc + sep + part;
      const s = document.createElement("span");
      s.className = "sep";
      s.textContent = "›";
      el.appendChild(s);
      add(part, acc);
    }
  }
}

function renderLegend() {
  const el = $("sow-legend");
  const items: [string, string][] = [
    ["Video", "#3b9eff"],
    ["Audio", "#11ff99"],
    ["Image/RAW", "#ff801f"],
    ["Other", "#ffc53d"],
    ["Folder", "var(--surface-elevated)"],
  ];
  let html = items.map(([l, c]) => `<span><i style="background:${c}"></i>${l}</span>`).join("");
  if (sowMode === "sow") {
    html += `<span><i style="background:var(--danger)"></i>Excluded</span>`;
    html += `<span><i style="background:var(--survey);opacity:.5"></i>Filtered (Options)</span>`;
  }
  el.innerHTML = html;
}

function renderTreemap() {
  const tm = $("treemap");
  tm.innerHTML = "";
  if (!sowListing) return;
  const W = tm.clientWidth || 600;
  const H = tm.clientHeight || 360;
  const entries = sowListing.entries.filter((e) => e.size > 0);
  if (entries.length === 0) {
    tm.innerHTML = '<div class="sow-hint">This folder is empty.</div>';
    return;
  }
  const total = entries.reduce((s, e) => s + e.size, 0) || 1;
  const area = W * H;
  const items = entries.map((e) => ({ area: (e.size / total) * area, e }));
  const placed = squarify(items, { x: 0, y: 0, w: W, h: H });
  const survey = sowMode === "survey";
  for (const p of placed) {
    const e = p.e;
    const manual = isExcluded(e.path);
    const filtered = !manual && !survey && filterExcludes(e);
    const div = document.createElement("div");
    div.className =
      "tile" + (e.isDir ? " dir" : "") + (manual ? " excluded" : "") + (filtered ? " filtered" : "");
    if (p.w < 32 || p.h < 18) div.classList.add("tiny");
    div.style.left = `${p.x}px`;
    div.style.top = `${p.y}px`;
    div.style.width = `${Math.max(0, p.w - 2)}px`;
    div.style.height = `${Math.max(0, p.h - 2)}px`;
    div.style.background = e.isDir ? "var(--surface-elevated)" : extColor(e.ext);
    div.innerHTML = `<div class="tile-name">${e.name}${e.isDir ? "/" : ""}</div><div class="tile-size">${humanBytes(e.size)}</div>`;
    const why = filtered ? " · excluded by Options filter" : e.isDir ? " · click to open" : survey ? "" : " · click to exclude";
    div.title = `${e.path}\n${humanBytes(e.size)}${why}`;
    div.onclick = () => {
      if (e.isDir) sowOpen(e.path);
      else if (!survey && !filtered) toggleExclude(e.path);
    };
    // manual exclude/include corner button — only for files not already
    // dropped by a filter, when building a transfer (Sow mode).
    if (!survey && !filtered) {
      const x = document.createElement("button");
      x.className = "tile-x";
      x.textContent = manual ? "+" : "✕";
      x.title = manual ? "Include in transfer" : "Exclude from transfer";
      x.setAttribute("aria-label", `${manual ? "Include" : "Exclude"} ${e.name}`);
      x.onclick = (ev) => {
        ev.stopPropagation();
        toggleExclude(e.path);
      };
      div.appendChild(x);
    }
    tm.appendChild(div);
  }
  renderLegend();
}

async function sowOpen(path: string) {
  sowPath = path;
  renderCrumbs();
  const tm = $("treemap");
  tm.innerHTML = '<div class="sow-hint">Scanning…</div>';
  try {
    sowListing = await invoke<DirListing>("scan_dir", { path });
  } catch (e) {
    tm.innerHTML = `<div class="sow-hint">Could not scan: ${e}</div>`;
    return;
  }
  renderTreemap();
}

function openVisualizer(root: string, mode: "sow" | "survey") {
  sowMode = mode;
  sowSource = root;
  const panel = $("center-panel");
  panel.classList.remove("sow", "survey");
  panel.classList.add(mode);
  $("center-title").textContent = `${mode === "sow" ? "Sow" : "Survey"} — ${basename(root)}`;
  ($("transfer-list") as HTMLElement).hidden = true;
  ($("sow-view") as HTMLElement).hidden = false;
  ($("new-transfer") as HTMLElement).hidden = true;
  ($("sow-exit") as HTMLElement).hidden = false;
  setStatus(
    mode === "sow"
      ? "Click folders to explore; click files (or ✕) to exclude them from the transfer."
      : "Surveying disk usage. Click folders to explore.",
  );
  sowOpen(root);
}

function enterSow() {
  if (sources.length === 0) {
    setStatus("Add a source folder to explore.", "error");
    return;
  }
  openVisualizer(sources[0], "sow");
}

async function enterSurvey() {
  const f = await open({ directory: true, multiple: false });
  if (typeof f === "string") openVisualizer(f, "survey");
}

function exitSow() {
  $("center-panel").classList.remove("sow", "survey");
  $("center-title").textContent = "Saved Transfers";
  ($("sow-view") as HTMLElement).hidden = true;
  ($("transfer-list") as HTMLElement).hidden = false;
  ($("new-transfer") as HTMLElement).hidden = false;
  ($("sow-exit") as HTMLElement).hidden = true;
}

// ---- status / progress ----------------------------------------------------

function setStatus(msg: string, kind: "" | "error" | "ok" = "") {
  const el = $("status");
  el.textContent = msg;
  el.className = `status ${kind}`;
}
function showProgress(show: boolean) {
  ($("progress-track") as HTMLElement).hidden = !show;
}
function updateProgress(done: number, total: number, current: string) {
  const pct = total > 0 ? Math.min(100, (done / total) * 100) : 0;
  ($("progress-fill") as HTMLElement).style.width = `${pct}%`;
  $("progress-current").textContent = current;
}
function refreshActionState() {
  const ready = sources.length > 0 && destinations.length > 0 && !running;
  ($("start") as HTMLButtonElement).disabled = !ready;
  ($("sow-btn") as HTMLButtonElement).disabled = sources.length === 0 || running;
  ($("cancel") as HTMLElement).hidden = !running;
  ($("start") as HTMLElement).hidden = running;
}

// ---- options gathering ----------------------------------------------------

function gatherCommon() {
  return {
    hash: val("hash") || "xxh64",
    verify: checked("verify"),
    skipExisting: checked("skip-existing"),
    includeExt: orNull(val("include-ext")),
    excludeExt: orNull(val("exclude-ext")),
    minSize: orNull(val("min-size")),
    maxSize: orNull(val("max-size")),
    newerThan: orNull(val("newer-than")),
    olderThan: orNull(val("older-than")),
    excludePaths: [...excludePaths],
    ownerOnly: checked("owner-only"),
    destTemplate: orNull(val("dest-template")),
    project: orNull(val("project")),
    writeManifest: checked("manifest"),
  };
}

function applyOptions(p: Preset) {
  setVal("hash", p.hash || "xxh64");
  setChecked("verify", p.verify);
  setChecked("skip-existing", p.skipExisting);
  setChecked("manifest", p.writeManifest);
  setVal("include-ext", p.includeExt ?? "");
  setVal("exclude-ext", p.excludeExt ?? "");
  setVal("min-size", p.minSize ?? "");
  setVal("max-size", p.maxSize ?? "");
  setVal("newer-than", p.newerThan ?? "");
  setVal("older-than", p.olderThan ?? "");
  setVal("dest-template", p.destTemplate ?? "");
  setVal("project", p.project ?? "");
  setChecked("owner-only", !!p.ownerOnly);
  excludePaths = [...(p.excludePaths ?? [])];
  renderExclusions();
}

function loadTransfer(p: Preset) {
  sources = [...(p.sources ?? [])];
  destinations = [...(p.dests ?? [])];
  renderColumn("source");
  renderColumn("dest");
  applyOptions(p);
  refreshActionState();
  setStatus(`Loaded “${p.name}”.`, "ok");
  sources.forEach((s) => inspect(s, "source"));
  destinations.forEach((d) => inspect(d, "dest"));
}

/// Seed the per-transfer Options form from the global Settings defaults.
function applyDefaultsToOptions() {
  setVal("hash", settings.defaultHash || "xxh64");
  setChecked("verify", settings.defaultVerify);
  setChecked("skip-existing", settings.defaultSkipExisting);
  setChecked("manifest", settings.defaultWriteManifest);
  setVal("exclude-ext", settings.defaultExcludeExt ?? "");
  setVal("include-ext", "");
  setVal("min-size", "");
  setVal("max-size", "");
  setVal("newer-than", "");
  setVal("older-than", "");
  setVal("dest-template", "");
  setVal("project", "");
  setChecked("resume", false);
  setChecked("owner-only", false);
}

function clearAll() {
  sources = [];
  destinations = [];
  excludePaths = [];
  applyDefaultsToOptions();
  renderColumn("source");
  renderColumn("dest");
  renderExclusions();
  setStatus("Cleared. Add a source and a destination to begin.");
  refreshActionState();
}

// ---- global settings ------------------------------------------------------

async function loadSettings() {
  try {
    settings = await invoke<Settings>("get_settings");
  } catch {
    settings = { ...DEFAULT_SETTINGS };
  }
}
function fillSettingsForm() {
  setVal("s-default-hash", settings.defaultHash || "xxh64");
  setChecked("s-verify", settings.defaultVerify);
  setChecked("s-skip", settings.defaultSkipExisting);
  setChecked("s-manifest", settings.defaultWriteManifest);
  setVal("s-exclude-ext", settings.defaultExcludeExt ?? "");
  setChecked("s-confirm", settings.confirmBeforeHarvest);
  setChecked("s-notify", settings.notify);
  setChecked("s-eject", settings.autoEject);
  setChecked("s-keepawake", settings.keepAwake);
}
function gatherSettings(): Settings {
  return {
    defaultHash: val("s-default-hash") || "xxh64",
    defaultVerify: checked("s-verify"),
    defaultSkipExisting: checked("s-skip"),
    defaultWriteManifest: checked("s-manifest"),
    defaultExcludeExt: val("s-exclude-ext"),
    confirmBeforeHarvest: checked("s-confirm"),
    notify: checked("s-notify"),
    autoEject: checked("s-eject"),
    keepAwake: checked("s-keepawake"),
  };
}
async function saveSettings() {
  settings = gatherSettings();
  try {
    await invoke("save_settings", { settings });
  } catch (e) {
    setStatus(`Could not save settings: ${e}`, "error");
  }
}

// ---- saved transfers ------------------------------------------------------

async function refreshTransfers() {
  const list = $("transfer-list");
  let presets: Preset[] = [];
  try {
    presets = await invoke<Preset[]>("list_presets");
  } catch {
    presets = [];
  }
  list.innerHTML = "";
  if (presets.length === 0) {
    list.innerHTML =
      '<div class="muted transfer-empty">No saved transfers yet.<br/>Build one on the sides, then “Save Transfer”.</div>';
    return;
  }
  for (const p of presets) {
    const card = document.createElement("div");
    card.className = "transfer-card";
    const srcLabel = (p.sources ?? []).map(basename).join(", ") || "—";
    const dstLabel =
      (p.dests ?? []).length > 1
        ? `${basename(p.dests[0])} +${p.dests.length - 1}`
        : (p.dests ?? []).map(basename).join(", ") || "—";
    const tmpl = p.destTemplate ? ` · 🗂️ ${p.destTemplate}` : "";
    const head = document.createElement("div");
    head.className = "transfer-head";
    head.innerHTML = `<div class="transfer-name">${p.name}</div>
      <div class="transfer-route muted">${srcLabel} → ${dstLabel}</div>`;
    const meta = document.createElement("div");
    meta.className = "transfer-meta muted";
    meta.textContent = `${p.hash}${p.verify ? " · verify" : ""}${p.skipExisting ? " · skip-existing" : ""}${tmpl}`;
    const actions = document.createElement("div");
    actions.className = "transfer-actions";
    const runBtn = document.createElement("button");
    runBtn.className = "primary small";
    runBtn.textContent = "▶ Run";
    runBtn.onclick = () => {
      loadTransfer(p);
      onHarvest();
    };
    const loadBtn = document.createElement("button");
    loadBtn.className = "ghost small";
    loadBtn.textContent = "Load";
    loadBtn.onclick = () => loadTransfer(p);
    const delBtn = document.createElement("button");
    delBtn.className = "ghost small danger";
    delBtn.textContent = "Delete";
    delBtn.onclick = async () => {
      if (!confirm(`Delete transfer “${p.name}”?`)) return;
      await invoke("delete_preset", { name: p.name });
      await refreshTransfers();
    };
    actions.append(runBtn, loadBtn, delBtn);
    card.append(head, meta, actions);
    list.appendChild(card);
  }
}

async function saveTransfer() {
  if (sources.length === 0 && destinations.length === 0) {
    setStatus("Add a source or destination before saving a transfer.", "error");
    return;
  }
  const name = prompt("Save this transfer as:");
  if (!name) return;
  const preset: Preset = { name, sources: [...sources], dests: [...destinations], ...gatherCommon() };
  try {
    await invoke("save_preset", { preset });
    await refreshTransfers();
    setStatus(`Saved transfer “${name}”.`, "ok");
  } catch (e) {
    setStatus(`Could not save: ${e}`, "error");
  }
}

// ---- pre-flight compare ---------------------------------------------------

async function onHarvest() {
  if (running) return;
  if (sources.length === 0 || destinations.length === 0) {
    setStatus("Add at least one source and one destination.", "error");
    return;
  }
  if (!settings.confirmBeforeHarvest) {
    runQueue();
    return;
  }
  // Aggregate a plan across all sources.
  const agg = { total: 0, new: 0, present: 0, conflict: 0, copyBytes: 0, destFree: 0 };
  try {
    for (const s of sources) {
      const req = { source: s, dests: destinations, resume: checked("resume"), ...gatherCommon() };
      const p = await invoke<PlanResult>("plan_harvest", { req });
      agg.total += p.total;
      agg.new += p.new;
      agg.present += p.present;
      agg.conflict += p.conflict;
      agg.copyBytes += p.copyBytes;
      agg.destFree = p.destFree;
    }
  } catch (e) {
    // If planning fails, fall back to running directly.
    setStatus(`Compare skipped: ${e}`);
    runQueue();
    return;
  }

  const fits = agg.destFree === 0 || agg.destFree >= agg.copyBytes;
  const row = (dot: string, label: string, v: string) =>
    `<div class="plan-row"><span class="label"><span class="dot ${dot}"></span>${label}</span><span class="v">${v}</span></div>`;
  const plain = (label: string, v: string) =>
    `<div class="plan-row"><span class="label">${label}</span><span class="v">${v}</span></div>`;
  $("plan-body").innerHTML =
    row("new", "New files", String(agg.new)) +
    row("present", "Already present", String(agg.present)) +
    row("conflict", "Differ (will overwrite)", String(agg.conflict)) +
    plain("To copy", humanBytes(agg.copyBytes)) +
    plain("Destination free", agg.destFree ? humanBytes(agg.destFree) : "—");
  const warn = $("plan-warn");
  if (!fits) {
    warn.hidden = false;
    warn.textContent = `Not enough free space: need ${humanBytes(agg.copyBytes)}, only ${humanBytes(agg.destFree)} free.`;
  } else {
    warn.hidden = true;
  }
  ($("plan-overlay") as HTMLElement).hidden = false;
}

// ---- running the harvest (sequential queue over sources) ------------------

let running = false;
let planTotal = 1;
let resolveCurrent: ((d: Done) => void) | null = null;

function harvestOne(source: string): Promise<Done> {
  const req = { source, dests: destinations, resume: checked("resume"), ...gatherCommon() };
  return new Promise((resolve, reject) => {
    resolveCurrent = resolve;
    invoke("start_harvest", { req }).catch((e) => {
      resolveCurrent = null;
      reject(e);
    });
  });
}

async function runQueue() {
  if (running) return;
  running = true;
  refreshActionState();
  showProgress(true);
  ($("result-overlay") as HTMLElement).hidden = true;

  const agg = {
    copied: 0, skipped: 0, bytes: 0,
    errors: [] as string[], fails: [] as string[],
    manifest: null as string | null, journal: "", cancelled: false, ejected: false,
  };
  try {
    for (let i = 0; i < sources.length; i++) {
      setStatus(`Harvesting ${basename(sources[i])} (${i + 1}/${sources.length})…`);
      const d = await harvestOne(sources[i]);
      agg.copied += d.copied;
      agg.skipped += d.skipped;
      agg.bytes += d.copiedBytes;
      agg.errors.push(...d.errors);
      agg.fails.push(...d.verifyFailures);
      if (d.manifestPath) agg.manifest = d.manifestPath;
      if (d.ejected) agg.ejected = true;
      agg.journal = d.journalPath;
      if (d.cancelled) {
        agg.cancelled = true;
        break;
      }
    }
    const success = !agg.cancelled && agg.errors.length === 0 && agg.fails.length === 0;
    showResult({
      success, copied: agg.copied, skipped: agg.skipped, copiedBytes: agg.bytes,
      verifyFailures: agg.fails, errors: agg.errors,
      manifestPath: agg.manifest, journalPath: agg.journal, cancelled: agg.cancelled, ejected: agg.ejected,
    });
    setStatus(agg.cancelled ? "Cancelled." : success ? "Done." : "Finished with problems.", success ? "ok" : "error");
  } catch (e) {
    setStatus(`Failed: ${e}`, "error");
  } finally {
    running = false;
    showProgress(false);
    refreshActionState();
  }
}

function verifyOne(source: string): Promise<Done> {
  const req = { source, dests: destinations, resume: false, ...gatherCommon() };
  return new Promise((resolve, reject) => {
    resolveCurrent = resolve;
    invoke("verify_harvest", { req }).catch((e) => {
      resolveCurrent = null;
      reject(e);
    });
  });
}

async function runVerify() {
  if (running) return;
  if (sources.length === 0 || destinations.length === 0) {
    setStatus("Add a source and destination to verify.", "error");
    return;
  }
  running = true;
  refreshActionState();
  showProgress(true);
  ($("result-overlay") as HTMLElement).hidden = true;
  const agg = { ok: 0, bytes: 0, fails: [] as string[], errors: [] as string[], cancelled: false };
  try {
    for (let i = 0; i < sources.length; i++) {
      setStatus(`Verifying ${basename(sources[i])} (${i + 1}/${sources.length})…`);
      const d = await verifyOne(sources[i]);
      agg.ok += d.copied;
      agg.bytes += d.copiedBytes;
      agg.fails.push(...d.verifyFailures);
      agg.errors.push(...d.errors);
      if (d.cancelled) {
        agg.cancelled = true;
        break;
      }
    }
    const success = !agg.cancelled && agg.errors.length === 0 && agg.fails.length === 0;
    showResult(
      {
        success, copied: agg.ok, skipped: 0, copiedBytes: agg.bytes,
        verifyFailures: agg.fails, errors: agg.errors,
        manifestPath: null, journalPath: "", cancelled: agg.cancelled, ejected: false,
      },
      { verify: true },
    );
    setStatus(success ? "Verified." : "Verification found problems.", success ? "ok" : "error");
  } catch (e) {
    setStatus(`Failed: ${e}`, "error");
  } finally {
    running = false;
    showProgress(false);
    refreshActionState();
  }
}

function showResult(d: Done, opts: { verify?: boolean } = {}) {
  ($("result-overlay") as HTMLElement).hidden = false;
  if (opts.verify) {
    $("result-title").textContent = d.success ? "✓ Verification passed" : "✗ Verification failed";
    $("result-summary").innerHTML = d.success
      ? `${d.copied} file(s) verified OK across ${destinations.length} destination(s).`
      : `${d.verifyFailures.length} file(s) did not match the source.`;
  } else {
    $("result-title").textContent = d.cancelled
      ? "Cancelled"
      : d.success
        ? "✓ Harvest complete"
        : "✗ Finished with problems";
    $("result-summary").innerHTML = d.success
      ? `${d.copied} copied, ${d.skipped} already present — <strong>${humanBytes(d.copiedBytes)}</strong> verified across ${destinations.length} destination(s).`
      : d.cancelled
        ? `Stopped after ${d.copied} file(s) copied. Re-run with Resume to continue.`
        : `${d.errors.length} error(s), ${d.verifyFailures.length} verification failure(s).`;
  }

  // Offer to eject any removable source (after a successful copy).
  const eject = $("result-eject");
  eject.innerHTML = "";
  if (d.ejected) {
    eject.innerHTML = `<div class="status ok">⏏ Source ejected — safe to remove.</div>`;
  } else if (d.success && !opts.verify) {
    for (const src of sources) {
      if (infoCache[src]?.removable) {
        const b = document.createElement("button");
        b.textContent = `⏏ Eject ${basename(src)}`;
        b.onclick = async () => {
          b.disabled = true;
          b.textContent = `Ejecting ${basename(src)}…`;
          try {
            await invoke("eject_drive", { path: src });
            b.textContent = `Ejected ${basename(src)} ✓`;
          } catch (e) {
            b.disabled = false;
            b.textContent = `Eject failed — retry ${basename(src)}`;
            setStatus(`Eject failed: ${e}`, "error");
          }
        };
        eject.appendChild(b);
      }
    }
  }

  const detail: string[] = [];
  if (d.manifestPath) detail.push(`Manifest: ${d.manifestPath}`);
  if (d.errors.length) detail.push("", "Errors:", ...d.errors.map((e) => "  " + e));
  if (d.verifyFailures.length) detail.push("", opts.verify ? "Did not match:" : "Verification failed:", ...d.verifyFailures.map((e) => "  " + e));
  if (!d.success && !opts.verify && d.journalPath) detail.push("", `Journal: ${d.journalPath}`);
  $("result-detail").textContent = detail.join("\n");
}

// ---- history --------------------------------------------------------------

async function openHistory() {
  const list = $("history-list");
  let entries: HistoryEntry[] = [];
  try {
    entries = await invoke<HistoryEntry[]>("list_history");
  } catch {
    entries = [];
  }
  list.innerHTML = entries.length
    ? ""
    : '<div class="muted" style="text-align:center;padding:30px">No transfers yet.</div>';
  for (const e of entries) {
    const div = document.createElement("div");
    div.className = "history-item";
    const when = new Date(e.when * 1000).toLocaleString();
    const stateClass = e.success ? "history-ok" : "history-bad";
    const state = e.cancelled ? "cancelled" : e.success ? "ok" : "problems";
    div.innerHTML = `<div class="history-when">${when}</div>
      <div class="history-route">${basename(e.source)} → ${e.dests.map(basename).join(", ")}</div>
      <div class="history-stats">${e.copied} copied · ${e.skipped} skipped · ${humanBytes(e.bytes)} · <span class="${stateClass}">${state}</span></div>`;
    list.appendChild(div);
  }
  ($("history-overlay") as HTMLElement).hidden = false;
}

// ---- menu / dialogs -------------------------------------------------------

function toggleOverlay(id: string, show: boolean) {
  ($(id) as HTMLElement).hidden = !show;
}
function toggleMenu(show?: boolean) {
  const m = $("menu-pop");
  m.hidden = show === undefined ? !m.hidden : !show;
}
async function openDestinationFolder() {
  if (destinations.length === 0) return setStatus("No destination to open.", "error");
  try {
    await openPath(destinations[0]);
  } catch (e) {
    setStatus(`Could not open folder: ${e}`, "error");
  }
}

// ---- wire up --------------------------------------------------------------

window.addEventListener("DOMContentLoaded", async () => {
  // Custom window controls (native chrome is disabled).
  try {
    const appWindow = getCurrentWindow();
    $("win-min").onclick = () => appWindow.minimize();
    $("win-max").onclick = () => appWindow.toggleMaximize();
    $("win-close").onclick = () => appWindow.close();
  } catch {
    /* not in Tauri (browser preview) */
  }

  // Show the real app version in the badge + About.
  try {
    const v = await getVersion();
    $("badge").textContent = `v${v} · β`;
    $("about-version").textContent = `Verified media ingest · v${v}`;
  } catch {
    /* not in Tauri (browser preview) */
  }

  await loadSettings();
  applyDefaultsToOptions();
  renderColumn("source");
  renderColumn("dest");
  renderExclusions();
  refreshActionState();
  await refreshTransfers();

  // Auto-save global settings whenever a settings field changes.
  for (const id of ["s-default-hash", "s-verify", "s-skip", "s-manifest", "s-exclude-ext", "s-confirm", "s-notify", "s-eject", "s-keepawake"]) {
    $(id).addEventListener("change", saveSettings);
  }

  $("add-exclude-folder").onclick = async () => {
    const f = await open({ directory: true, multiple: false });
    if (typeof f === "string") addExclusion(f);
  };
  $("add-exclude-file").onclick = async () => {
    const f = await open({ directory: false, multiple: false });
    if (typeof f === "string") addExclusion(f);
  };

  $("start").onclick = onHarvest;
  $("cancel").onclick = () => {
    invoke("cancel_harvest").catch(() => {});
    setStatus("Cancelling…");
  };
  $("plan-confirm").onclick = () => {
    toggleOverlay("plan-overlay", false);
    runQueue();
  };
  $("plan-cancel").onclick = () => toggleOverlay("plan-overlay", false);

  $("save-transfer").onclick = saveTransfer;
  $("new-transfer").onclick = clearAll;
  $("sow-btn").onclick = enterSow;
  $("sow-exit").onclick = exitSow;
  window.addEventListener("resize", () => {
    if (!($("sow-view") as HTMLElement).hidden && sowListing) renderTreemap();
  });
  const reRenderIfSow = () => {
    if (!($("sow-view") as HTMLElement).hidden && sowListing) renderTreemap();
  };
  $("open-options").onclick = () => toggleOverlay("options-overlay", true);
  $("close-options").onclick = () => {
    toggleOverlay("options-overlay", false);
    reRenderIfSow();
  };
  $("close-settings").onclick = async () => {
    await saveSettings();
    toggleOverlay("settings-overlay", false);
  };
  // Live-update the Sow treemap as Options filters change.
  for (const id of ["include-ext", "exclude-ext", "min-size", "max-size", "newer-than", "older-than"]) {
    $(id).addEventListener("input", reRenderIfSow);
  }
  $("close-result").onclick = () => toggleOverlay("result-overlay", false);
  $("close-about").onclick = () => toggleOverlay("about-overlay", false);
  $("close-history").onclick = () => toggleOverlay("history-overlay", false);
  $("clear-history").onclick = async () => {
    await invoke("clear_history").catch(() => {});
    openHistory();
  };

  $("menu-btn").onclick = (e) => {
    e.stopPropagation();
    toggleMenu();
  };
  document.addEventListener("click", () => toggleMenu(false));

  // Escape closes the menu, any open overlay, or exits the Sow/Survey view.
  document.addEventListener("keydown", (e) => {
    if (e.key !== "Escape") return;
    toggleMenu(false);
    const overlays = ["options-overlay", "settings-overlay", "result-overlay", "about-overlay", "plan-overlay", "history-overlay"];
    const open = overlays.find((id) => !($(id) as HTMLElement).hidden);
    if (open) {
      if (open === "settings-overlay") saveSettings();
      toggleOverlay(open, false);
    } else if (!($("sow-view") as HTMLElement).hidden) {
      exitSow();
    }
  });
  $("menu-pop").addEventListener("click", (e) => e.stopPropagation());
  $("menu-pop").querySelectorAll<HTMLButtonElement>("button").forEach((b) => {
    b.onclick = () => {
      toggleMenu(false);
      switch (b.dataset.act) {
        case "new": clearAll(); break;
        case "save": saveTransfer(); break;
        case "options": toggleOverlay("options-overlay", true); break;
        case "settings": fillSettingsForm(); toggleOverlay("settings-overlay", true); break;
        case "survey": enterSurvey(); break;
        case "verify": runVerify(); break;
        case "manifests": openDestinationFolder(); break;
        case "history": openHistory(); break;
        case "about": toggleOverlay("about-overlay", true); break;
      }
    };
  });

  for (const id of ["options-overlay", "settings-overlay", "result-overlay", "about-overlay", "plan-overlay", "history-overlay"]) {
    $(id).addEventListener("click", (e) => {
      if (e.target === $(id)) toggleOverlay(id, false);
    });
  }

  await listen<Planned>("harvest:planned", (e) => {
    planTotal = e.payload.copyBytes || 1;
    updateProgress(0, planTotal, "");
  });
  await listen<Progress>("harvest:progress", (e) => {
    const p = e.payload;
    updateProgress(p.doneBytes, planTotal, `${p.doneFiles} · ${p.rel}`);
  });
  await listen<Done>("harvest:done", (e) => {
    if (resolveCurrent) {
      const r = resolveCurrent;
      resolveCurrent = null;
      r(e.payload);
    }
  });
  await listen<string>("harvest:failed", (e) => {
    setStatus(`Failed: ${e.payload}`, "error");
    if (resolveCurrent) {
      const r = resolveCurrent;
      resolveCurrent = null;
      r({
        success: false, copied: 0, skipped: 0, copiedBytes: 0,
        verifyFailures: [], errors: [String(e.payload)],
        manifestPath: null, journalPath: "", cancelled: false, ejected: false,
      });
    }
  });
});
