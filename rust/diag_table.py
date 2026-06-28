#!/usr/bin/env python3
"""Clean tabular view of a `krarksim diag` game.

Runs `./target/release/krarksim diag --seed N [extra...]`, parses the verbose trace,
and prints a per-turn table (Drew / Land / Plays / Lost) where Drew lists every card drawn
that turn (draw step + all develop draws/tutors), each play shows how many times it was
attempted (xN) and its coin-flip results (wins/flips), and Lost lists every card discarded
or exiled from hand that turn. Also prints the go-off sequence.

Note: the go-off/kill line is computed by the planner and shown separately; cards a kill line
dumps from hand (e.g. Lion's Eye Diamond's "discard your hand") are not itemized per-turn.

Usage:  python diag_table.py <seed> [extra diag flags...]
"""
import re, subprocess, sys, collections
sys.stdout.reconfigure(encoding="utf-8", errors="replace")

seed = sys.argv[1] if len(sys.argv) > 1 else "0"
extra = sys.argv[2:]
raw = subprocess.run(
    ["./target/release/krarksim", "diag", "--seed", seed, *extra],
    capture_output=True, text=True, encoding="utf-8", errors="replace",
).stdout.splitlines()

CAST = re.compile(r"^\s*CAST\s*:\s*(.+?)\s*\(from (\w+)\)")
DEV = re.compile(r"^\s*DEV\s*\d+:\s*(.+?)(?:\s*\((\d+)/(\d+)\))?\s*$")
FLIP = re.compile(r"^\s*FLIP\s*\d+:\s*(.+?)\s*\((\d+)/(\d+)\)")
DEPLOY = re.compile(r"^\s*DEPLOY\s*:\s*(.+?)\s*\((engine permanent|from (\w+))\)")

turns, cur = [], None
opening = win_turn = win_detail = ""
in_goff = False
goff_hdr = ""
# Deterministic-kill detail block (SETUP actions + KILL-LINE walkthrough + final board).
seen_win = False
win_mode = None
win_setup, win_kill, win_board = [], [], ""

def fresh(n):
    return dict(n=n, hand="", draws=[], land="", single=[], dev=collections.OrderedDict(),
                deploy=[], jeska_cards=[], lost=[])

