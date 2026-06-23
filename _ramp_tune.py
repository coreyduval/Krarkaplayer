"""Sweep ramp-gate modes and score each on win speed without stalling.

Objective: PENALIZED MEAN TURN = mean over all games of (win turn, or LOSS_PENALTY if no
win). Rewards fast wins, punishes stalls. Reported alongside win% and avg-win-turn so the
speed/win-rate tradeoff is visible. _ramp_mode() reads KRARK_RAMP live, so each worker just
sets the env before a game — no module reload needed.
"""
import os, sys, time, statistics
import multiprocessing as mp
import sim

N_GAMES = int(os.environ.get("TUNE_GAMES", "200"))
LOSS_PENALTY = 25          # a stall (no win in 20) counts as turn 25
MODES = ["off", "stuck", "stuck2", "sink", "uncond"]


def _run_one(args):
    mode, seed = args
    os.environ["KRARK_RAMP"] = mode            # _ramp reads this live
    r = sim._play_quiet(seed, 20, 0.75)
    return (r["won"], r["turn"])


def sweep(mode, n, workers):
    tasks = [(mode, s) for s in range(1, n + 1)]
    with mp.Pool(workers) as pool:
        res = pool.map(_run_one, tasks)
    wins = [t for w, t in res if w]
    pen = [(t if w else LOSS_PENALTY) for w, t in res]
    return {
        "mode": mode, "win%": 100 * len(wins) / n,
        "avg_win_turn": statistics.mean(wins) if wins else float("nan"),
        "median_win": statistics.median(wins) if wins else float("nan"),
        "penalized_mean": statistics.mean(pen),
    }


if __name__ == "__main__":
    workers = os.cpu_count() or 1
    modes = sys.argv[1:] or MODES
    print(f"games={N_GAMES}  loss_penalty={LOSS_PENALTY}  workers={workers}\n")
    print(f"{'mode':>8} | {'win%':>5} | {'avg_win':>7} | {'median':>6} | {'penalized_mean':>14}")
    print("-" * 56)
    rows = []
    for m in modes:
        t0 = time.time()
        row = sweep(m, N_GAMES, workers)
        rows.append(row)
        print(f"{row['mode']:>8} | {row['win%']:>5.0f} | {row['avg_win_turn']:>7.2f} | "
              f"{row['median_win']:>6.0f} | {row['penalized_mean']:>14.3f}   ({time.time()-t0:.0f}s)")
    best = min(rows, key=lambda r: r["penalized_mean"])
    print("-" * 56)
    print(f"  best by penalized_mean: {best['mode']}  ({best['penalized_mean']:.3f})")
