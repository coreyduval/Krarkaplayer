"""
dryrun.py — paste a board state in the spec's notation, get a recommendation.

Usage:
    from dryrun import run
    run('''
    MANA: R R U
    HAND: Twinflame, Dualcaster Mage
    BOARD: Krark, Mountain, Mountain, Mountain, Island, Island
    LIBRARY: 70
    STORM: 0
    TURN: my T3, main 1
    DEFENSE GRID: no
    ''')

Or run this file for the bundled examples. The parser accepts the §7 notation:
  MANA / HAND / BOARD / GY / STACK / STORM / LIBRARY / TURN / DEFENSE GRID.
Use short card names in BOARD/HAND (e.g. "Krark", "Sakashima(as Krark)") — they
resolve to full names. Library is filled with neutral fillers to the given count.
"""
from __future__ import annotations

import re
from typing import List

import cards
from cards import CardType
from game_state import GameState, Permanent, ManaPool
from planner import solve, Line

FILLER = "Island"   # neutral filler for unknown library cards (keeps size correct)

# short-name aliases for the linchpins; full names also resolve directly
ALIASES = {
    "krark": "Krark, the Thumbless",
    "sakashima": "Sakashima of a Thousand Faces",
    "saka": "Sakashima of a Thousand Faces",
    "thumb": "Krark's Thumb",
    "veyran": "Veyran, Voice of Duality",
    "harmonic": "Harmonic Prodigy",
    "birgi": "Birgi, God of Storytelling",
    "archmage": "Archmage Emeritus",
    "storm-kiln": "Storm-Kiln Artist",
    "storm kiln": "Storm-Kiln Artist",
    "ska": "Storm-Kiln Artist",
    "dualcaster": "Dualcaster Mage",
    "jeska": "Jeska's Will",
    "thoracle": "Thassa's Oracle",
    "thassa": "Thassa's Oracle",
    "led": "Lion's Eye Diamond",
    "breach": "Underworld Breach",
    "urabrask": "Urabrask",
    "baral": "Baral, Chief of Compliance",
    "tavern": "Tavern Scoundrel",
    "ragavan": "Ragavan, Nimble Pilferer",
    "gale": "Gale, Waterdeep Prodigy",
}


def resolve_name(token: str) -> str:
    cards.get  # ensure module loaded
    if not cards.REGISTRY:
        cards.load()
    t = token.strip()
    tl = t.lower()
    if t in cards.REGISTRY:
        return t
    if tl in ALIASES:
        return ALIASES[tl]
    starts = [n for n in cards.REGISTRY if n.lower().startswith(tl)]
    if len(starts) == 1:
        return starts[0]
    subs = [n for n in cards.REGISTRY if tl in n.lower()]
    if len(subs) == 1:
        return subs[0]
    if starts:
        return min(starts, key=len)
    if subs:
        return min(subs, key=len)
    raise ValueError(f"can't resolve card name {token!r}")


def _split_csv(val: str) -> List[str]:
    return [x.strip() for x in val.split(",") if x.strip()]


def _parse_perm(token: str) -> Permanent:
    m = re.match(r"^(.*?)\(\s*as\s+(.*?)\s*\)$", token.strip(), re.IGNORECASE)
    if m:
        return Permanent(resolve_name(m.group(1)), copy_of=resolve_name(m.group(2)),
                         summoning_sick=False)
    return Permanent(resolve_name(token), summoning_sick=False)


def parse_board(text: str) -> GameState:
    s = GameState(library=[], hand=[], battlefield=[], graveyard=[], mana=ManaPool())
    for raw in text.strip().splitlines():
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        key, _, val = line.partition(":")
        key = key.strip().upper()
        val = val.strip()
        if key == "MANA":
            for tok in val.split():
                if tok.isdigit():
                    s.mana.add("C", int(tok))
                elif tok in ("W", "U", "B", "R", "G", "C"):
                    s.mana.add(tok, 1)
                elif tok == "*":
                    s.mana.add("*", 1)
        elif key == "HAND":
            s.hand = [resolve_name(x) for x in _split_csv(val)]
        elif key == "BOARD":
            s.battlefield = [_parse_perm(x) for x in _split_csv(val)]
        elif key in ("GY", "GRAVEYARD"):
            s.graveyard = [resolve_name(x) for x in _split_csv(val)]
        elif key == "LIBRARY":
            n = int(re.search(r"\d+", val).group()) if re.search(r"\d+", val) else 0
            s.library = [FILLER] * n
        elif key == "STORM":
            s.storm_count = int(re.search(r"\d+", val).group()) if re.search(r"\d+", val) else 0
        elif key in ("DEFENSE GRID", "DEFENSEGRID", "DGRID"):
            if val.lower().startswith("y"):
                s.battlefield.append(Permanent("Defense Grid", summoning_sick=False))
        elif key == "TURN":
            s.is_my_turn = not val.lower().lstrip().startswith(("opp", "their", "enemy"))
        elif key == "STACK":
            pass  # not modeled in the solitaire planner yet
    return s


def describe(s: GameState) -> str:
    return (f"  bodies={s.krark_bodies} doublers={s.trigger_doublers} "
            f"flips/cast={s.flips_per_cast} p={s.flip_p:.2f} | "
            f"blue devotion={s.blue_devotion} | storm={s.storm_count} "
            f"library={len(s.library)} | mana={dict(s.mana.pool)} "
            f"{'(my turn)' if s.is_my_turn else '(opp turn)'}")


def run(text: str) -> Line:
    s = parse_board(text)
    print("READ:")
    print(describe(s))
    line = solve(s)
    print("RESULT:")
    print(f"  {line}")
    if line.actions:
        print("  line:")
        for a in line.actions:
            print(f"    - {a}")
    return line


if __name__ == "__main__":
    cards.load()

    print("=" * 72)
    print("Example 1 — Twinflame + Dualcaster in hand, mana sitting in LANDS only")
    print("(the deterministic search must TAP lands to assemble the loop)")
    print("=" * 72)
    run("""
    MANA: 
    HAND: Twinflame, Dualcaster Mage
    BOARD: Krark, Mountain, Mountain, Mountain, Island, Island
    LIBRARY: 70
    STORM: 0
    TURN: my T3, main 1
    DEFENSE GRID: no
    """)

    print("\n" + "=" * 72)
    print("Example 2 — 'a few Krarks' + engines + Jeska's Will (probabilistic)")
    print("=" * 72)
    run("""
    MANA: R C C
    HAND: Jeska's Will, Thassa's Oracle, Grapeshot
    BOARD: Krark, Sakashima(as Krark), Veyran, Harmonic, Archmage, Storm-Kiln
    LIBRARY: 40
    STORM: 0
    TURN: my T4, main 1
    DEFENSE GRID: no
    """)

    print("\n" + "=" * 72)
    print("Example 3 — Defense Grid up on an opponent's turn taxes our own spells")
    print("(same loop pieces, but +3 generic each makes the floated mana fall short)")
    print("=" * 72)
    run("""
    MANA: R R R
    HAND: Twinflame, Dualcaster Mage
    BOARD: Krark, Mountain, Mountain
    LIBRARY: 70
    STORM: 0
    TURN: opp T3
    DEFENSE GRID: yes
    """)
