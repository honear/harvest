import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { open } from "@tauri-apps/plugin-dialog";

// ---- types mirroring the Rust payloads ----------------------------------

interface Planned {
  totalScanned: number;
  kept: number;
  toCopy: number;
  skipped: number;
  copyBytes: number;
}
interface Progress {
  rel: string;
  dest: string;
  bytes: number;
  doneFiles: number;
  doneBytes: number;
  ok: boolean;
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
}
interface Preset {
  name: string;
  hash: string;
  verify: boolean;
  includeExt?: string | null;
  excludeExt?: string | null;
  minSize?: string | null;
  maxSize?: string | null;
  newerThan?: string | null;
  olderThan?: string | null;
  destTemplate?: string | null;
  project?: string | null;
  writeManifest: boolean;
}

// ---- small DOM helpers ----------------------------------------------------

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
  return u === 0 ? `${n} B` : `${size.toFixed(2)} ${units[u]}`;
}

// ---- destinations (dynamic list) -----------------------------------------

const dests: string[] = [];

function renderDests() {
  const list = $("dest-list");
  list.innerHTML = "";
  if (dests.length === 0) {
    const li = document.createElement("li");
    li.className = "dest-empty muted";
    li.textContent = "No destinations yet — add at least one.";
    list.appendChild(li);
    return;
  }
  dests.forEach((d, i) => {
    const li = document.createElement("li");
    li.className = "dest-item";
    const span = document.createElement("span");
    span.className = "dest-path";
    span.textContent = d;
    const rm = document.createElement("button");
    rm.className = "ghost danger";
    rm.textContent = "Remove";
    rm.onclick = () => {
      dests.splice(i, 1);
      renderDests();
    };
    li.append(span, rm);
    list.appendChild(li);
  });
}

async function pickFolder(): Promise<string | null> {
  const picked = await open({ directory: true, multiple: false });
  return typeof picked === "string" ? picked : null;
}

// ---- status / progress / results -----------------------------------------

function setStatus(msg: string, kind: "" | "error" | "ok" = "") {
  const el = $("status");
  el.textContent = msg;
  el.className = `status ${kind}`;
}

function showProgress(show: boolean) {
  ($("progress-panel") as HTMLElement).hidden = !show;
}

function updateProgress(done: number, total: number, current: string) {
  const pct = total > 0 ? Math.min(100, (done / total) * 100) : 0;
  ($("progress-fill") as HTMLElement).style.width = `${pct}%`;
  $("progress-text").textContent = `${humanBytes(done)} / ${humanBytes(total)}`;
  $("progress-current").textContent = current;
}

function showResult(d: Done) {
  const panel = $("result-panel") as HTMLElement;
  panel.hidden = false;
  panel.classList.toggle("bad", !d.success);
  panel.classList.toggle("good", d.success);
  const summary = $("result-summary");
  if (d.success) {
    summary.innerHTML = `<strong>✓ Harvest complete.</strong> ${d.copied} copied, ${d.skipped} already done — ${humanBytes(d.copiedBytes)} verified.`;
  } else {
    summary.innerHTML = `<strong>✗ Harvest finished with problems.</strong> ${d.errors.length} error(s), ${d.verifyFailures.length} verification failure(s).`;
  }
  const detail: string[] = [];
  if (d.manifestPath) detail.push(`Manifest: ${d.manifestPath}`);
  if (d.errors.length) detail.push("", "Errors:", ...d.errors.map((e) => "  " + e));
  if (d.verifyFailures.length)
    detail.push("", "Verification failed:", ...d.verifyFailures.map((e) => "  " + e));
  if (!d.success) detail.push("", `Re-run with Resume to continue (journal: ${d.journalPath}).`);
  $("result-detail").textContent = detail.join("\n");
}

// ---- gather form into a request / preset ----------------------------------

function gatherCommon() {
  return {
    hash: val("hash") || "xxh64",
    verify: checked("verify"),
    includeExt: orNull(val("include-ext")),
    excludeExt: orNull(val("exclude-ext")),
    minSize: orNull(val("min-size")),
    maxSize: orNull(val("max-size")),
    newerThan: orNull(val("newer-than")),
    olderThan: orNull(val("older-than")),
    destTemplate: orNull(val("dest-template")),
    project: orNull(val("project")),
    writeManifest: checked("manifest"),
  };
}

let running = false;
let planTotal = 1;

