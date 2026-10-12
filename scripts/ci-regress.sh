#!/usr/bin/env bash
# Size and speed regression check for CI (and local use).
#
#   scripts/ci-regress.sh ZIPC [--update]
#
# Compresses bench-data/silesia and bench-data/tiny (build them with
# scripts/fetch-corpus.sh silesia tiny) with each preset and compares the
# archive sizes with scripts/ci-baseline.tsv. zipc's output is deterministic,
# so any size change is a real change in the encoder:
#
#   - larger than the baseline by more than SIZE_SLACK_PPM: failure
#   - smaller by more than SIZE_SLACK_PPM: a notice to refresh the baseline
#   - slower than the baseline's time cap: failure (the caps are generous
#     upper bounds for a shared CI runner, meant to catch large slowdowns,
#     not small ones; timing on CI is too noisy for more)
#
# Every archive must pass `zipc test` and `unzip -tq`. --update rewrites the
# baseline sizes with the ones just measured (time caps are kept).
# With GITHUB_STEP_SUMMARY set, a results table is appended to it.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DATA="$ROOT/bench-data"
BASELINE="$ROOT/scripts/ci-baseline.tsv"
# Slack for platform differences in the encoder (parts per million).
SIZE_SLACK_PPM=1000

log() { printf '[ci-regress] %s\n' "$*" >&2; }
die() { printf '[ci-regress] error: %s\n' "$*" >&2; exit 1; }

(($# >= 1)) || die "usage: ci-regress.sh ZIPC [--update]"
ZIPC="$1"
UPDATE=0
[[ "${2:-}" == "--update" ]] && UPDATE=1
[[ -x "$ZIPC" ]] || die "$ZIPC is not executable"
[[ -f "$BASELINE" ]] || die "missing $BASELINE"
command -v unzip >/dev/null || die "unzip is required"

OUT="$(mktemp -d)"
trap 'rm -rf "$OUT"' EXIT

now() { python3 -I -c 'import time; print(time.monotonic())'; }
file_size() { wc -c <"$1" | tr -d ' '; }

failed=0
rows=()
new_baseline=()
while IFS= read -r line; do
  if [[ -z "$line" || "$line" == \#* ]]; then
    new_baseline+=("$line")
    continue
  fi
  IFS=$'\t' read -r dataset preset bytes cap <<<"$line"
  [[ -d "$DATA/$dataset" ]] || die "bench-data/$dataset is missing; run scripts/fetch-corpus.sh $dataset"
  zip="$OUT/$dataset-$preset.zip"
  log "$dataset / $preset"
  start="$(now)"
  "$ZIPC" compress -q -p "$preset" -o "$zip" "$DATA/$dataset" >&2
  end="$(now)"
  secs="$(python3 -I -c "print(f'{$end - $start:.2f}')")"
  "$ZIPC" test "$zip" >/dev/null || { log "FAIL: zipc test rejected $dataset/$preset"; failed=1; }
  unzip -tq "$zip" >/dev/null || { log "FAIL: unzip -t rejected $dataset/$preset"; failed=1; }
  size="$(file_size "$zip")"
  rm -f "$zip"

  delta_ppm=$(( (size - bytes) * 1000000 / bytes ))
  status=ok
  if ((delta_ppm > SIZE_SLACK_PPM)); then
    status="LARGER"; failed=1
    log "FAIL: $dataset/$preset is $size bytes, baseline $bytes (+$delta_ppm ppm)"
  elif ((delta_ppm < -SIZE_SLACK_PPM)); then
    status="smaller (update baseline)"
    log "notice: $dataset/$preset is $size bytes, baseline $bytes ($delta_ppm ppm); run with --update"
  fi
  if python3 -I -c "import sys; sys.exit(0 if $secs <= $cap else 1)"; then :; else
    status="$status, SLOW"; failed=1
    log "FAIL: $dataset/$preset took ${secs}s, cap ${cap}s"
  fi
  rows+=("| $dataset | $preset | $size | $bytes | $delta_ppm | $secs | $cap | $status |")
  new_baseline+=("$dataset"$'\t'"$preset"$'\t'"$size"$'\t'"$cap")
done <"$BASELINE"

table="| Dataset | Preset | Bytes | Baseline | Δ ppm | Time s | Cap s | Status |
|---|---|---:|---:|---:|---:|---:|---|"
for r in "${rows[@]}"; do table+=$'\n'"$r"; done
printf '%s\n' "$table"
if [[ -n "${GITHUB_STEP_SUMMARY:-}" ]]; then
  printf '### Size and speed regression (%s)\n\n%s\n\n' "${RUNNER_OS:-local}" "$table" >>"$GITHUB_STEP_SUMMARY"
fi

if ((UPDATE)); then
  printf '%s\n' "${new_baseline[@]}" >"$BASELINE"
  log "baseline updated"
  exit 0
fi
exit "$failed"
