#!/usr/bin/env bash
# Benchmark zipc against the system ZIP tools on the corpus in bench-data/.
#
#   scripts/bench.sh [--quick] [--runs N] [--max-big] [--no-docs] [--note TEXT] [DATASET...]
#
#   --quick      one run per command and skip big/ (unless named explicitly);
#                the slow `--preset max` rows run only on source, tiny and photos
#   --runs N     best of N runs per command (default 3)
#   --max-big    also run `--preset max` on big/ (skipped by default: it takes
#                many minutes per run)
#   --no-docs    print the table only; do not rewrite docs/benchmarks.md
#   --note TEXT  add a line of context (e.g. engine state) to docs/benchmarks.md
#   DATASET      any of: silesia source tiny big photos (default: all present)
#
# Build the corpus first with scripts/fetch-corpus.sh. Uses hyperfine when it is
# installed, otherwise scripts/bench_time.py. Every zipc archive is checked with
# `unzip -tq`; an invalid one aborts the run.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DATA="$ROOT/bench-data"
DOCS="$ROOT/docs/benchmarks.md"
ALL=(silesia source tiny big photos)

RUNS=3
QUICK=0
MAX_BIG=0
WRITE_DOCS=1
NOTE=""
DATASETS=()

usage() { sed -n '2,17p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit "${1:-0}"; }
log() { printf '[bench] %s\n' "$*" >&2; }
die() { printf '[bench] error: %s\n' "$*" >&2; exit 1; }

while (($#)); do
  case "$1" in
    --quick) QUICK=1 ;;
    --runs) [[ $# -ge 2 && "$2" =~ ^[1-9][0-9]*$ ]] || die "--runs needs a positive integer"; RUNS="$2"; shift ;;
    --max-big) MAX_BIG=1 ;;
    --no-docs) WRITE_DOCS=0 ;;
    --note) [[ $# -ge 2 ]] || die "--note needs text"; NOTE="$2"; shift ;;
    -h|--help) usage 0 ;;
    -*) printf 'unknown option: %s\n\n' "$1" >&2; usage 2 ;;
    *) DATASETS+=("$1") ;;
  esac
  shift
done

