"""
_brick_diag.py — reusable BRICK DIAGNOSIS harness for the Krark/Sakashima solitaire sim.

Given a seed range, it
  (a) sweeps each seed at flip-depth 8 / max-turns 20 (the user-fixed params),
  (b) for every hard-brick (win 0%) and near-brick (<=2/8) seed, finds a bricking
      coin-flip LUCK, reproduces that game to turn 20, and inspects every zone,
  (c) classifies the failure into ONE primary mode and tags higher-level patterns,
  (d) decides VARIANCE vs FIXABLE and flags MYSTERY win-detection gaps,
  (e) prints a categorized report + tallies + ranked fixable levers.

It imports the sim read-only and MODIFIES NO OTHER FILE.

Usage:
    python _brick_diag.py <base> <count> [depth] [max_turns]
        base       first seed (default 6000)
        count      number of seeds to sweep (default 1000)
        depth      flip-trials per seed for the sweep (default 8)
        max_turns  game turn cap (default 20)
    python _brick_diag.py --seed S         # diagnose a single seed (validation)

Examples:
    python _brick_diag.py 6000 1000        # sweep 6000..6999 at depth 8
    python _brick_diag.py --seed 6044      # reproduce the known brick's diagnosis
"""
from __future__ import annotations

import contextlib
import io
import multiprocessing
import os
import random
import re
import sys
import time
from collections import Counter

import sim
from game_state import ManaPool

# ── card categories (names exactly as they appear in krarkashima.txt) ────────────
PAYOFFS  = {"Thassa's Oracle", "Grapeshot", "Brain Freeze"}
COMBO    = {"Twinflame", "Heat Shimmer", "Dualcaster Mage"}
MANAENG  = {"Storm-Kiln Artist", "Archmage Emeritus", "Birgi, God of Storytelling",
            "Jeska's Will", "Strike It Rich", "Rite of Flame", "Pyretic Ritual",
            "Desperate Ritual"}
BURN     = {"Urabrask", "Vivi Ornitier"}
DIG      = {"Brainstorm", "Ponder", "Peek", "Gitaxian Probe", "Frantic Search",
            "Borne Upon a Wind", "Opt", "Consider", "Serum Visions", "Preordain",
            "Gale, Waterdeep Prodigy", "Archmage Emeritus"}
BODIES   = {"Krark, the Thumbless", "Sakashima of a Thousand Faces",
            "Zndrsplt, Eye of Wisdom", "Vivi Ornitier"}
DOUBLERS = {"Veyran, Voice of Duality", "Harmonic Prowess", "Krark's Thumb"}
TUTORS   = {"Imperial Recruiter", "Spellseeker", "Gamble"}

DEPTH = 8
MAXT = 20


# ── parallel sweep over coin-flip luck (mirrors sim._play_quiet_luck) ─────────────
def _luck_win(args):
    """Run seed `seed`'s fixed deck/opening under one coin-flip `luck`; return win turn or None."""
    seed, luck, max_turns = args
    with open(os.devnull, "w") as dn, contextlib.redirect_stdout(dn):
        g = sim.SimGame(rng_seed=seed)
        g.dev_rng = random.Random(seed * 1_000_003 + luck)
        line = None
        for _ in range(max_turns):
            line = g.play_turn()
            if line:
                break
    return (seed, luck, (g.turn if line else None))


def sweep(base, count, depth=DEPTH, max_turns=MAXT, workers=0):
    """Depth-`depth` flip sweep over seeds base..base+count-1. Returns {seed: [win_turn_or_None]*depth}."""
    workers = workers or (os.cpu_count() or 1)
    seeds = list(range(base, base + count))
    tasks = [(s, k, max_turns) for s in seeds for k in range(depth)]
    workers = max(1, min(workers, len(tasks)))
    t0 = time.time()
    if workers == 1:
        results = [_luck_win(t) for t in tasks]
    else:
        with multiprocessing.Pool(workers) as pool:
            results = pool.map(_luck_win, tasks)
    by_seed = {s: [] for s in seeds}
    for s, k, turn in results:
        by_seed[s].append(turn)
    elapsed = time.time() - t0
    return by_seed, elapsed


