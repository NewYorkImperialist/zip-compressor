#!/usr/bin/env python3
"""Minimal timing wrapper for scripts/bench.sh, used when hyperfine is absent.

Run with `python3 -I`. Standard library only.

    bench_time.py RUNS PREPARE COMMAND

Runs the shell command PREPARE (untimed) and then COMMAND (timed), RUNS times,
both through `bash -c` in the current directory, and prints the best wall time
in seconds. Exits non-zero, showing COMMAND's stderr, if any run fails.
"""

import subprocess
import sys
import time


def main(argv):
    if len(argv) != 3:
        sys.exit(__doc__)
    runs, prepare, command = int(argv[0]), argv[1], argv[2]
    if runs < 1:
        sys.exit("bench_time: RUNS must be at least 1")
    best = None
    for _ in range(runs):
        if prepare:
            subprocess.run(["bash", "-c", prepare], check=True)
        start = time.perf_counter()
        proc = subprocess.run(
            ["bash", "-c", command],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
        )
        elapsed = time.perf_counter() - start
        if proc.returncode != 0:
            sys.stderr.write(proc.stderr.decode(errors="replace"))
            sys.exit(f"bench_time: command failed with status {proc.returncode}: {command}")
        best = elapsed if best is None else min(best, elapsed)
    print(f"{best:.6f}")


if __name__ == "__main__":
    main(sys.argv[1:])