if ((QUICK)); then RUNS=1; fi
if ((${#DATASETS[@]} == 0)); then
  for ds in "${ALL[@]}"; do
    if ((QUICK)) && [[ "$ds" == big ]]; then continue; fi
    if [[ -d "$DATA/$ds" ]]; then DATASETS+=("$ds"); fi
  done
  ((${#DATASETS[@]} > 0)) || die "no datasets in $DATA; run scripts/fetch-corpus.sh first"
fi
for ds in "${DATASETS[@]}"; do
  case " ${ALL[*]} " in *" $ds "*) ;; *) die "unknown dataset '$ds' (choose from: ${ALL[*]})" ;; esac
  [[ -d "$DATA/$ds" ]] || die "bench-data/$ds is missing; run scripts/fetch-corpus.sh $ds"
done
command -v python3 >/dev/null || die "python3 is required"
if ! command -v cargo >/dev/null && [[ -x "${CARGO_HOME:-$HOME/.cargo}/bin/cargo" ]]; then
  PATH="${CARGO_HOME:-$HOME/.cargo}/bin:$PATH"
fi
command -v cargo >/dev/null || die "cargo is required"
command -v unzip >/dev/null || die "unzip is required"

# ---- build ---------------------------------------------------------------
log "building zipc (release)"
(cd "$ROOT/src-tauri" && cargo build --release -p zipc --quiet) || die "cargo build failed"
ZIPC="${CARGO_TARGET_DIR:-$ROOT/src-tauri/target}/release/zipc"
[[ -x "$ZIPC" ]] || die "zipc binary not found at $ZIPC"

# ---- machine ---------------------------------------------------------------
if [[ "$(uname -s)" == Darwin ]]; then
  CPU="$(sysctl -n machdep.cpu.brand_string 2>/dev/null || echo unknown)"
  CORES="$(sysctl -n hw.ncpu)"
  OS="macOS $(sw_vers -productVersion 2>/dev/null || uname -r)"
else
  CPU="$(sed -n 's/^model name[[:space:]]*: //p' /proc/cpuinfo 2>/dev/null | head -n1)"
  CPU="${CPU:-unknown}"
  CORES="$(nproc)"
  OS="$(uname -sr)"
fi
COMMIT="$(git -C "$ROOT" rev-parse --short HEAD 2>/dev/null || echo unknown)"
if [[ -n "$(git -C "$ROOT" status --porcelain --untracked-files=no 2>/dev/null)" ]]; then
  COMMIT="$COMMIT (uncommitted changes)"
fi

if command -v hyperfine >/dev/null; then
  TIMER="hyperfine $(hyperfine --version | awk '{print $2}')"
else
  TIMER="scripts/bench_time.py"
fi

# ---- tools -----------------------------------------------------------------
# Each entry is "label|command". In commands, @IN@ is the dataset dir (relative
# to bench-data/) and @OUT@ the archive path. Commands run from bench-data/.
# The fast and balanced presets are exactly -l 1 and -l 6, so the -l rows
# cover them; best and max have their own rows.
TOOLS=(
  "zipc -l 6 -t 1|'$ZIPC' compress @IN@ -o @OUT@ -l 6 -t 1 -q -f"
  "zipc -l 6 ($CORES threads)|'$ZIPC' compress @IN@ -o @OUT@ -l 6 -q -f"
  "zipc -l 9 -t 1|'$ZIPC' compress @IN@ -o @OUT@ -l 9 -t 1 -q -f"
  "zipc -l 9 ($CORES threads)|'$ZIPC' compress @IN@ -o @OUT@ -l 9 -q -f"
  "zipc --preset best -t 1|'$ZIPC' compress @IN@ -o @OUT@ --preset best -t 1 -q -f"
  "zipc --preset best ($CORES threads)|'$ZIPC' compress @IN@ -o @OUT@ --preset best -q -f"
  "zipc --preset max ($CORES threads)|'$ZIPC' compress @IN@ -o @OUT@ --preset max -q -f"
)
if command -v zip >/dev/null; then
  TOOLS+=("zip -9|zip -qr -9 @OUT@ @IN@" "zip -6|zip -qr -6 @OUT@ @IN@")
else
  log "zip not installed, skipping Info-ZIP"
fi
if [[ "$(uname -s)" == Darwin ]] && command -v ditto >/dev/null; then
  TOOLS+=("ditto -c -k|ditto -c -k --keepParent @IN@ @OUT@")
fi
SEVENZ="$(command -v 7zz || command -v 7z || true)"
if [[ -n "$SEVENZ" ]]; then
  TOOLS+=("$(basename "$SEVENZ") -tzip -mx9|'$SEVENZ' a -tzip -mx9 -bd @OUT@ @IN@ >/dev/null")
else
  log "7zz/7z not installed, skipping 7-Zip"
fi

# ---- run -------------------------------------------------------------------
# Stale work dirs from a killed run would hold whole archives; clear them.
rm -rf "$DATA"/.bench.*
WORK="$(mktemp -d "$DATA/.bench.XXXXXX")"
trap 'rm -rf "$WORK"' EXIT
trap 'exit 130' INT TERM
OUT="$WORK/out.zip"
ROWS="$WORK/rows.tsv"
: >"$ROWS"
SKIPPED=()

# Whether to skip tool $1 on dataset $2 (the slow max preset on large data).
skip_tool() {
  local label="$1" ds="$2"
  [[ "$label" == *"--preset max"* ]] || return 1
  if ((QUICK)); then
    case "$ds" in source|tiny|photos) return 1 ;; *) return 0 ;; esac
  fi
  [[ "$ds" == big ]] && ((!MAX_BIG))
}

# Best wall time in seconds of shell command $1, run from bench-data/.
time_cmd() {
  local cmd="$1" prepare="rm -f '$OUT'"
  if [[ "$TIMER" == hyperfine* ]]; then
    (cd "$DATA" && hyperfine --runs "$RUNS" --prepare "$prepare" --style none \
      --export-json "$WORK/hf.json" "$cmd" >/dev/null) || return 1
    python3 -I -c 'import json,sys; print(min(json.load(open(sys.argv[1]))["results"][0]["times"]))' "$WORK/hf.json"
  else
    (cd "$DATA" && python3 -I "$ROOT/scripts/bench_time.py" "$RUNS" "$prepare" "$cmd")
  fi
}

for ds in "${DATASETS[@]}"; do
  read -r files bytes < <(python3 -I "$ROOT/scripts/gen_corpus.py" stats "$DATA/$ds")
  log "$ds: $files files, $((bytes / 1000000)) MB; warming the page cache"
  find "$DATA/$ds" -type f -exec cat {} + >/dev/null
  for tool in "${TOOLS[@]}"; do
    label="${tool%%|*}"
    cmd="${tool#*|}"
    q_in="'$ds'" q_out="'$OUT'"
    cmd="${cmd//@IN@/$q_in}"
    cmd="${cmd//@OUT@/$q_out}"
    if skip_tool "$label" "$ds"; then
      log "$ds: $label (skipped)"
      SKIPPED+=("$label on $ds")
      continue
    fi
    log "$ds: $label"
    secs="$(time_cmd "$cmd")" || die "$label failed on $ds"
    if ! unzip -tq "$OUT" >"$WORK/unzip.log" 2>&1; then
      cat "$WORK/unzip.log" >&2
      if [[ "$label" == zipc* ]]; then
        die "INVALID ARCHIVE: $label on $ds failed 'unzip -tq'"
      fi
      log "warning: $label on $ds produced an archive unzip rejects"
    fi
    size="$(wc -c <"$OUT" | tr -d ' ')"
    printf '%s\t%s\t%s\t%s\t%s\n' "$ds" "$label" "$bytes" "$size" "$secs" >>"$ROWS"
    rm -f "$OUT"
  done
done

# ---- report ----------------------------------------------------------------
TABLE="$(awk -F'\t' '
  BEGIN {
    print "| Dataset | Tool | Size | Ratio % | Time s | MB/s |"
    print "|---|---|---:|---:|---:|---:|"
  }
  {
    printf "| %s (%.1f MB) | %s | %.2f MB | %.2f | %.3f | %.1f |\n", $1, $3 / 1e6, $2, $4 / 1e6, 100 * $4 / $3, $5, ($5 > 0 ? $3 / 1e6 / $5 : 0)
  }' "$ROWS")"

printf '%s\n' "$TABLE"
if ((${#SKIPPED[@]})); then
  SKIP_NOTE="Not run: $(printf '%s; ' "${SKIPPED[@]}" | sed 's/; $//')."
  if [[ " ${SKIPPED[*]} " == *" on big "* ]] && ((!QUICK)); then
    SKIP_NOTE="$SKIP_NOTE \`--preset max\` on big/ takes many minutes per run; pass \`--max-big\` to include it."
  fi
  printf '\n%s\n' "$SKIP_NOTE"
else
  SKIP_NOTE=""
fi

if ((WRITE_DOCS)); then
  mkdir -p "$(dirname "$DOCS")"
  # Hand-written sections after this marker survive a rewrite.
  KEEP_MARK='<!-- kept by scripts/bench.sh: everything below is preserved -->'
  KEPT=""
  if [[ -f "$DOCS" ]] && grep -qF "$KEEP_MARK" "$DOCS"; then
    KEPT="$(awk -v m="$KEEP_MARK" 'f || $0 == m { f = 1; print }' "$DOCS")"
  fi
  zip_ver="$(zip -v 2>/dev/null | sed -n 's/^This is \(Zip [0-9.]*\).*/\1/p' | head -n1)"
  {
    printf '# Benchmarks\n\n'
    printf 'Generated by `scripts/bench.sh%s%s` — re-run it to refresh this file.\n\n' \
      "$( ((QUICK)) && printf ' --quick' )" "$( ((MAX_BIG)) && printf ' --max-big' )"
    printf -- '- **Date:** %s\n' "$(date -u '+%Y-%m-%d %H:%M UTC')"
    printf -- '- **Machine:** %s, %s logical cores, %s\n' "$CPU" "$CORES" "$OS"
    printf -- '- **Commit:** %s\n' "$COMMIT"
    if [[ -n "$NOTE" ]]; then printf -- '- **Note:** %s\n' "$NOTE"; fi
    printf -- '- **Timing:** best of %s run(s), wall clock, warm page cache (%s)\n' "$RUNS" "$TIMER"
    printf -- '- **Other tools:** %s%s%s\n' "${zip_ver:-Info-ZIP not found}" \
      "$( [[ "$(uname -s)" == Darwin ]] && printf ', ditto' )" \
      "$( [[ -n "$SEVENZ" ]] && printf ', %s' "$(basename "$SEVENZ")" || printf ', 7-Zip not installed' )"
    printf '\n'
    printf 'Size is the archive size; ratio is archive / input bytes (lower is better);\n'
    printf 'MB/s is input megabytes (10^6 bytes) per second. Every zipc archive passed `unzip -tq`.\n\n'
    printf 'zipc presets: `fast` and `balanced` are exactly `-l 1` and `-l 6`; `best` is level 9\n'
    printf 'trying several encoder settings per file (plain `-l 9` is shown for comparison);\n'
    printf '`max` is Zopfli with 15 iterations. Rows without `-t 1` use every core.\n\n'
    printf '%s\n\n' "$TABLE"
    if [[ -n "$SKIP_NOTE" ]]; then printf '%s\n\n' "$SKIP_NOTE"; fi
    cat <<'EOF'
## Datasets

Built by `scripts/fetch-corpus.sh` into `bench-data/` (gitignored):

- **silesia** — the Silesia compression corpus (12 files, 211.9 MB).
- **source** — several crates copied from the local cargo registry (versions vary by machine).
- **tiny** — 20,000 seeded, generated text files of 100 B–4 KiB in nested directories.
- **big** — a 1 GiB file of lightly mutated slices of source/ and silesia/, plus 256 MiB of seeded random bytes.
- **photos** — macOS wallpapers (HEIC/JPG, up to 50 MB); already-compressed data.
EOF
    if [[ -n "$KEPT" ]]; then printf '\n%s\n' "$KEPT"; fi
  } >"$DOCS"
  log "wrote $DOCS"
fi
