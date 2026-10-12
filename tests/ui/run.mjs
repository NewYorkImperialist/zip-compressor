#!/usr/bin/env node
// Headless smoke test of the desktop app's front end (src/), no dependencies:
// serves the real index.html with a mock Tauri API injected, opens it in
// headless Chrome/Chromium, and prints the scenario results.
//
//   node tests/ui/run.mjs            # CHROME=/path/to/chrome to pick a browser
//
// Exit status 0 if every scenario passed.
import { spawn, spawnSync } from "node:child_process";
import { existsSync, mkdtempSync, readFileSync, rmSync } from "node:fs";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import { extname, join, normalize, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";

const here = fileURLToPath(new URL(".", import.meta.url));
const appDir = resolve(here, "../../src");
const TIMEOUT_MS = 30_000;

function findChrome() {
  if (process.env.CHROME) return process.env.CHROME;
  const fixed = [
    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    "/Applications/Chromium.app/Contents/MacOS/Chromium",
    "C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe",
  ];
  for (const p of fixed) if (existsSync(p)) return p;
  for (const name of ["google-chrome", "google-chrome-stable", "chromium", "chromium-browser"]) {
    const r = spawnSync("which", [name], { encoding: "utf8" });
    if (r.status === 0 && r.stdout.trim()) return r.stdout.trim();
  }
  return null;
}

const types = { ".html": "text/html", ".js": "text/javascript", ".css": "text/css", ".svg": "image/svg+xml" };

// Pages under test: the app page, its scenario script, and query strings
// (one browser run each; quick mode runs once per case).
const RUNS = [
  { page: "index.html", main: "main.js", scenarios: "scenarios.js", query: "" },
  ...["ok", "error", "cancel", "close", "none"].map((c) => ({
    page: "quick.html",
    main: "quick.js",
    scenarios: "quick-scenarios.js",
    query: `?case=${c}`,
  })),
];

function page(run) {
  const html = readFileSync(join(appDir, run.page), "utf8");
  const tag = `<script type="module" src="/${run.main}" defer></script>`;
  if (!html.includes(tag)) throw new Error(`${run.page} no longer loads ${run.main} as ${tag}; update tests/ui/run.mjs`);
  return html.replace(
    tag,
    `<script src="/__smoke/mock-tauri.js"></script>\n    ${tag}\n    <script type="module" src="/__smoke/${run.scenarios}"></script>`,
  );
}

// Serve `rel` from `root` only if it stays inside it.
function serveFile(res, root, rel) {
  const path = normalize(join(root, rel));
  if (!path.startsWith(root + sep) || !existsSync(path)) {
    res.writeHead(404).end();
    return;
  }
  res.writeHead(200, { "content-type": types[extname(path)] || "application/octet-stream" });
  res.end(readFileSync(path));
}

const chrome = findChrome();
if (!chrome) {
  console.error("ui-smoke: no Chrome/Chromium found (set CHROME=/path/to/chrome)");
  process.exit(2);
}

let current = null;
let finish = () => {};

const server = createServer((req, res) => {
  const url = new URL(req.url, "http://localhost");
  if (req.method === "POST" && url.pathname === "/__results") {
    let body = "";
    req.on("data", (c) => (body += c));
    req.on("end", () => {
      res.writeHead(204).end();
      finish(JSON.parse(body));
    });
    return;
  }
  if (url.pathname === "/" + current.page) {
    res.writeHead(200, { "content-type": "text/html" }).end(page(current));
  } else if (url.pathname.startsWith("/__smoke/")) {
    serveFile(res, here.replace(/[\\/]$/, ""), url.pathname.slice("/__smoke/".length));
  } else {
    serveFile(res, appDir, url.pathname.slice(1));
  }
});

// Load one page in a fresh headless browser; resolves to its results.
function runPage(port, run) {
  current = run;
  const profile = mkdtempSync(join(tmpdir(), "ui-smoke-"));
  const browser = spawn(
    chrome,
    [
      "--headless=new",
      "--disable-gpu",
      "--no-first-run",
      "--no-default-browser-check",
      ...(process.platform === "linux" ? ["--no-sandbox"] : []),
      `--user-data-dir=${profile}`,
      `http://127.0.0.1:${port}/${run.page}${run.query}`,
    ],
    { stdio: "ignore" },
  );
  return new Promise((resolve) => {
    const timer = setTimeout(() => finish(null), TIMEOUT_MS);
    finish = (results) => {
      finish = () => {};
      clearTimeout(timer);
      browser.kill();
      try {
        rmSync(profile, { recursive: true, force: true });
      } catch {}
      resolve(results ?? [{ name: `${run.page}${run.query}`, ok: false, error: `no results within ${TIMEOUT_MS / 1000} s` }]);
    };
  });
}

server.listen(0, "127.0.0.1", async () => {
  const { port } = server.address();
  const results = [];
  for (const run of RUNS) {
    const prefix = run.page === "index.html" ? "" : `quick ${run.query.slice(6)}: `;
    for (const r of await runPage(port, run)) results.push({ ...r, name: prefix + r.name });
  }
  server.close();
  let failed = 0;
  for (const r of results) {
    console.log(`${r.ok ? "ok  " : "FAIL"}  ${r.name}`);
    if (!r.ok) {
      failed++;
      console.log(`      ${r.error.replace(/\n/g, "\n      ")}`);
    }
  }
  console.log(`\n${results.length - failed} passed, ${failed} failed`);
  process.exit(failed ? 1 : 0);
});