# ── reproduce one bricking game with full verbose log + final state ──────────────
def reproduce(seed, luck, max_turns=MAXT):
    """Replay seed/luck capturing the verbose log; return (log_text, final GameState, game)."""
    buf = io.StringIO()
    with contextlib.redirect_stdout(buf):
        g = sim.SimGame(rng_seed=seed)
        g.dev_rng = random.Random(seed * 1_000_003 + luck)
        line = None
        for _ in range(max_turns):
            line = g.play_turn()
            if line:
                break
    pool = ManaPool(treasures=g.treasures)
    state = g._build_state(pool)
    return buf.getvalue(), state, g, (line is not None)


def find_brick_luck(seed, depth, max_turns=MAXT):
    """Find a coin-flip luck in 0..depth*4 that bricks (no win). Returns luck or None."""
    for luck in range(depth * 4):
        if _luck_win((seed, luck, max_turns))[2] is None:
            return luck
    return None


# ── inspection: pull the diagnostic readout from log + final state ───────────────
def _board_engine_names(state):
    eng = []
    for p in state.battlefield:
        nm = p.effective_name
        if nm in BODIES or nm in DOUBLERS or nm in MANAENG or nm in BURN or nm in PAYOFFS:
            eng.append(nm)
    return eng


def inspect(seed, luck, max_turns=MAXT):
    log, state, g, won = reproduce(seed, luck, max_turns)
    lib_names = set(state.library)
    seen = set(state.hand) | set(state.graveyard) | set(g._board_names())
    # also count cards seen anywhere across the game from the log (drawn / cast / tutored)
    # last CHECK line -> flips/cast etc.
    checks = re.findall(
        r"CHECK\s+:\s+(.*?)\s+\(bodies=(\d+)\s+doublers=(\d+)\s+flips/cast=(\d+)\s+devotion=(\d+)\)",
        log)
    last_check = checks[-1] if checks else None
    tutors = re.findall(r"TUTOR\s+:\s+(.*)", log)
    dig_cast = [d for d in DIG if re.search(r"(DEVELOP|CAST)\s+:.*\b" + re.escape(d), log)]
    # any dig at all (cantrip) cast during develop
    any_dig = bool(dig_cast)

    info = {
        "seed": seed, "luck": luck, "won": won,
        "lib_left": len(state.library),
        "gy_size": len(state.graveyard),
        "treasures": g.treasures,
        "opp_life": sum(state.opponent_life),
        "krark_bodies": state.krark_bodies,
        "doublers": state.trigger_doublers,
        "flips_per_cast": state.flips_per_cast,
        "devotion": state.blue_devotion,
        "last_check": last_check,
        "tutors": tutors,
        "dig_cast": dig_cast,
        "any_dig": any_dig,
        "board_engines": _board_engine_names(state),
        "hand": list(state.hand),
        "gy": list(state.graveyard),
        # category SEEN vs STILL-IN-LIBRARY
        "payoffs_seen": sorted(PAYOFFS & seen),
        "payoffs_lib": sorted(PAYOFFS & lib_names),
        "combo_seen": sorted(COMBO & seen),
        "combo_lib": sorted(COMBO & lib_names),
        "manaeng_seen": sorted(MANAENG & seen),
        "manaeng_lib": sorted(MANAENG & lib_names),
        "burn_seen": sorted(BURN & seen),
        "burn_lib": sorted(BURN & lib_names),
        "dig_seen": sorted(DIG & seen),
        "dig_lib": sorted(DIG & lib_names),
        "lands_on_board": sum(1 for n in g._board_names() if n in sim.LANDS),
        "log": log,
    }
    # check whether a castable payoff is actually in hand/board (for ORACLE-TRAP / MYSTERY)
    info["oracle_in_hand"] = "Thassa's Oracle" in state.hand or state.has_permanent("Thassa's Oracle")
    info["payoff_in_hand"] = any(p in state.hand for p in PAYOFFS) or \
        any(state.has_permanent(p) for p in PAYOFFS)
    return info


