#!/usr/bin/env bash
# Build the benchmark corpus in <repo>/bench-data/ (gitignored).
#
#   scripts/fetch-corpus.sh [DATASET...]     # default: all datasets
#
# Datasets: silesia source tiny big photos. Each one is built in a scratch dir
# and moved into place only when complete, so re-running skips finished
# datasets. Delete bench-data/<name> to rebuild one.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SCRIPTS="$ROOT/scripts"
DATA="$ROOT/bench-data"
GEN=(python3 -I "$SCRIPTS/gen_corpus.py")

SILESIA_URL="https://sun.aei.polsl.pl/~sdeor/corpus/silesia.zip"
SILESIA_MIRROR="https://raw.githubusercontent.com/MiloszKrajewski/SilesiaCorpus/master"
# name:size for the 12 Silesia files (211,938,580 bytes in total).
SILESIA_FILES=(
  dickens:10192446 mozilla:51220480 mr:9970564 nci:33553445
  ooffice:6152192 osdb:10085684 reymont:6627202 samba:21606400
  sao:7251944 webster:41458703 x-ray:8474240 xml:5345280
)
SOURCE_CRATES=(tokio syn clap_builder tauri zip serde libc regex-automata rustix time zerocopy objc2-foundation)
PHOTO_CAP=$((50 * 1000 * 1000))

log() { printf '[fetch-corpus] %s\n' "$*"; }
die() { printf '[fetch-corpus] error: %s\n' "$*" >&2; exit 1; }

for tool in curl unzip python3; do
  command -v "$tool" >/dev/null || die "$tool is required"
done

mkdir -p "$DATA"
SCRATCH=""
cleanup() { if [[ -n "$SCRATCH" ]]; then rm -rf "$SCRATCH"; fi; }
trap cleanup EXIT

# Start a dataset in a fresh scratch dir; returns 1 if it already exists.
begin() {
  if [[ -d "$DATA/$1" ]]; then
    log "$1: already present ($(stats "$DATA/$1")), skipping"
    return 1
  fi
  SCRATCH="$(mktemp -d "$DATA/.build-$1.XXXXXX")"
  log "$1: building"
}

# Publish the scratch dir as bench-data/<name>.
finish() {
  mv "$SCRATCH/out" "$DATA/$1"
  rm -rf "$SCRATCH"
  SCRATCH=""
  log "$1: done ($(stats "$DATA/$1"))"
}

stats() {
  local files bytes
  read -r files bytes < <("${GEN[@]}" stats "$1")
  printf '%s files, %s MB' "$files" "$((bytes / 1000000))"
}

file_size() { wc -c <"$1" | tr -d ' '; }

# Check that $1 holds exactly the 12 Silesia files with the right sizes.
verify_silesia() {
  local dir="$1" entry name size count
  count="$(find "$dir" -mindepth 1 | wc -l | tr -d ' ')"
  [[ "$count" == "${#SILESIA_FILES[@]}" ]] || { log "silesia: expected ${#SILESIA_FILES[@]} entries, found $count"; return 1; }
  for entry in "${SILESIA_FILES[@]}"; do
    name="${entry%%:*}" size="${entry#*:}"
    [[ -f "$dir/$name" && ! -L "$dir/$name" ]] || { log "silesia: missing $name"; return 1; }
    [[ "$(file_size "$dir/$name")" == "$size" ]] || { log "silesia: $name has the wrong size"; return 1; }
  done
}

build_silesia() {
  begin silesia || return 0
  local dl="$SCRATCH/download" ex="$SCRATCH/extract" entry name
  mkdir "$dl" "$ex" "$SCRATCH/out"
  # The downloads are untrusted: each is unpacked into a fresh, empty directory
  # and only the expected regular files are moved out of it.
  log "silesia: downloading $SILESIA_URL"
  if curl -fsSL --retry 2 --connect-timeout 20 -o "$dl/silesia.zip" "$SILESIA_URL"; then
    unzip -q "$dl/silesia.zip" -d "$ex" || die "silesia: cannot extract the archive"
  else
    log "silesia: $SILESIA_URL unreachable, trying the GitHub mirror"
    for entry in "${SILESIA_FILES[@]}"; do
      name="${entry%%:*}"
      curl -fsSL --retry 2 --connect-timeout 20 -o "$dl/$name.zip" "$SILESIA_MIRROR/$name.zip" \
        || die "silesia: cannot download $name from the mirror"
      mkdir "$ex/$name.d"
      unzip -q "$dl/$name.zip" -d "$ex/$name.d" || die "silesia: cannot extract $name.zip"
      [[ -f "$ex/$name.d/$name" && ! -L "$ex/$name.d/$name" ]] || die "silesia: $name.zip lacks $name"
      mv "$ex/$name.d/$name" "$ex/$name" && rm -rf "$ex/$name.d"
    done
  fi
  verify_silesia "$ex" || die "silesia: downloaded files failed verification"
  for entry in "${SILESIA_FILES[@]}"; do
    mv "$ex/${entry%%:*}" "$SCRATCH/out/"
  done
  finish silesia
}

