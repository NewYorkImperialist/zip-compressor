const { invoke, Channel } = window.__TAURI__.core;
const { open, save } = window.__TAURI__.dialog;
const { revealItemInDir } = window.__TAURI__.opener;
const { getCurrentWebview } = window.__TAURI__.webview;

const $ = (id) => document.getElementById(id);

const state = {
  inputs: [],
  busy: false,
  lastOutput: null,
};

const isWindows = navigator.userAgent.includes("Windows");

// ---------- helpers ----------

function baseName(path) {
  const trimmed = path.replace(/[\\/]+$/, "");
  const parts = trimmed.split(/[\\/]/);
  return parts[parts.length - 1] || trimmed;
}

function dirName(path) {
  const trimmed = path.replace(/[\\/]+$/, "");
  const idx = Math.max(trimmed.lastIndexOf("/"), trimmed.lastIndexOf("\\"));
  return idx > 0 ? trimmed.slice(0, idx) : "";
}

function joinPath(dir, name) {
  if (!dir) return name;
  const sep = dir.includes("\\") && !dir.includes("/") ? "\\" : "/";
  return dir + sep + name;
}

function formatBytes(bytes) {
  const units = ["KiB", "MiB", "GiB", "TiB"];
  if (bytes < 1024) return `${bytes} B`;
  let v = bytes / 1024;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  return `${v.toFixed(1)} ${units[i]}`;
}

function formatDuration(ms) {
  if (ms < 1000) return `${ms} ms`;
  const s = ms / 1000;
  if (s < 60) return `${s.toFixed(2)} s`;
  const m = Math.floor(s / 60);
  return `${m}m ${String(Math.floor(s % 60)).padStart(2, "0")}s`;
}

function plural(n, word) {
  return `${n} ${word}${n === 1 ? "" : "s"}`;
}

// ---------- inputs ----------

function addInputs(paths) {
  if (state.busy || !paths) return;
  const list = Array.isArray(paths) ? paths : [paths];
  for (const p of list) {
    if (p && !state.inputs.includes(p)) state.inputs.push(p);
  }
  renderInputs();
}

function removeInput(path) {
  if (state.busy) return;
  state.inputs = state.inputs.filter((p) => p !== path);
  renderInputs();
}

function renderInputs() {
  const ul = $("input-list");
  ul.replaceChildren();
  for (const path of state.inputs) {
    const li = document.createElement("li");
    const text = document.createElement("div");
    text.className = "input-text";
    const name = document.createElement("span");
    name.className = "input-name";
    name.textContent = baseName(path);
    const full = document.createElement("span");
    full.className = "input-path";
    full.textContent = dirName(path);
    full.title = path;
    text.append(name, full);

    const btn = document.createElement("button");
    btn.type = "button";
    btn.className = "remove";
    btn.title = "Remove";
    btn.setAttribute("aria-label", `Remove ${baseName(path)}`);
    btn.textContent = "×";
    btn.disabled = state.busy;
    btn.addEventListener("click", () => removeInput(path));

    li.append(text, btn);
    ul.append(li);
  }
  const empty = state.inputs.length === 0;
  ul.hidden = empty;
  $("empty-hint").hidden = !empty;
  updateControls();
}

function updateControls() {
  const empty = state.inputs.length === 0;
  $("compress").disabled = state.busy || empty;
  $("clear-inputs").disabled = state.busy || empty;
  $("add-files").disabled = state.busy;
  $("add-folder").disabled = state.busy;
  $("method").disabled = state.busy;
  $("exclude-junk").disabled = state.busy;
  $("cancel").hidden = !state.busy;
  $("cancel").disabled = false;
  for (const b of document.querySelectorAll(".remove")) b.disabled = state.busy;
}

// ---------- options ----------

function currentOptions() {
  const m = $("method").value;
  const excludeJunk = $("exclude-junk").checked;
  if (m === "store") {
    return { method: "store", level: null, symlinks: "store", excludeJunk };
  }
  return { method: "deflate", level: Number(m), symlinks: "store", excludeJunk };
}

// ---------- progress / result ----------

function showProgress(p) {
  const bar = $("progress-bar");
  const frac = p.bytesTotal > 0 ? p.bytesDone / p.bytesTotal : p.filesTotal > 0 ? p.filesDone / p.filesTotal : 1;
  bar.value = Math.min(1, frac);
  $("progress-text").textContent =
    `${Math.floor(Math.min(1, frac) * 100)}%  ·  ${formatBytes(p.bytesDone)} of ${formatBytes(p.bytesTotal)}`;
  $("progress-counts").textContent = `${p.filesDone} / ${p.filesTotal} files`;
  $("progress-current").textContent = p.current;
  $("progress-current").title = p.current;
}

