// Quick mode: the window the right-click "Compress with zipc" opens. It
// compresses the selection with the default preset into the archive the app
// picked (next to the files), then closes; errors stay on screen.
const { invoke, Channel } = window.__TAURI__.core;
const { revealItemInDir } = window.__TAURI__.opener;
const { getCurrentWindow } = window.__TAURI__.window;

const $ = (id) => document.getElementById(id);
const isWindows = navigator.userAgent.includes("Windows");
// How long a finished job stays on screen before the window closes.
const CLOSE_AFTER_MS = 1200;

let busy = false;
let closing = false;

function baseName(path) {
  const parts = path.replace(/[\\/]+$/, "").split(/[\\/]/);
  return parts[parts.length - 1] || path;
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

function showProgress(p) {
  const frac = p.bytesTotal > 0 ? p.bytesDone / p.bytesTotal : p.filesTotal > 0 ? p.filesDone / p.filesTotal : 1;
  $("progress-bar").value = Math.min(1, frac);
  $("progress-text").textContent = `${Math.floor(Math.min(1, frac) * 100)}%  ·  ${formatBytes(p.bytesDone)} of ${formatBytes(p.bytesTotal)}`;
  $("progress-counts").textContent = `${p.filesDone} / ${p.filesTotal} files`;
}

function showResult(kind, title, detail) {
  $("quick-result").hidden = false;
  $("quick-result").dataset.kind = kind;
  $("result-title").textContent = title;
  $("result-detail").textContent = detail;
  $("progress-bar").hidden = true;
  $("progress-text").textContent = "";
  $("progress-counts").textContent = "";
}

function close() {
  closing = true;
  getCurrentWindow().close();
}

async function run() {
  const job = await invoke("quick_job");
  if (!job) {
    close();
    return;
  }
  const name = baseName(job.output);
  document.title = `Compressing ${name}`;
  $("quick-title").textContent =
    job.inputs.length === 1 ? `Compressing ${baseName(job.inputs[0])}` : `Compressing ${job.inputs.length} items`;

  const onProgress = new Channel();
  onProgress.onmessage = showProgress;
  busy = true;
  try {
    const s = await invoke("compress", {
      inputs: job.inputs,
      output: job.output,
      options: { preset: "balanced", symlinks: "store", excludeJunk: true },
      onProgress,
    });
    busy = false;
    const ratio = s.bytesIn > 0 ? `${((s.bytesOut / s.bytesIn) * 100).toFixed(1)}%` : "n/a";
    $("quick-title").textContent = `Created ${name}`;
    showResult("ok", "", `${formatBytes(s.bytesIn)} → ${formatBytes(s.bytesOut)} (${ratio})`);
    $("cancel").textContent = "Close";
    $("reveal").hidden = false;
    $("reveal").onclick = async () => {
      await revealItemInDir(s.output).catch(() => {});
      close();
    };
    setTimeout(() => {
      // Stay open if the user is about to click something.
      if (!document.querySelector("button:hover")) close();
    }, CLOSE_AFTER_MS);
  } catch (e) {
    busy = false;
    const msg = String(e);
    if (closing || msg === "cancelled") {
      close();
      return;
    }
    $("quick-title").textContent = `Could not create ${name}`;
    showResult("error", "", msg);
    $("cancel").textContent = "Close";
  }
}

async function cancelOrClose() {
  if (!busy) {
    close();
    return;
  }
  closing = true;
  $("cancel").disabled = true;
  $("progress-text").textContent = "Cancelling…";
  await invoke("cancel").catch(() => {});
}

window.addEventListener("DOMContentLoaded", () => {
  $("reveal").textContent = isWindows ? "Show in Explorer" : "Show in Finder";
  $("cancel").addEventListener("click", cancelOrClose);
  // Closing the window mid-job cancels it first, so no partial archive is left.
  getCurrentWindow().onCloseRequested(async (event) => {
    if (busy) {
      event.preventDefault();
      await cancelOrClose();
    }
  });
  run().catch((e) => {
    busy = false;
    showResult("error", "", String(e));
    $("cancel").textContent = "Close";
  });
});
