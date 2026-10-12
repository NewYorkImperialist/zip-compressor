// UI smoke scenarios for src/main.js, run in order against the real page with
// the mock Tauri API (mock-tauri.js). Results are POSTed to the runner.

const mock = window.__mock;
const $ = (id) => document.getElementById(id);
const results = [];

const tick = () => new Promise((r) => setTimeout(r, 0));
async function settle() {
  for (let i = 0; i < 5; i++) await tick();
}

function check(cond, msg) {
  if (!cond) throw new Error(msg);
}
function eq(actual, expected, msg) {
  const a = JSON.stringify(actual);
  const e = JSON.stringify(expected);
  if (a !== e) throw new Error(`${msg}: expected ${e}, got ${a}`);
}
const visible = (id) => !$(id).hidden;
const listNames = () => [...document.querySelectorAll("#input-list .input-name")].map((n) => n.textContent);
const invokes = (cmd) => mock.calls.filter((c) => c[0] === "invoke" && c[1] === cmd);

function setPreset(value) {
  $("preset").value = value;
  $("preset").dispatchEvent(new Event("change"));
}

// A compress call the scenario resolves or rejects by hand.
function pendingCompress() {
  const job = {};
  job.started = new Promise((started) => {
    mock.onInvoke = (cmd, args) => {
      if (cmd === "cancel") return Promise.resolve(null);
      if (cmd !== "compress") return Promise.reject(new Error(`unexpected invoke ${cmd}`));
      job.args = args;
      started();
      return new Promise((resolve, reject) => {
        job.resolve = resolve;
        job.reject = reject;
      });
    };
  });
  return job;
}

const summary = (output) => ({
  output,
  files: 3,
  dirs: 1,
  symlinks: 0,
  bytesIn: 3 << 20,
  bytesOut: 1 << 20,
  elapsedMs: 1500,
});