function resetProgress() {
  $("progress-bar").removeAttribute("value"); // indeterminate until the first event
  $("progress-text").textContent = "Preparing…";
  $("progress-counts").textContent = "";
  $("progress-current").textContent = "";
}

function showResult(kind, title, detail, output) {
  const box = $("result");
  box.hidden = false;
  box.dataset.kind = kind;
  $("result-title").textContent = title;
  $("result-detail").textContent = detail;
  state.lastOutput = output || null;
  $("reveal").hidden = !output;
}

function summaryText(s) {
  const ratio = s.bytesIn > 0 ? `${((s.bytesOut / s.bytesIn) * 100).toFixed(1)}%` : "n/a";
  const secs = s.elapsedMs / 1000;
  const speed = secs > 0 ? `${(s.bytesIn / 1e6 / secs).toFixed(1)} MB/s` : "n/a";
  const counts = [plural(s.files, "file"), plural(s.dirs, "folder")];
  if (s.symlinks) counts.push(plural(s.symlinks, "symlink"));
  return (
    `${counts.join(", ")}  ·  ${formatBytes(s.bytesIn)} → ${formatBytes(s.bytesOut)} (${ratio})  ·  ` +
    `${formatDuration(s.elapsedMs)} at ${speed}`
  );
}

// ---------- actions ----------

async function pick(directory) {
  try {
    const picked = await open({ multiple: true, directory });
    addInputs(picked);
  } catch (e) {
    showResult("error", "Could not open the file picker", String(e));
  }
}

async function runCompress() {
  if (state.busy || state.inputs.length === 0) return;
  const first = state.inputs[0];
  const defaultName = `${state.inputs.length === 1 ? baseName(first) : "Archive"}.zip`;

  let output;
  try {
    output = await save({
      title: "Save ZIP archive",
      defaultPath: joinPath(dirName(first), defaultName),
      filters: [{ name: "ZIP archive", extensions: ["zip"] }],
    });
  } catch (e) {
    showResult("error", "Could not open the save dialog", String(e));
    return;
  }
  if (!output) return;
  if (!/\.zip$/i.test(output)) output += ".zip";

  const onProgress = new Channel();
  onProgress.onmessage = showProgress;

  state.busy = true;
  updateControls();
  $("result").hidden = true;
  $("progress").hidden = false;
  resetProgress();

  try {
    const summary = await invoke("compress", {
      inputs: state.inputs,
      output,
      options: currentOptions(),
      onProgress,
    });
    $("progress-bar").value = 1;
    showResult("ok", `Created ${baseName(summary.output)}`, summaryText(summary), summary.output);
  } catch (e) {
    const msg = String(e);
    if (msg === "cancelled") {
      showResult("cancelled", "Cancelled", "No archive was written.");
    } else {
      showResult("error", "Compression failed", msg);
    }
  } finally {
    state.busy = false;
    $("progress").hidden = true;
    updateControls();
  }
}

async function cancelJob() {
  $("cancel").disabled = true;
  $("progress-text").textContent = "Cancelling…";
  try {
    await invoke("cancel");
  } catch (e) {
    console.error(e);
  }
}

async function reveal() {
  if (!state.lastOutput) return;
  try {
    await revealItemInDir(state.lastOutput);
  } catch (e) {
    showResult("error", "Could not reveal the archive", String(e));
  }
}

// ---------- drag and drop ----------

async function setupDragDrop() {
  const zone = $("drop-zone");
  await getCurrentWebview().onDragDropEvent((event) => {
    const { type } = event.payload;
    if (type === "enter" || type === "over") {
      zone.classList.toggle("dragging", !state.busy);
    } else if (type === "drop") {
      zone.classList.remove("dragging");
      addInputs(event.payload.paths);
    } else {
      zone.classList.remove("dragging");
    }
  });
}

window.addEventListener("DOMContentLoaded", () => {
  $("reveal").textContent = isWindows ? "Show in Explorer" : "Show in Finder";
  $("add-files").addEventListener("click", () => pick(false));
  $("add-folder").addEventListener("click", () => pick(true));
  $("clear-inputs").addEventListener("click", () => {
    if (state.busy) return;
    state.inputs = [];
    renderInputs();
  });
  $("compress").addEventListener("click", runCompress);
  $("cancel").addEventListener("click", cancelJob);
  $("reveal").addEventListener("click", reveal);
  renderInputs();
  setupDragDrop().catch((e) => console.error("drag-drop unavailable:", e));
});