# Newest version of crate $1 in the cargo registry, or nothing.
latest_crate() {
  local reg="${CARGO_HOME:-$HOME/.cargo}/registry/src"
  python3 -I - "$reg" "$1" <<'PY'
import glob, os, re, sys
reg, name = sys.argv[1], sys.argv[2]
best = None
for p in glob.glob(os.path.join(reg, "*", name + "-*")):
    m = re.fullmatch(re.escape(name) + r"-(\d+)\.(\d+)\.(\d+)", os.path.basename(p))
    if m and os.path.isdir(p):
        key = tuple(map(int, m.groups()))
        if best is None or key > best[0]:
            best = (key, p)
if best:
    print(best[1])
PY
}

build_source() {
  begin source || return 0
  mkdir "$SCRATCH/out"
  local crate dir copied=0
  for crate in "${SOURCE_CRATES[@]}"; do
    dir="$(latest_crate "$crate")"
    if [[ -z "$dir" ]]; then
      log "source: $crate not in the cargo registry, skipping"
      continue
    fi
    cp -R "$dir" "$SCRATCH/out/"
    log "source: copied $(basename "$dir")"
    copied=$((copied + 1))
  done
  ((copied > 0)) || die "source: no crates found under ~/.cargo/registry/src (run a cargo build first)"
  finish source
}

build_tiny() {
  begin tiny || return 0
  mkdir "$SCRATCH/out"
  "${GEN[@]}" tiny "$SCRATCH/out"
  finish tiny
}

build_big() {
  begin big || return 0
  mkdir "$SCRATCH/out"
  local seeds=()
  [[ -d "$DATA/source" ]] && seeds+=("$DATA/source")
  [[ -d "$DATA/silesia" ]] && seeds+=("$DATA/silesia")
  ((${#seeds[@]} > 0)) || die "big: needs silesia/ or source/ as seed material; build those first"
  log "big: generating text.bin (1 GiB) from ${seeds[*]##*/}"
  "${GEN[@]}" big "$SCRATCH/out/text.bin" "${seeds[@]}"
  log "big: generating random.bin (256 MiB)"
  "${GEN[@]}" random "$SCRATCH/out/random.bin" $((256 << 20))
  finish big
}

build_photos() {
  local dirs=() d
  for d in "/System/Library/Desktop Pictures" /System/Library/CoreServices; do
    [[ -d "$d" ]] && dirs+=("$d")
  done
  if ((${#dirs[@]} == 0)); then
    log "photos: no system wallpapers on this machine, skipping"
    return 0
  fi
  begin photos || return 0
  mkdir "$SCRATCH/out"
  # Smallest first, skipping duplicates (CoreServices links into Desktop Pictures),
  # until the next file would take the set past PHOTO_CAP.
  local total=0 size path base seen=" " n=0
  while IFS=$'\t' read -r size path; do
    base="$(basename "$path")"
    case "$seen" in *" $size "*) continue ;; esac
    ((total + size <= PHOTO_CAP)) || break
    seen+="$size "
    cp -L "$path" "$SCRATCH/out/$base"
    total=$((total + size)) n=$((n + 1))
  done < <(find -L "${dirs[@]}" -maxdepth 3 -type f \( -iname '*.heic' -o -iname '*.jpg' \) \
             -not -path '*/.thumbnails/*' -size +100k 2>/dev/null \
           | while IFS= read -r p; do printf '%s\t%s\n' "$(file_size "$p")" "$p"; done \
           | sort -n)
  if ((n == 0)); then
    log "photos: no HEIC/JPG wallpapers found, skipping"
    rm -rf "$SCRATCH"
    SCRATCH=""
    return 0
  fi
  finish photos
}

ALL=(silesia source tiny big photos)
if (($# == 0)); then set -- "${ALL[@]}"; fi
for ds in "$@"; do
  case "$ds" in
    silesia|source|tiny|big|photos) "build_$ds" ;;
    *) die "unknown dataset '$ds' (choose from: ${ALL[*]})" ;;
  esac
done
log "corpus ready in $DATA"
