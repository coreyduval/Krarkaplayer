#!/usr/bin/env python3
"""Run the sweep and diff against golden.txt (ignoring the timing line).
Usage: python verify/check.py [--games 60 --flip-trials 10]
Exit 0 + prints PASS if byte-identical (modulo timing line); else prints FAIL + diff.
"""
import os, sys, subprocess, difflib

RUST_DIR = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
CARGO_BIN = os.path.expanduser("~/.cargo/bin")
EXE = os.path.join(RUST_DIR, "target", "release", "krarksim.exe")
GOLDEN = os.path.join(RUST_DIR, "verify", "golden.txt")

env = dict(os.environ)
env["PATH"] = env.get("PATH", "") + os.pathsep + CARGO_BIN


def keep(line):
    # drop the header banner (timing-free but run-specific) and timing summary line
    s = line.rstrip("\n")
    if "total," in s and "s/game" in s:
        return False
    if s.startswith("===") or "FLIP-DISTRIBUTION SWEEP" in s or "(seeds" in s:
        return False
    if s.startswith("RUN:"):
        return False
    return True


def main():
    args = sys.argv[1:] or ["--games", "60", "--flip-trials", "10"]
    out = subprocess.run([EXE, "sweep"] + args, cwd=RUST_DIR, env=env,
                         capture_output=True, text=True)
    lines = [l for l in out.stdout.splitlines() if keep(l + "\n")]
    with open(GOLDEN) as f:
        golden = [l.rstrip("\n") for l in f if keep(l)]
    cur = [l for l in lines]
    if cur == golden:
        print("PASS: byte-identical to golden (modulo timing).")
        return 0
    print("FAIL: output diverged from golden:")
    for d in difflib.unified_diff(golden, cur, "golden", "current", lineterm=""):
        print(d)
    return 1


if __name__ == "__main__":
    sys.exit(main())
