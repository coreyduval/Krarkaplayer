#!/usr/bin/env python3
"""Helper to drive cargo / the krarksim binary with cargo on PATH.
Usage:
  python run.py build
  python run.py selftest
  python run.py test
  python run.py exe <args...>        # run target/release/krarksim.exe with args
  python run.py cargo <args...>      # arbitrary cargo invocation
"""
import os, sys, subprocess

RUST_DIR = os.path.dirname(os.path.abspath(__file__))
CARGO_BIN = os.path.expanduser("~/.cargo/bin")
CARGO = os.path.join(CARGO_BIN, "cargo.exe")
EXE = os.path.join(RUST_DIR, "target", "release", "krarksim.exe")

env = dict(os.environ)
env["PATH"] = env.get("PATH", "") + os.pathsep + CARGO_BIN


def run(cmd):
    print("RUN:", " ".join(cmd), flush=True)
    r = subprocess.run(cmd, cwd=RUST_DIR, env=env)
    return r.returncode


def main():
    if len(sys.argv) < 2:
        print(__doc__)
        return 2
    mode = sys.argv[1]
    rest = sys.argv[2:]
    if mode == "build":
        return run([CARGO, "build", "--release"] + rest)
    if mode == "test":
        return run([CARGO, "test", "--release"] + rest)
    if mode == "selftest":
        rc = run([CARGO, "build", "--release"])
        if rc:
            return rc
        return run([EXE, "selftest"] + rest)
    if mode == "exe":
        return run([EXE] + rest)
    if mode == "cargo":
        return run([CARGO] + rest)
    print("unknown mode", mode)
    return 2


if __name__ == "__main__":
    sys.exit(main())
