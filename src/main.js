const { invoke, Channel } = window.__TAURI__.core;
const { open, save } = window.__TAURI__.dialog;
const { revealItemInDir } = window.__TAURI__.opener;
const { getCurrentWebview } = window.__TAURI__.webview;

const $ = (id) => document.getElementById(id);

const state = {
  inputs: [],
  busy: false,
  lastOutput: null,
  // Running job timing: { start, last (latest progress), timer, slow }.
  job: null,
};

// The value of each #preset option: label, the hint line under the select, and
// whether it is slow enough to warrant a visible elapsed/ETA line from the start.
// Every choice writes a standard ZIP that opens with the built-in tools.
const PRESETS = {
  fast: { label: "Fast", hint: "Quickest. Files come out a little larger." },
  balanced: { label: "Balanced", hint: "Good size at high speed. Right for most things." },
  best: { label: "Best", hint: "Smaller files, somewhat slower. Tries several settings per file." },
  max: {
    label: "Max",
    hint: "Smallest file that still opens anywhere. Much slower — best for archiving or sending large text/code.",
    slow: true,
  },
  store: { label: "Store", hint: "No compression. Fastest; only useful for files that are already compressed." },
};

// Show elapsed time (and an ETA) once a non-slow job has run this long.
const SHOW_TIME_AFTER_MS = 3000;

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

function formatClock(ms) {
  const total = Math.floor(ms / 1000);
  const h = Math.floor(total / 3600);
  const m = Math.floor((total % 3600) / 60);
  const sec = String(total % 60).padStart(2, "0");
  return h > 0 ? `${h}h ${String(m).padStart(2, "0")}m ${sec}s` : `${m}m ${sec}s`;
}

function formatEta(seconds) {
  if (seconds < 10) return "a few seconds left";
  if (seconds < 60) return `about ${Math.ceil(seconds / 5) * 5} s left`;
  if (seconds < 3600) return `about ${Math.round(seconds / 60)} min left`;
  const h = Math.floor(seconds / 3600);
  const m = Math.round((seconds % 3600) / 60);
  return `about ${h} h ${m} min left`;
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
  $("preset").disabled = state.busy;
  $("exclude-junk").disabled = state.busy;
  $("cancel").hidden = !state.busy;
  $("cancel").disabled = false;
  for (const b of document.querySelectorAll(".remove")) b.disabled = state.busy;
}

// ---------- options ----------

function selectedPreset() {
  const v = $("preset").value;
  return v in PRESETS ? v : "balanced";
}

function currentOptions() {
  const p = selectedPreset();
  const excludeJunk = $("exclude-junk").checked;
  // "Store" is not a preset; it uses the explicit method override.
  if (p === "store") return { method: "store", symlinks: "store", excludeJunk };
  return { preset: p, symlinks: "store", excludeJunk };
}

function renderPresetHint() {
  const info = PRESETS[selectedPreset()];
  const hint = $("preset-hint");
  hint.textContent = info.hint;
  hint.toggleAttribute("data-slow", Boolean(info.slow));
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
  if (state.job) {
    state.job.last = p;
    renderTiming();
  }
}

// Elapsed time, throughput and ETA from bytes/sec so far. Shown from the start
// for slow presets (Max) and after a few seconds for everything else.
function renderTiming() {
  const job = state.job;
  const line = $("progress-time");
  if (!job) {
    line.hidden = true;
    return;
  }
  const elapsed = performance.now() - job.start;
  if (!job.slow && elapsed < SHOW_TIME_AFTER_MS) {
    line.hidden = true;
    return;
  }
  const parts = [`Elapsed ${formatClock(elapsed)}`];
  const p = job.last;
  const secs = elapsed / 1000;
  if (p && p.bytesDone > 0 && secs >= 2) {
    const rate = p.bytesDone / secs;
    parts.push(`${(rate / 1e6).toFixed(1)} MB/s`);
    const left = p.bytesTotal - p.bytesDone;
    if (left > 0) parts.push(formatEta(left / rate));
    else parts.push("finishing…");
  } else if (job.slow) {
    parts.push("estimating time left…");
  }
  line.textContent = parts.join("  ·  ");
  line.hidden = false;
}

function startTiming(slow) {
  stopTiming();
  state.job = { start: performance.now(), last: null, slow, timer: setInterval(renderTiming, 1000) };
  renderTiming();
}

function stopTiming() {
  if (state.job) clearInterval(state.job.timer);
  state.job = null;
  renderTiming();
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

function summaryText(s, presetLabel) {
  const ratio = s.bytesIn > 0 ? `${((s.bytesOut / s.bytesIn) * 100).toFixed(1)}%` : "n/a";
  const secs = s.elapsedMs / 1000;
  const speed = secs > 0 ? `${(s.bytesIn / 1e6 / secs).toFixed(1)} MB/s` : "n/a";
  const counts = [plural(s.files, "file"), plural(s.dirs, "folder")];
  if (s.symlinks) counts.push(plural(s.symlinks, "symlink"));
  return (
    `${counts.join(", ")}  ·  ${formatBytes(s.bytesIn)} → ${formatBytes(s.bytesOut)} (${ratio})  ·  ` +
    `${formatDuration(s.elapsedMs)} at ${speed}  ·  ${presetLabel}`
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
  const preset = PRESETS[selectedPreset()];

  state.busy = true;
  updateControls();
  $("result").hidden = true;
  $("progress").hidden = false;
  resetProgress();
  startTiming(Boolean(preset.slow));

  try {
    const summary = await invoke("compress", {
      inputs: state.inputs,
      output,
      options: currentOptions(),
      onProgress,
    });
    $("progress-bar").value = 1;
    showResult("ok", `Created ${baseName(summary.output)}`, summaryText(summary, preset.label), summary.output);
  } catch (e) {
    const msg = String(e);
    if (msg === "cancelled") {
      showResult("cancelled", "Cancelled", "No archive was written.");
    } else {
      showResult("error", "Compression failed", msg);
    }
  } finally {
    stopTiming();
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
  $("preset").addEventListener("change", renderPresetHint);
  renderPresetHint();
  renderInputs();
  setupDragDrop().catch((e) => console.error("drag-drop unavailable:", e));
});
