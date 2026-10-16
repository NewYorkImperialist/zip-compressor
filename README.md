# zip-compressor

A ZIP compressor written in Rust, with a desktop app (Tauri) and a command-line
tool (`zipc`). Both use the same engine, `zipcore`, which compresses files in
parallel on every CPU core.

**Every archive opens with a double-click.** zipc writes only standard Deflate
and Stored entries, so its archives open with the tools built into Windows
(Explorer), macOS (Archive Utility) and Linux (`unzip`), and with 7-Zip.
Nobody needs to install anything to open a file you send. Every preset follows
this rule, including the slowest.

## Presets

| Preset | What it does | When to use it |
|---|---|---|
| `fast` | Deflate level 1 | Quickest; files come out a little larger. |
| `balanced` (default) | Deflate level 6 | Good size at high speed. Right for most things. |
| `best` | Deflate level 9; files under 1 MiB also try level 6 (and level 9 tuned for tables and logs, where that tends to help) and keep the smallest | Slightly smaller than `balanced`, about level-9 speed. |
| `max` | Zopfli, 15 iterations (still plain Deflate) | Smallest file that still opens anywhere. **Much slower**, often 50–100x slower than `best`, for typically 3–8% smaller output. For archiving or sending large text/code. |

Already-compressed data (photos, video, archives) is never made larger: an
entry that does not shrink is stored as is. Data that deflate can't usefully
shrink at all (most photos, encrypted or random data) is detected from a few
samples and stored without trying (2.5x less CPU on photos, 8x on random
data); on real photos, video, JARs and app bundles this costs at most 0.05%
in size (details in [docs/benchmarks.md](docs/benchmarks.md)).

<!-- phase3-numbers -->
Measured on an Apple M1 (8 cores), best of 3 runs; full table in
[docs/benchmarks.md](docs/benchmarks.md). Sizes are archive size as a share of
the input (lower is better).

| Dataset | Finder (`ditto`) | `zip -9` | zipc `balanced` | zipc `best` | zipc `max` |
|---|---|---|---|---|---|
| Silesia corpus, 212 MB | 32.19%, 6.0 s | 31.91%, 15.0 s | 32.51%, 0.53 s | 31.89%, 1.1 s | **30.45%**, 218 s |
| Rust source, 26 MB | 23.51%, 0.52 s | 23.48%, 1.04 s | 23.25%, 0.09 s | 23.09%, 0.16 s | **22.24%**, 24 s |
| 20,000 small files, 42 MB | 50.98%, 1.8 s | 51.35%, 1.6 s | 48.80%, 0.27 s | 48.16%, 0.57 s | **47.23%**, 17 s |
| One 1.3 GB file | 44.58%, 34 s | 44.39%, 78 s | 44.82%, 3.2 s | 44.35%, 6.2 s | **43.17%**, 18 min |
| Photos (HEIC/JPEG), 36 MB | 99.89%, 0.71 s | 99.84%, 0.87 s | 99.87%, 0.15 s | 99.86%, 0.18 s | **99.82%**, 2.1 s |

In short: `balanced` is 4–11x faster than Finder at about the same size;
`best` is the same size as `zip -9` or smaller (photos aside, by 0.02%; 6%
smaller on many small files) while running 2–13x faster; `max` is 2.7–8%
smaller than `zip -9` on text, code and mixed data, but takes minutes on
large inputs (18 minutes for 1.3 GB).
<!-- /phase3-numbers -->

## Download

