// Quick-mode (right-click "Compress with zipc") scenarios for src/quick.js.
// One case per page load, chosen by ?case=...; quick.js runs on load, so the
// mock answers are set up here, before DOMContentLoaded.

const mock = window.__mock;
const $ = (id) => document.getElementById(id);
const testCase = new URLSearchParams(location.search).get("case");

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
async function until(cond, what, ms = 3000) {
  const t = performance.now();
  while (!cond()) {
    if (performance.now() - t > ms) throw new Error(`timed out waiting for ${what}`);
    await sleep(10);
  }
}
function check(cond, msg) {
  if (!cond) throw new Error(msg);
}
function eq(actual, expected, msg) {
  const a = JSON.stringify(actual);
  const e = JSON.stringify(expected);
  if (a !== e) throw new Error(`${msg}: expected ${e}, got ${a}`);
}
const invokes = (cmd) => mock.calls.filter((c) => c[0] === "invoke" && c[1] === cmd);

const JOB = { inputs: ["/home/u/docs/Photos"], output: "/home/u/docs/Photos.zip" };
const job = {};
mock.onInvoke = (cmd, args) => {
  if (cmd === "quick_job") return Promise.resolve(testCase === "none" ? null : JOB);
  if (cmd === "cancel") return Promise.resolve(null);
  if (cmd === "compress") {
    job.args = args;
    return new Promise((resolve, reject) => {
      job.resolve = resolve;
      job.reject = reject;
    });
  }
  return Promise.reject(new Error(`unexpected invoke ${cmd}`));
};

const summary = { output: JOB.output, files: 2, dirs: 1, symlinks: 0, bytesIn: 4 << 20, bytesOut: 1 << 20, elapsedMs: 300 };

async function started() {
  await until(() => job.args, "compress to start");
  eq($("quick-title").textContent, "Compressing Photos", "title while running");
  eq(job.args.output, JOB.output, "output path from the app");
  eq(job.args.inputs, JOB.inputs, "inputs");
  eq(job.args.options, { preset: "balanced", symlinks: "store", excludeJunk: true }, "default preset");
}

const cases = {
  ok: ["success: progress, result, closes by itself", async () => {
    await started();
    job.args.onProgress.onmessage({ current: "Photos/a.jpg", filesDone: 1, filesTotal: 2, bytesDone: 1 << 21, bytesTotal: 1 << 22 });
    check($("progress-text").textContent.startsWith("50%"), `progress: ${$("progress-text").textContent}`);
    job.resolve(summary);
    await until(() => $("quick-title").textContent === "Created Photos.zip", "result title");
    check($("result-detail").textContent.includes("25.0%"), `detail: ${$("result-detail").textContent}`);
    check(!$("reveal").hidden, "reveal offered");
    check(!mock.closed, "still open right after finishing");
    await until(() => mock.closed, "window to close by itself", 3000);
  }],
  error: ["error: message stays until closed", async () => {
    await started();
    job.reject("I/O error: No space left on device");
    await until(() => $("quick-title").textContent === "Could not create Photos.zip", "error title");
    check($("result-detail").textContent.includes("No space left"), "error detail");
    eq($("cancel").textContent, "Close", "button becomes Close");
    await sleep(1600);
    check(!mock.closed, "error window does not close by itself");
    $("cancel").click();
    await until(() => mock.closed, "close on click");
  }],
  cancel: ["cancel button cancels, then closes", async () => {
    await started();
    $("cancel").click();
    await until(() => invokes("cancel").length === 1, "cancel command");
    check($("cancel").disabled, "cancel disabled while cancelling");
    check(!mock.closed, "not closed before the job stops");
    job.reject("cancelled");
    await until(() => mock.closed, "close after cancel");
  }],
  close: ["closing the window mid-job cancels first", async () => {
    await started();
    const prevented = await mock.requestClose();
    check(prevented, "close vetoed while busy");
    await until(() => invokes("cancel").length === 1, "cancel command");
    check(!mock.closed, "still open until the job stops");
    job.reject("cancelled");
    await until(() => mock.closed, "close once cancelled");
  }],
  none: ["no job: closes at once", async () => {
    await until(() => mock.closed, "close");
    eq(invokes("compress").length, 0, "nothing compressed");
  }],
};

window.addEventListener("load", async () => {
  const [name, fn] = cases[testCase];
  const results = [];
  try {
    await fn();
    results.push({ name, ok: true });
  } catch (e) {
    results.push({ name, ok: false, error: String(e && e.stack ? e.stack : e) });
  }
  results.push(
    mock.errors.length === 0
      ? { name: "no uncaught errors", ok: true }
      : { name: "no uncaught errors", ok: false, error: mock.errors.join("\n") },
  );
  await fetch("/__results", { method: "POST", body: JSON.stringify(results) });
});