for ln in raw:
    s = ln.strip()
    if s.startswith("OPENING"):
        opening = s.split(":", 1)[1].strip()
    elif ln.startswith("=== TURN"):
        if cur: turns.append(cur)
        cur = fresh(int(re.search(r"TURN (\d+)", ln).group(1)))
        in_goff = False
    elif "=== WIN" in ln:
        win_turn = re.search(r"turn (\d+)", ln).group(1)
        seen_win = True
    elif (s.startswith("[P(win)") or s.startswith("[KILL]")) and not win_detail:
        win_detail = s
    elif seen_win and s.startswith("SETUP"):
        win_mode = "setup"
    elif seen_win and s.startswith("KILL-LINE"):
        win_mode = "kill"
    elif seen_win and s.startswith("BOARD"):
        win_board = s.split(":", 1)[1].strip(); win_mode = None
    elif seen_win and s.startswith(("ZONES", "PAYOFFS", "COMBO", "MANA-ENG", "BURN", "GY(", "HAND")):
        win_mode = None
    elif seen_win and win_mode == "setup" and s.startswith("-"):
        win_setup.append(s.lstrip("- ").strip())
    elif seen_win and win_mode == "kill" and s:
        win_kill.append(ln.rstrip())  # preserve walkthrough indentation
    elif s.startswith("GO-OFF  :"):
        in_goff = True
        goff_hdr = s.split(":", 1)[1].strip()
        cur and cur.setdefault("goff", [])
        if cur is not None: cur["goff"] = []
    elif cur is None:
        continue
    elif s.startswith("HAND") and not cur["hand"]:
        # first HAND line per turn = the start-of-turn (post-draw) hand; ignore the win-block HAND
        cur["hand"] = s.split(":", 1)[1].strip()
    elif s.startswith("DRAW"):
        cur["draws"] += [x.strip() for x in s.split(":", 1)[1].split(",") if x.strip()]
    elif s.startswith("LAND"):
        cur["land"] = s.split(":", 1)[1].strip()
    elif (m := CAST.match(ln)):
        cur["single"].append(f"{m.group(1)} [{m.group(2)}]")
    elif (m := DEPLOY.match(ln)):
        if m.group(3):  # "from <zone>" deploy (e.g. Sakashima from command) — tag like a CAST
            cur["single"].append(f"{m.group(1)} [{m.group(3)}]")
        else:           # engine-permanent deploy — keep grouped under "deploy ..."
            cur["deploy"] += [x.strip() for x in m.group(1).split(",")]
    elif s.startswith("DUG"):
        cur["draws"] += [x.strip() for x in s.split(":", 1)[1].split(",") if x.strip()]
    elif s.startswith("EXILE") and "play-this-turn" in s:
        cur["jeska_cards"] += [x.strip() for x in s.split("play-this-turn", 1)[1].split(",") if x.strip()]
    elif s.startswith("IMPRINT"):
        if (m2 := re.search(r"exiles (.+?) \(", s)):
            cur["lost"].append(f"{m2.group(1).strip()} (exiled)")
    elif s.startswith("PITCH"):
        if (m2 := re.search(r"discards (.+?) \(", s)):
            cur["lost"].append(f"{m2.group(1).strip()} (discarded)")
    elif s.startswith("DISCARD"):
        val = re.sub(r"\s*\(.*\)\s*$", "", s.split(":", 1)[1].strip())
        cur["lost"] += [f"{x.strip()} (discarded)" for x in val.split(",") if x.strip()]
    elif (m := FLIP.match(ln)) and in_goff:
        cur.setdefault("goff", []).append((m.group(1), m.group(2), m.group(3)))
    elif (m := DEV.match(ln)) and not in_goff:
        card, w, f = m.group(1), m.group(2), m.group(3)
        d = cur["dev"].setdefault(card, [])
        if w is not None: d.append(f"{w}/{f}")
        else: d.append(None)
if cur: turns.append(cur)

def plays_cell(t):
    parts = list(t["single"])
    for card, flips in t["dev"].items():
        n = len(flips)
        fl = [x for x in flips if x]
        tag = f" x{n}" if n > 1 else ""
        parts.append(f"{card}{tag}" + (f" ({', '.join(fl)})" if fl else ""))
    if t["deploy"]:
        parts.append("deploy " + ", ".join(t["deploy"]))
    if t["jeska_cards"]:
        parts.append("Jeska's Will -> exile [" + ", ".join(t["jeska_cards"]) + "] to play")
    return "; ".join(parts) if parts else "-"

print(f"Game seed={seed}")
print(f"Opening: {opening}\n")
print("| Turn | Hand @ start | Drew | Land | Plays (xN = attempts, x/y = flip wins/flips) | Lost (discard/exile) |")
print("|---|---|---|---|---|---|")
for t in turns:
    drew = ", ".join(t["draws"]) if t["draws"] else "-"
    lost = ", ".join(t["lost"]) if t["lost"] else "-"
    print(f"| {t['n']} | {t['hand'] or '-'} | {drew} | {t['land'] or '-'} | {plays_cell(t)} | {lost} |")

if win_turn:
    print(f"\nWin — turn {win_turn}: {win_detail}")
    if win_kill:
        # Deterministic kill: show the full combo walkthrough (more detail than the per-turn rows).
        if win_setup:
            print(f"Setup: {' · '.join(win_setup)}")
        for line in win_kill:
            print(line)
        if win_board:
            print(f"Board at kill: {win_board}")
    else:
        goff = next((t['goff'] for t in turns if t.get('goff')), None)
        if goff:
            payoff = goff[0][0]
            ratios = ", ".join(f"{w}/{f}" for _, w, f in goff)
            print(f"Go-off: {goff_hdr}")
            print(f"  {payoff} x{len(goff)} — flips: {ratios}")
