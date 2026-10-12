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
| `best` | Deflate level 9; files under 1 MiB also try level 6 and keep the smaller | Slightly smaller than `balanced`, about level-9 speed. |
| `max` | Zopfli, 15 iterations (still plain Deflate) | Smallest file that still opens anywhere. **Much slower**, often 50–100x slower than `best`, for typically 3–8% smaller output. For archiving or sending large text/code. |

Already-compressed data (photos, video, archives) is never made larger: an
entry that does not shrink is stored as is.

<!-- phase3-numbers -->
Measured on an Apple M1 (8 cores), best of 3 runs; full table in
[docs/benchmarks.md](docs/benchmarks.md). Sizes are archive size as a share of
the input (lower is better).

| Dataset | Finder (`ditto`) | `zip -9` | zipc `balanced` | zipc `best` | zipc `max` |
|---|---|---|---|---|---|
| Silesia corpus, 212 MB | 32.19%, 5.8 s | 31.91%, 14.7 s | 32.51%, 0.62 s | 31.89%, 1.2 s | **30.45%**, 221 s |
| Rust source, 26 MB | 23.51%, 0.53 s | 23.48%, 1.05 s | 23.25%, 0.10 s | 23.09%, 0.16 s | **22.24%**, 24 s |
| 20,000 small files, 42 MB | 50.98%, 3.5 s | 51.35%, 3.3 s | 48.80%, 0.42 s | 48.58%, 0.70 s | **47.25%**, 17 s |
| One 1.3 GB file | 44.58%, 37.9 s | 44.39%, 82.6 s | 44.82%, 3.1 s | 44.35%, 8.0 s | not run |
| Photos (HEIC/JPEG), 36 MB | 99.89%, 0.84 s | 99.84%, 1.0 s | 99.87%, 0.38 s | 99.86%, 0.23 s | **99.82%**, 3.9 s |

In short: `balanced` is 5–12x faster than Finder at about the same size
(2x on photos); `best` is the same size as `zip -9` or smaller (photos aside,
by 0.02%) while running 4–12x faster; `max` is 4.6–8% smaller than `zip -9`
on text, code and mixed data, but takes minutes on large inputs.
<!-- /phase3-numbers -->

## Command line

Build it with `cargo build --release -p zipc` from `src-tauri/`; the binary is
`src-tauri/target/release/zipc`.

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
presets, progress, success, cancel and errors.

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
- `scripts/` — corpus and benchmark scripts.