Installers for macOS (`.dmg`, Apple Silicon and Intel), Windows (`.exe`,
`.msi`) and Linux (`.AppImage`, `.deb`, `.rpm`), plus the `zipc` command-line
tool, are on the [Releases](https://github.com/NewYorkImperialist/zip-compressor/releases)
page with SHA-256 checksums.

The builds are not signed, so the first launch shows a warning: on macOS,
right-click the app and choose **Open**; on Windows, click **More info > Run
anyway** in SmartScreen.

## Command line

Download it from Releases, or build it with `cargo build --release -p zipc`
from `src-tauri/`; the binary is `src-tauri/target/release/zipc`.

```sh
zipc compress project/ notes.txt -o out.zip              # balanced
zipc compress project/ -o out.zip --preset max           # smallest, slow
zipc compress photos/ -o photos.zip -p fast -t 4 --force
```

Options for `zipc compress`:

- `-o, --output OUT.zip` — archive to create (required). Refuses to overwrite
  an existing file unless `-f/--force` is given.
- `-p, --preset fast|balanced|best|max` — speed/size trade-off (default `balanced`).
- `-t, --threads N` — worker threads; `0` (default) uses every core. The
  archive is byte-for-byte identical whatever the thread count, and on any
  CPU (checked for every preset between Apple Silicon and x86-64).
- `--symlinks store|follow|skip` — how to treat symbolic links (default `store`).
- `--keep-junk` — keep `.DS_Store`, `__MACOSX/`, `._*`, `Thumbs.db` and
  `desktop.ini` (they are left out by default).
- `-q, --quiet` — no progress line.
- Advanced overrides, which cannot be combined with `--preset` or each other:
  `-l, --level 0-9` (plain Deflate), `--store` (no compression), and
  `--zopfli-iterations N` (Zopfli with N iterations; `max` uses 15).

Exit status: 0 on success, 1 on an error, 2 on a usage error, 130 when
interrupted with Ctrl-C (no partial archive is left behind).

To check archives (any ZIP, not only zipc's), like `unzip -t`:

```sh
zipc test out.zip other.zip     # exit 0 if every entry's data and CRC-32 check out
```

To see what an archive contains without extracting it:

```sh
zipc list out.zip               # one name per line, for piping
zipc list -l out.zip            # size, compressed, ratio, method, modified (local time), name
zipc list --json out.zip        # one JSON object: archive, archive_bytes, entries, problems
```

With several archives, `list` and `list -l` print a `path:` heading before
each; `--json` prints one object per archive, one per line. In JSON,
`modified` is RFC 3339 UTC (or `null`) and `unix_mode` an octal string such as
`"0644"` (or `null` for archives made on Windows).

To extract:

```sh
zipc extract photos.zip                 # into ./photos/ next to the archive
zipc extract photos.zip -d ~/Pictures   # into a folder of your choice
zipc extract photos.zip 2024/ notes.txt # only these paths (a folder takes everything under it)
```

Options for `zipc extract`:

- `-d, --dest DIR` — folder to extract into (created if missing). By default
  it is a folder next to the archive named after it without `.zip`; if that
  folder already exists, zipc stops rather than merging into it, unless
  `--overwrite` or `--skip-existing` is given.
- `--overwrite` — replace existing files. `--skip-existing` — keep them and
  extract the rest. Without either, an existing file stops the extraction
  before anything is written.
- `--no-permissions`, `--no-mtime`, `--no-symlinks` — don't restore Unix
  permissions or modification times; skip symbolic links.
- `-j, --threads N` — worker threads; `0` (default) uses every core.
- `-q, --quiet` — no progress line. `--json` — print the summary as JSON.

Nothing is ever written outside the destination (entries with absolute paths,
`..` or links pointing out are refused), and each file is checked against its
CRC-32 before it appears. zipc never prompts; the exit status is as above.

## Desktop app

Requires Node.js and the [Tauri prerequisites](https://tauri.app/start/prerequisites/)
for your platform.

```sh
npm install
npm run tauri dev      # run in development
npm run tauri build    # build an installable app
```

Drop files or folders on the window (or use Add Files / Add Folder), pick a
preset, and choose where to save. Long runs show elapsed time and an estimate
of the time left.

### Right-click "Compress with zipc"

In the app, click **Add** next to "Add “Compress with zipc” to the right-click
menu" (and **Remove** to take it out again). Then right-click files or folders:

- **macOS:** Finder > Quick Actions > Compress with zipc (a Quick Action in
  `~/Library/Services`).
- **Windows:** Explorer > Show more options > Compress with zipc (a verb under
  `HKEY_CURRENT_USER\Software\Classes`; multi-selections become one archive).
- **Linux:** GNOME Files and Nemo > Scripts > Compress with zipc, and
  Dolphin's context menu.

It makes `Name.zip` next to the selection (`Archive.zip` for several items,
`… 2.zip` if the name is taken) with the `balanced` preset, in a small progress
window that closes itself when done. The same thing from a terminal:
`zip-compressor --quick PATH...`.

## Tests

From `src-tauri/`:

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
```

Some tests check archives with `unzip -tq` when Info-ZIP's `unzip` is
installed. The slow tests (4.5 GiB ZIP64 entries, 70,000 files, Zopfli on
several MiB) are ignored in debug builds; run everything with

```sh
cargo test --release -p zipcore -p zipc -- --include-ignored
```

The app's front end has a headless smoke test (needs Node and Chrome or
Chromium; no npm packages): it loads the real `src/index.html` and `main.js`
with a mock Tauri API and clicks through adding inputs, drag and drop,
presets, progress, success, cancel and errors, plus the right-click
quick-mode window (`quick.html`).

```sh
node tests/ui/run.mjs
```

CI also runs `scripts/ci-regress.sh`, which compresses Silesia and the tiny
corpus with every preset and fails if an archive grows past the sizes in
`scripts/ci-baseline.tsv` (output is deterministic, so any change is real)
or a run blows far past its time cap. After an intended size change, refresh
the baseline with `scripts/ci-regress.sh src-tauri/target/release/zipc --update`.

## Benchmarks

```sh
scripts/fetch-corpus.sh          # build bench-data/ (about 1.7 GB, gitignored)
scripts/bench.sh                 # full run; rewrites docs/benchmarks.md
scripts/bench.sh --quick         # one run each, no big/ dataset
```

`scripts/bench.sh` compares zipc with Info-ZIP `zip`, `ditto` (macOS) and
7-Zip when they are installed, and checks every zipc archive with `unzip -tq`.
The `max` preset is skipped on the 1.3 GB `big` dataset unless `--max-big` is
given, because it takes many minutes per run. Results, machine details and
dataset descriptions are in [docs/benchmarks.md](docs/benchmarks.md).

## Layout

- `src-tauri/crates/zipcore` — the compression engine (library).
- `src-tauri/crates/zipc` — the `zipc` command-line tool.
- `src-tauri/src` — Tauri commands for the desktop app.
- `src/` — the app's web front end (plain HTML, CSS and JavaScript).
- `scripts/` — corpus, benchmark and CI regression scripts.
- `tests/ui/` — the front end's headless smoke test.
- `docs/` — benchmark results.
- `.github/workflows/` — CI (`ci.yml`) and release builds on a `v*` tag
  (`release.yml`).