const scenarios = [
  ["initial state", async () => {
    check(!visible("input-list") && visible("empty-hint"), "empty list should show the hint");
    check($("compress").disabled, "compress should be disabled with no inputs");
    check($("clear-inputs").disabled, "clear should be disabled with no inputs");
    check(!visible("cancel") && !visible("progress") && !visible("result"), "no job UI at start");
    eq($("preset").value, "balanced", "default preset");
    check($("preset-hint").textContent.length > 0, "preset hint is shown");
    check(mock.dragHandler, "drag-and-drop handler registered");
  }],

  ["add files and folders, no duplicates, picker cancel", async () => {
    mock.dialog.open.push(["/home/u/docs/report.txt", "/home/u/docs/photos"]);
    $("add-files").click();
    await settle();
    eq(listNames(), ["report.txt", "photos"], "listed names");
    check(!$("compress").disabled && !$("clear-inputs").disabled, "compress and clear enabled");
    const open = mock.calls.filter((c) => c[0] === "open");
    eq(open[0][1], { multiple: true, directory: false }, "Add Files opens a file picker");

    mock.dialog.open.push(["/home/u/docs/photos"]);
    $("add-folder").click();
    await settle();
    eq(mock.calls.filter((c) => c[0] === "open")[1][1], { multiple: true, directory: true }, "Add Folder opens a folder picker");
    eq(listNames(), ["report.txt", "photos"], "a duplicate is not added twice");

    mock.dialog.open.push(null); // user closed the picker
    $("add-files").click();
    await settle();
    eq(listNames(), ["report.txt", "photos"], "cancelling the picker changes nothing");
  }],

  ["remove an input", async () => {
    document.querySelector("#input-list .remove").click();
    await settle();
    eq(listNames(), ["photos"], "report.txt removed");
  }],

  ["drag and drop", async () => {
    mock.dragHandler({ payload: { type: "enter", paths: [] } });
    check($("drop-zone").classList.contains("dragging"), "drop zone highlights on drag enter");
    mock.dragHandler({ payload: { type: "leave" } });
    check(!$("drop-zone").classList.contains("dragging"), "highlight cleared on leave");
    mock.dragHandler({ payload: { type: "over", paths: [] } });
    mock.dragHandler({ payload: { type: "drop", paths: ["/home/u/docs/notes.md", "/home/u/docs/photos"] } });
    await settle();
    check(!$("drop-zone").classList.contains("dragging"), "highlight cleared on drop");
    eq(listNames(), ["photos", "notes.md"], "dropped paths added once");
  }],

  ["preset hints", async () => {
    setPreset("max");
    check($("preset-hint").hasAttribute("data-slow"), "Max is flagged slow");
    setPreset("fast");
    check(!$("preset-hint").hasAttribute("data-slow"), "Fast is not slow");
  }],

  ["compress: progress, success, reveal", async () => {
    setPreset("best");
    mock.dialog.save.push("/home/u/out/Bundle"); // no extension: .zip is added
    const job = pendingCompress();
    $("compress").click();
    await job.started;
    await settle();

    const save = mock.calls.filter((c) => c[0] === "save").at(-1)[1];
    eq(save.defaultPath, "/home/u/docs/Archive.zip", "default name for several inputs");
    eq(job.args.inputs, ["/home/u/docs/photos", "/home/u/docs/notes.md"], "inputs sent in order");
    eq(job.args.output, "/home/u/out/Bundle.zip", ".zip appended to the output");
    eq(job.args.options, { preset: "best", symlinks: "store", excludeJunk: true }, "options sent");
    check(job.args.onProgress instanceof window.__TAURI__.core.Channel, "progress channel passed");

    check(visible("progress") && visible("cancel"), "progress and cancel shown while busy");
    for (const id of ["compress", "add-files", "add-folder", "clear-inputs", "preset", "exclude-junk"]) {
      check($(id).disabled, `${id} disabled while busy`);
    }
    check([...document.querySelectorAll(".remove")].every((b) => b.disabled), "remove buttons disabled while busy");
    mock.dragHandler({ payload: { type: "drop", paths: ["/home/u/late.txt"] } });
    eq(listNames(), ["photos", "notes.md"], "drops ignored while busy");

    job.args.onProgress.onmessage({
      current: "photos/a.jpg",
      filesDone: 1,
      filesTotal: 3,
      bytesDone: 3 << 19,
      bytesTotal: 3 << 20,
    });
    check($("progress-text").textContent.startsWith("50%"), `progress text: ${$("progress-text").textContent}`);
    eq($("progress-counts").textContent, "1 / 3 files", "file counts");
    eq($("progress-current").textContent, "photos/a.jpg", "current file");

    job.resolve(summary("/home/u/out/Bundle.zip"));
    await settle();
    check(!visible("progress") && !visible("cancel"), "job UI hidden after success");
    eq($("result").dataset.kind, "ok", "result kind");
    eq($("result-title").textContent, "Created Bundle.zip", "result title");
    const detail = $("result-detail").textContent;
    check(detail.includes("3 files") && detail.includes("33.3%") && detail.includes("Best"), `result detail: ${detail}`);
    check(visible("reveal"), "reveal button shown");
    check(!$("compress").disabled && !$("preset").disabled, "controls re-enabled");

    $("reveal").click();
    await settle();
    eq(mock.calls.at(-1), ["reveal", "/home/u/out/Bundle.zip"], "reveal opens the archive's folder");
  }],

  ["store and junk options", async () => {
    setPreset("store");
    $("exclude-junk").checked = false;
    mock.dialog.save.push("/home/u/out/raw.ZIP");
    const job = pendingCompress();
    $("compress").click();
    await job.started;
    eq(job.args.output, "/home/u/out/raw.ZIP", "an existing .zip extension (any case) is kept");
    eq(job.args.options, { method: "store", symlinks: "store", excludeJunk: false }, "store options");
    job.resolve(summary("/home/u/out/raw.ZIP"));
    await settle();
    $("exclude-junk").checked = true;
    setPreset("balanced");
  }],

  ["cancel", async () => {
    mock.dialog.save.push("/home/u/out/c.zip");
    const job = pendingCompress();
    $("compress").click();
    await job.started;
    await settle();
    $("cancel").click();
    await settle();
    eq(invokes("cancel").length, 1, "cancel command sent");
    check($("cancel").disabled, "cancel button disabled after the click");
    eq($("progress-text").textContent, "Cancelling…", "cancelling text");
    job.reject("cancelled");
    await settle();
    eq($("result").dataset.kind, "cancelled", "result kind");
    check(!visible("reveal"), "no reveal for a cancelled job");
    check(!$("compress").disabled && !visible("cancel"), "controls back after cancel");
  }],

  ["engine error", async () => {
    mock.dialog.save.push("/home/u/out/e.zip");
    const job = pendingCompress();
    $("compress").click();
    await job.started;
    job.reject("I/O error on /home/u/out/e.zip: No space left on device");
    await settle();
    eq($("result").dataset.kind, "error", "result kind");
    eq($("result-title").textContent, "Compression failed", "error title");
    check($("result-detail").textContent.includes("No space left"), "error detail shown");
    check(!$("compress").disabled, "can retry after an error");
  }],

  ["save dialog cancelled", async () => {
    const before = invokes("compress").length;
    mock.dialog.save.push(null);
    $("compress").click();
    await settle();
    eq(invokes("compress").length, before, "no compress without an output path");
    check(!visible("progress"), "no progress shown");
  }],

  ["single input default name, then clear", async () => {
    $("clear-inputs").click();
    await settle();
    eq(listNames(), [], "cleared");
    check($("compress").disabled && visible("empty-hint"), "empty state after clear");
    mock.dragHandler({ payload: { type: "drop", paths: ["/home/u/docs/photos/"] } });
    await settle();
    mock.dialog.save.push(null);
    $("compress").click();
    await settle();
    const save = mock.calls.filter((c) => c[0] === "save").at(-1)[1];
    eq(save.defaultPath, "/home/u/docs/photos.zip", "one input: archive named after it, next to it");
    eq(save.filters, [{ name: "ZIP archive", extensions: ["zip"] }], "save filter");
  }],
];

async function run() {
  for (const [name, fn] of scenarios) {
    try {
      // A failed check can leave a job pending; don't let it hang the run.
      let timer;
      const limit = new Promise((_, reject) => {
        timer = setTimeout(() => reject(new Error("timed out after 5 s")), 5000);
      });
      await Promise.race([fn(), limit]).finally(() => clearTimeout(timer));
      results.push({ name, ok: true });
    } catch (e) {
      results.push({ name, ok: false, error: String(e && e.stack ? e.stack : e) });
    }
  }
  results.push(
    mock.errors.length === 0
      ? { name: "no uncaught errors or console.error", ok: true }
      : { name: "no uncaught errors or console.error", ok: false, error: mock.errors.join("\n") },
  );
  await fetch("/__results", { method: "POST", body: JSON.stringify(results) });
}

window.addEventListener("load", () => {
  run().catch((e) => fetch("/__results", { method: "POST", body: JSON.stringify([{ name: "runner", ok: false, error: String(e) }]) }));
});
