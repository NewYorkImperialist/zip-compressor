#!/usr/bin/env python3
"""Deterministic generators for the zipc benchmark corpus.

Run with `python3 -I` (isolated mode). Standard library only.

    gen_corpus.py tiny   OUT_DIR
    gen_corpus.py big    OUT_DIR SEED_DIR...
    gen_corpus.py random OUT_FILE SIZE_BYTES
    gen_corpus.py stats  DIR          # prints "<files> <bytes>"

Every generator is seeded, so the same command always writes the same bytes.
"""

import os
import random
import sys

CHUNK = 1 << 20  # 1 MiB

WORDS = (
    "the of and to in is that for it as with was on be by this are or from at "
    "an which but not have has had were all their one can there been if more "
    "when will would who so no buffer stream archive deflate entry header "
    "central directory offset length window literal match distance huffman "
    "table symbol checksum crc block thread worker queue chunk payload file "
    "path name size time modified extract compress level ratio fast slow error "
    "result option value default config request response client server cache"
).split()


def gen_tiny(out_dir, count=20000, seed=20000):
    """`count` small text files (100 B - 4 KiB) in a nested directory tree."""
    rng = random.Random(seed)
    for i in range(count):
        a, b = divmod(i % 400, 20)
        sub = os.path.join(out_dir, f"pkg{a:02d}", f"mod{b:02d}")
        if i < 400:
            os.makedirs(sub, exist_ok=True)
        size = rng.randint(100, 4096)
        kind = rng.random()
        parts = []
        n = 0
        while n < size:
            if kind < 0.4:  # prose
                line = " ".join(rng.choice(WORDS) for _ in range(rng.randint(5, 14)))
                line = line.capitalize() + ".\n"
            elif kind < 0.75:  # code-like
                ident = "_".join(rng.choice(WORDS) for _ in range(2))
                line = (
                    f"{' ' * (4 * rng.randint(0, 3))}let {ident} = "
                    f"{rng.choice(WORDS)}({rng.randint(0, 9999)});\n"
                )
            else:  # csv-like
                line = ",".join(
                    str(rng.randint(0, 10**rng.randint(1, 7))) for _ in range(8)
                ) + "\n"
            parts.append(line)
            n += len(line)
        data = "".join(parts).encode()[:size]
        ext = ".txt" if kind < 0.4 else (".rs" if kind < 0.75 else ".csv")
        with open(os.path.join(sub, f"f{i:05d}{ext}"), "wb") as f:
            f.write(data)


def _seed_pool(seed_dirs, limit=256 << 20):
    """Concatenate regular files under `seed_dirs` (sorted, capped) into one buffer."""
    paths = []
    for d in seed_dirs:
        for root, dirs, files in os.walk(d):
            dirs.sort()
            for name in sorted(files):
                p = os.path.join(root, name)
                if os.path.isfile(p) and not os.path.islink(p):
                    paths.append(p)
    pool = bytearray()
    for p in paths:
        with open(p, "rb") as f:
            pool += f.read(limit - len(pool))
        if len(pool) >= limit:
            break
    if len(pool) < CHUNK:
        sys.exit("gen_corpus: seed material too small for big/ (need >= 1 MiB)")
    return bytes(pool)


def gen_big(out_file, seed_dirs, size=1 << 30, seed=1):
    """~1 GiB of real text/binary slices from `seed_dirs`, each lightly mutated."""
    rng = random.Random(seed)
    pool = _seed_pool(seed_dirs)
    written = 0
    with open(out_file, "wb") as f:
        while written < size:
            n = min(rng.randint(64 << 10, 1 << 20), size - written)
            start = rng.randrange(0, len(pool) - n + 1) if len(pool) > n else 0
            piece = bytearray(pool[start : start + n])
            # About one changed byte per 2 KiB, so copies are never exact repeats.
            for _ in range(max(1, n >> 11)):
                piece[rng.randrange(len(piece))] = rng.randrange(256)
            f.write(piece)
            written += len(piece)


def gen_random(out_file, size, seed=256):
    rng = random.Random(seed)
    with open(out_file, "wb") as f:
        left = size
        while left:
            n = min(CHUNK * 8, left)
            f.write(rng.randbytes(n))
            left -= n


def stats(d):
    files = total = 0
    for root, _dirs, names in os.walk(d):
        for name in names:
            p = os.path.join(root, name)
            if os.path.isfile(p) and not os.path.islink(p):
                files += 1
                total += os.path.getsize(p)
    print(files, total)


def main(argv):
    if len(argv) >= 2 and argv[0] == "tiny":
        gen_tiny(argv[1])
    elif len(argv) >= 3 and argv[0] == "big":
        gen_big(argv[1], argv[2:])
    elif len(argv) == 3 and argv[0] == "random":
        gen_random(argv[1], int(argv[2]))
    elif len(argv) == 2 and argv[0] == "stats":
        stats(argv[1])
    else:
        sys.exit(__doc__)


if __name__ == "__main__":
    main(sys.argv[1:])