# ── classification ───────────────────────────────────────────────────────────────
def classify(info):
    """Return (primary_mode, [tags], verdict, evidence)."""
    tags = []
    lib = info["lib_left"]
    gy = info["gy_size"]
    bodies = info["krark_bodies"]
    fpc = info["flips_per_cast"]
    lands = info["lands_on_board"]
    dev = info["devotion"]
    no_line = (info["last_check"] is None) or (info["last_check"][0].strip() == "no line")

    payoff_seen = bool(info["payoffs_seen"] or info["combo_seen"])
    payoff_in_lib = bool(info["payoffs_lib"])
    dig_in_lib = bool(info["dig_lib"])

    # ── higher-level tags ──
    # ORACLE-TRAP: holds Oracle but library >> devotion and no dig/mill to empty it.
    if info["oracle_in_hand"] and lib > dev + 5 and not info["any_dig"]:
        tags.append("ORACLE-TRAP")
    # TUTOR-MISFETCH: a tutor fetched a dead/uncashable card (Oracle with no dig, etc.)
    for fetch in info["tutors"]:
        if "Thassa's Oracle" in fetch and not info["any_dig"] and lib > dev + 5:
            tags.append(f"TUTOR-MISFETCH({fetch.strip()})")
    # ORPHANED-COMBO: holds Twinflame/Heat Shimmer but no Dualcaster (or vice-versa), no dig.
    seenset = set(info["combo_seen"])
    has_spark = bool(seenset & {"Twinflame", "Heat Shimmer"})
    has_dc = "Dualcaster Mage" in seenset
    if (has_spark != has_dc) and (has_spark or has_dc) and not dig_in_lib_reachable(info):
        tags.append("ORPHANED-COMBO")
    # ENGINE-RICH/PAYOFF-POOR
    engine_rich = (bodies >= 3) or (info["doublers"] >= 1) or (info["treasures"] >= 3)
    no_reach_payoff = not info["payoff_in_hand"] and not info["combo_seen"]
    if engine_rich and no_reach_payoff and not payoff_seen:
        tags.append("ENGINE-RICH/PAYOFF-POOR")

    # ── primary mode ──
    mode = None
    evidence = ""

    # MYSTERY: a reachable win existed but CHECK stayed "no line" / never fired.
    # castable payoff in hand AND an established loop/engine (bodies>=1 + mana engine) but no win.
    has_mana_engine = any(n in MANAENG or n in BURN for n in info["board_engines"])
    if info["payoff_in_hand"] and bodies >= 1 and has_mana_engine and not info["won"]:
        # Oracle is only a real win if library small enough; Grapeshot/Freeze need storm.
        if "Thassa's Oracle" in info["hand"] and lib <= dev:
            mode = "MYSTERY"
            evidence = (f"Oracle in hand, library {lib}<=devotion {dev}, bodies={bodies}, "
                        f"mana engine {info['board_engines']} — lethal but CHECK said no line")

    if mode is None:
        # MANA-SCREW
        if lands <= 2 and bodies <= 1:
            mode = "MANA-SCREW"
            evidence = f"only {lands} lands on board, bodies={bodies} at turn {MAXT}"
        # FLOODED
        elif lands >= 6 and bodies <= 1 and lib > 40:
            mode = "FLOODED"
            evidence = f"{lands} lands on board but bodies={bodies}, little action"
        # NO-DIG / PAYOFF-BURIED
        elif lib > 50 and (payoff_in_lib or info["combo_lib"]) and not info["any_dig"]:
            mode = "NO-DIG/PAYOFF-BURIED"
            seen_frac = 100 - int(100 * lib / max(lib + 0, 1))
            evidence = (f"library {lib} at turn {MAXT}, no dig cast, win-enablers still in lib: "
                        f"{info['payoffs_lib'] + info['combo_lib']}")
        # SELF-MILL / OVER-DIG
        elif gy > 40 and not info["payoff_in_hand"]:
            binned = sorted((PAYOFFS | COMBO | {"Gale, Waterdeep Prodigy", "Underworld Breach"})
                            & set(info["gy"]))
            mode = "SELF-MILL/OVER-DIG"
            evidence = f"graveyard {gy}, payoffs/combo binned into gy: {binned}, no castable payoff"
        # GAS-STARVED
        elif bodies >= 2 and fpc >= 2 and not payoff_seen and not info["dig_seen"]:
            mode = "GAS-STARVED"
            evidence = (f"engine up (bodies={bodies}, flips/cast={fpc}) but no payoff & no dig seen; "
                        f"CHECK '{_check_str(info)}'")
        # THRESHOLD
        elif info["last_check"] and not no_line:
            m = re.search(r"P\(win\)=([0-9.]+)", info["last_check"][0])
            p = float(m.group(1)) if m else 0.0
            if 0.80 <= p < 0.95:
                mode = "THRESHOLD"
                evidence = f"solver finds P={p:.3f} line late but 0.95 gate declines it"
    if mode is None:
        # fall through: engine assembled but no payoff route -> GAS-STARVED-ish; else generic.
        if bodies >= 2 and not payoff_seen:
            mode = "GAS-STARVED"
            evidence = (f"bodies={bodies} flips/cast={fpc}, no payoff/dig seen; "
                        f"CHECK '{_check_str(info)}'")
        elif payoff_in_lib and not info["any_dig"]:
            mode = "NO-DIG/PAYOFF-BURIED"
            evidence = (f"library {lib}, win-enablers still in lib {info['payoffs_lib']}, "
                        f"no dig cast")
        else:
            mode = "VARIANCE-OTHER"
            evidence = (f"lib={lib} gy={gy} bodies={bodies} lands={lands} "
                        f"payoff_seen={payoff_seen}")

    # ── VARIANCE vs FIXABLE ──
    fixable_tags = {"ORACLE-TRAP", "ORPHANED-COMBO"}
    is_fixable = (mode in ("MYSTERY", "THRESHOLD")
                  or any(t in fixable_tags for t in tags)
                  or any(t.startswith("TUTOR-MISFETCH") for t in tags))
    verdict = "FIXABLE" if is_fixable else "VARIANCE"
    return mode, tags, verdict, evidence


