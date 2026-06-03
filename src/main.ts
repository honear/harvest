import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { open } from "@tauri-apps/plugin-dialog";
import { openPath } from "@tauri-apps/plugin-opener";

// ---- types ----------------------------------------------------------------

interface PathInfo {
  files: number;
  bytes: number;
  freeSpace: number;
  driveTotal: number;
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
}
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

function renderColumn(role: "source" | "dest") {
  const items = role === "source" ? sources : destinations;
  const el = $(role === "source" ? "source-list" : "dest-list");
  el.innerHTML = "";
  if (items.length === 0) {
    const li = document.createElement("li");
    li.className = "drop-hint muted";
    li.textContent = "Nothing here yet — add a folder below.";
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
      rm.onclick = () => {
        items.splice(i, 1);
        renderColumn(role);
        refreshActionState();
      };
      li.append(info, rm);
      el.appendChild(li);
    });
  }
  renderColumnInfo(role);
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

function clearAll() {
  sources = [];
  destinations = [];
  excludePaths = [];
  renderColumn("source");
  renderColumn("dest");
  renderExclusions();
  setStatus("Cleared. Add a source and a destination to begin.");
  refreshActionState();
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
    manifest: null as string | null, journal: "", cancelled: false,
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
      manifestPath: agg.manifest, journalPath: agg.journal, cancelled: agg.cancelled,
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

function showResult(d: Done) {
  ($("result-overlay") as HTMLElement).hidden = false;
  $("result-title").textContent = d.cancelled
    ? "Cancelled"
    : d.success
      ? "✓ Harvest complete"
      : "✗ Finished with problems";
  const summary = $("result-summary");
  summary.innerHTML = d.success
    ? `${d.copied} copied, ${d.skipped} already present — <strong>${humanBytes(d.copiedBytes)}</strong> verified across ${destinations.length} destination(s).`
    : d.cancelled
      ? `Stopped after ${d.copied} file(s) copied. Re-run with Resume to continue.`
      : `${d.errors.length} error(s), ${d.verifyFailures.length} verification failure(s).`;
  const detail: string[] = [];
  if (d.manifestPath) detail.push(`Manifest: ${d.manifestPath}`);
  if (d.errors.length) detail.push("", "Errors:", ...d.errors.map((e) => "  " + e));
  if (d.verifyFailures.length) detail.push("", "Verification failed:", ...d.verifyFailures.map((e) => "  " + e));
  if (!d.success && d.journalPath) detail.push("", `Journal: ${d.journalPath}`);
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
  renderColumn("source");
  renderColumn("dest");
  renderExclusions();
  refreshActionState();
  await refreshTransfers();

  $("add-source").onclick = async () => {
    const f = await open({ directory: true, multiple: false });
    if (typeof f === "string") addPath("source", f);
  };
  $("add-dest").onclick = async () => {
    const f = await open({ directory: true, multiple: false });
    if (typeof f === "string") addPath("dest", f);
  };
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
  $("open-options").onclick = () => toggleOverlay("options-overlay", true);
  $("close-options").onclick = () => toggleOverlay("options-overlay", false);
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
  $("menu-pop").addEventListener("click", (e) => e.stopPropagation());
  $("menu-pop").querySelectorAll<HTMLButtonElement>("button").forEach((b) => {
    b.onclick = () => {
      toggleMenu(false);
      switch (b.dataset.act) {
        case "new": clearAll(); break;
        case "save": saveTransfer(); break;
        case "options": toggleOverlay("options-overlay", true); break;
        case "manifests": openDestinationFolder(); break;
        case "history": openHistory(); break;
        case "about": toggleOverlay("about-overlay", true); break;
      }
    };
  });

  for (const id of ["options-overlay", "result-overlay", "about-overlay", "plan-overlay", "history-overlay"]) {
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
        manifestPath: null, journalPath: "", cancelled: false,
      });
    }
  });
});