async function start() {
  if (running) return;
  const source = val("source");
  if (!source) return setStatus("Pick a source folder first.", "error");
  if (dests.length === 0) return setStatus("Add at least one destination.", "error");

  const req = { source, dests, resume: checked("resume"), ...gatherCommon() };

  running = true;
  ($("start") as HTMLButtonElement).disabled = true;
  ($("result-panel") as HTMLElement).hidden = true;
  setStatus("Scanning…");
  showProgress(true);
  updateProgress(0, 1, "");

  try {
    await invoke("start_harvest", { req });
  } catch (e) {
    running = false;
    ($("start") as HTMLButtonElement).disabled = false;
    showProgress(false);
    setStatus(`Could not start: ${e}`, "error");
  }
}

// ---- presets --------------------------------------------------------------

async function refreshPresets(selectName?: string) {
  const presets = await invoke<Preset[]>("list_presets");
  const sel = $("preset-select") as HTMLSelectElement;
  sel.innerHTML = '<option value="">Preset…</option>';
  for (const p of presets) {
    const opt = document.createElement("option");
    opt.value = p.name;
    opt.textContent = p.name;
    sel.appendChild(opt);
  }
  if (selectName) sel.value = selectName;
}

function applyPreset(p: Preset) {
  setVal("hash", p.hash || "xxh64");
  setChecked("verify", p.verify);
  setChecked("manifest", p.writeManifest);
  setVal("include-ext", p.includeExt ?? "");
  setVal("exclude-ext", p.excludeExt ?? "");
  setVal("min-size", p.minSize ?? "");
  setVal("max-size", p.maxSize ?? "");
  setVal("newer-than", p.newerThan ?? "");
  setVal("older-than", p.olderThan ?? "");
  setVal("dest-template", p.destTemplate ?? "");
  setVal("project", p.project ?? "");
}

async function loadSelectedPreset() {
  const name = (($("preset-select") as HTMLSelectElement).value || "").trim();
  if (!name) return;
  const presets = await invoke<Preset[]>("list_presets");
  const p = presets.find((x) => x.name === name);
  if (p) {
    applyPreset(p);
    setStatus(`Loaded preset “${name}”.`, "ok");
  }
}

async function saveCurrentPreset() {
  const name = prompt("Save preset as:");
  if (!name) return;
  const preset: Preset = { name, ...gatherCommon() };
  try {
    await invoke("save_preset", { preset });
    await refreshPresets(name);
    setStatus(`Saved preset “${name}”.`, "ok");
  } catch (e) {
    setStatus(`Could not save preset: ${e}`, "error");
  }
}

async function deleteSelectedPreset() {
  const name = (($("preset-select") as HTMLSelectElement).value || "").trim();
  if (!name) return;
  if (!confirm(`Delete preset “${name}”?`)) return;
  await invoke("delete_preset", { name });
  await refreshPresets();
  setStatus(`Deleted preset “${name}”.`, "ok");
}

// ---- wire up --------------------------------------------------------------

window.addEventListener("DOMContentLoaded", async () => {
  renderDests();

  $("pick-source").onclick = async () => {
    const f = await pickFolder();
    if (f) setVal("source", f);
  };
  $("add-dest").onclick = async () => {
    const f = await pickFolder();
    if (f && !dests.includes(f)) {
      dests.push(f);
      renderDests();
    }
  };
  $("start").onclick = start;
  ($("preset-select") as HTMLSelectElement).onchange = loadSelectedPreset;
  $("preset-save").onclick = saveCurrentPreset;
  $("preset-delete").onclick = deleteSelectedPreset;

  // progress / completion events from the backend
  await listen<Planned>("harvest:planned", (e) => {
    const p = e.payload;
    planTotal = p.copyBytes || 1;
    if (p.kept !== p.totalScanned) {
      setStatus(`Filter kept ${p.kept} of ${p.totalScanned} files. Copying ${p.toCopy}…`);
    } else {
      setStatus(`Copying ${p.toCopy} file(s), ${p.skipped} already done…`);
    }
    updateProgress(0, planTotal, "");
  });
  await listen<Progress>("harvest:progress", (e) => {
    const p = e.payload;
    updateProgress(p.doneBytes, planTotal, `${p.doneFiles} · ${p.rel}`);
  });
  await listen<Done>("harvest:done", (e) => {
    running = false;
    ($("start") as HTMLButtonElement).disabled = false;
    showProgress(false);
    setStatus(
      e.payload.success ? "Done." : "Finished with problems.",
      e.payload.success ? "ok" : "error",
    );
    showResult(e.payload);
  });
  await listen<string>("harvest:failed", (e) => {
    running = false;
    ($("start") as HTMLButtonElement).disabled = false;
    showProgress(false);
    setStatus(`Failed: ${e.payload}`, "error");
  });

  await refreshPresets();
});