def dig_in_lib_reachable(info):
    """Could a dig spell still be reached? True if any dig is in library AND we cast dig before."""
    return bool(info["dig_lib"]) and info["any_dig"]


def _check_str(info):
    c = info["last_check"]
    return c[0].strip() if c else "no line"


# ── reporting ────────────────────────────────────────────────────────────────────
def winpct(turns_list, depth):
    return 100.0 * sum(1 for t in turns_list if t is not None) / depth


def diagnose_seed(seed, depth=DEPTH, max_turns=MAXT, verbose=True):
    luck = find_brick_luck(seed, depth, max_turns)
    if luck is None:
        if verbose:
            print(f"  seed {seed}: no bricking luck found in {depth*4} samples (not a brick)")
        return None
    info = inspect(seed, luck, max_turns)
    mode, tags, verdict, evidence = classify(info)
    if verbose:
        print("=" * 78)
        print(f"  SEED {seed}  (brick luck={luck})   primary={mode}   {verdict}")
        if tags:
            print(f"    tags: {', '.join(tags)}")
        print(f"    lib_left={info['lib_left']}  gy={info['gy_size']}  treasures={info['treasures']}"
              f"  opp_life={info['opp_life']}  bodies={info['krark_bodies']}"
              f"  doublers={info['doublers']}  flips/cast={info['flips_per_cast']}"
              f"  devotion={info['devotion']}  lands={info['lands_on_board']}")
        print(f"    last CHECK : {_check_str(info)}")
        print(f"    payoffs  SEEN={info['payoffs_seen']}  STILL-IN-LIB={info['payoffs_lib']}")
        print(f"    combo    SEEN={info['combo_seen']}  STILL-IN-LIB={info['combo_lib']}")
        print(f"    manaeng  SEEN={info['manaeng_seen']}  STILL-IN-LIB={info['manaeng_lib']}")
        print(f"    burn     SEEN={info['burn_seen']}  STILL-IN-LIB={info['burn_lib']}")
        print(f"    dig cast : {info['dig_cast'] or '(none)'}   any_dig={info['any_dig']}")
        print(f"    board engines : {info['board_engines'] or '(none)'}")
        print(f"    final hand    : {info['hand'] or '(empty)'}")
        print(f"    TUTOR fetches : {info['tutors'] or '(none)'}")
        print(f"    EVIDENCE: {evidence}")
    return {"seed": seed, "luck": luck, "mode": mode, "tags": tags,
            "verdict": verdict, "evidence": evidence, "info": info}


def run(base, count, depth=DEPTH, max_turns=MAXT):
    print("#" * 78)
    print(f"# BRICK DIAGNOSIS  seeds {base}..{base+count-1}  depth={depth}  max-turns={max_turns}")
    print("#" * 78)
    by_seed, elapsed = sweep(base, count, depth, max_turns)
    # rank seeds by win%
    scored = sorted(((winpct(t, depth), s) for s, t in by_seed.items()))
    bricks = [s for wp, s in scored if wp == 0.0]
    near = [s for wp, s in scored if 0.0 < wp <= 100.0 * 2 / depth]
    print(f"\nSWEEP DONE in {elapsed:.1f}s. "
          f"hard-bricks (0%): {len(bricks)}   near-bricks (<=2/{depth}): {len(near)}")
    print(f"hard-brick seeds: {bricks}")
    print(f"near-brick seeds: {near}\n")

    diagnoses = []
    for s in bricks + near:
        wp = dict((sd, wpv) for wpv, sd in scored)[s]
        print(f"\n[win {wp:.0f}% over {depth}]")
        d = diagnose_seed(s, depth, max_turns, verbose=True)
        if d:
            d["winpct"] = wp
            diagnoses.append(d)

    _summary(diagnoses, depth)
    return diagnoses


def _summary(diagnoses, depth):
    print("\n" + "#" * 78)
    print("# SUMMARY TABLE")
    print("#" * 78)
    print(f"{'seed':>6} | {'win%':>5} | {'primary mode':<22} | {'tags':<34} | "
          f"{'verdict':<8} | evidence")
    print("-" * 140)
    for d in diagnoses:
        print(f"{d['seed']:>6} | {d.get('winpct',0):>4.0f}% | {d['mode']:<22} | "
              f"{', '.join(d['tags'])[:34]:<34} | {d['verdict']:<8} | {d['evidence'][:60]}")

    by_mode = Counter(d["mode"] for d in diagnoses)
    by_verdict = Counter(d["verdict"] for d in diagnoses)
    print("\nTALLY BY MODE:")
    for m, c in by_mode.most_common():
        print(f"   {m:<24} {c}")
    print("TALLY BY VERDICT:")
    for v, c in by_verdict.most_common():
        print(f"   {v:<24} {c}")

    # fixable levers
    print("\nFIXABLE LEVERS (ranked by seeds rescued):")
    levers = {
        "gate Oracle tutor-value on dig/mill access (ORACLE-TRAP / Oracle TUTOR-MISFETCH)":
            [d["seed"] for d in diagnoses
             if "ORACLE-TRAP" in d["tags"]
             or any(t.startswith("TUTOR-MISFETCH") and "Oracle" in t for t in d["tags"])],
        "tutor wishlist should fetch a combo partner / dig over a dead body (ORPHANED-COMBO / misfetch)":
            [d["seed"] for d in diagnoses
             if "ORPHANED-COMBO" in d["tags"]
             or any(t.startswith("TUTOR-MISFETCH") for t in d["tags"])],
        "lower / make-adaptive the 0.95 go-off gate (THRESHOLD)":
            [d["seed"] for d in diagnoses if d["mode"] == "THRESHOLD"],
        "win-detection gap — solver missed a real line (MYSTERY)":
            [d["seed"] for d in diagnoses if d["mode"] == "MYSTERY"],
    }
    ranked = sorted(((len(v), k, v) for k, v in levers.items() if v), reverse=True)
    if not ranked:
        print("   (none — all diagnosed bricks are pure variance)")
    for n, k, v in ranked:
        print(f"   [{n}] {k}\n        -> seeds {sorted(set(v))}")

    mysteries = [d for d in diagnoses if d["mode"] == "MYSTERY"]
    if mysteries:
        print("\n" + "#" * 78)
        print("# MYSTERY (win-detection / solver gaps) — escalate")
        print("#" * 78)
        for d in mysteries:
            print(f"  seed {d['seed']} luck {d['luck']}: {d['evidence']}")
            print(f"     repro: python sim.py --seed {d['seed']} --flip-trials 1 --max-turns {MAXT}"
                  f"  (or set g.dev_rng=random.Random({d['seed']}*1_000_003+{d['luck']}))")


# ── cli ──────────────────────────────────────────────────────────────────────────
if __name__ == "__main__":
    args = sys.argv[1:]
    if args and args[0] == "--seed":
        seed = int(args[1])
        depth = int(args[2]) if len(args) > 2 else DEPTH
        diagnose_seed(seed, depth, MAXT, verbose=True)
    else:
        base = int(args[0]) if len(args) > 0 else 6000
        count = int(args[1]) if len(args) > 1 else 1000
        depth = int(args[2]) if len(args) > 2 else DEPTH
        mt = int(args[3]) if len(args) > 3 else MAXT
        run(base, count, depth, mt)
