"""
seed.py — Random opening-hand generator for the Krark/Sakashima deck.

Shuffles the 98-card non-commander deck (Krark and Sakashima are always
in the command zone) and deals an opening hand, producing a board-state
string ready for dryrun.run().

CLI:
    python seed.py                     # random 7-card hand
    python seed.py --hand 6            # 6-card hand (post-mulligan)
    python seed.py --seed 42           # reproducible shuffle
    python seed.py --count 5           # print 5 different hands
    python seed.py --seed 7 --count 3  # 3 hands from seeds 7, 8, 9

API:
    from seed import generate, build_deck
    state_str = generate(rng_seed=42)
    from dryrun import run
    run(state_str)   # always returns P(win)=0 at T1 — no board yet, useful
                     # after you edit BOARD: to match your in-game position
"""
from __future__ import annotations

import argparse
import os
import random
from typing import Optional

import cards as _cards

COMMANDERS = frozenset({"Krark, the Thumbless", "Sakashima of a Thousand Faces"})
DECK_SIZE = 98  # 100-card singleton + 2 partner commanders in the command zone

# The registry holds exactly 1 Island and 1 Mountain; the deck runs 16 of each by
# default. Filler restores the correct basic land counts (15 more of each = 30 cards).
# The Island/Mountain SPLIT is overridable for mana-base tuning via env vars
# KRARK_ISLANDS / KRARK_MOUNTAINS (totals, incl. the 1 in the registry). They must
# still sum to the same basic-land total (32) so the filler stays 30 and the deck 98 —
# changing the TOTAL land count is a separate experiment (edit DECK_SIZE / the deck).
def _basic_filler() -> list[str]:
    # 20 basics after the 2026-06-21 upgrade (12 cards added, 6 Island + 6 Mountain cut to
    # keep the deck at 98). Default split 8 Island / 12 Mountain: a paired 50-seed x 8-flip
    # sweep found the upgraded list wants RED basics (its new dual/Confluence/Mox Amber/Relic
    # cover blue), with 7-9 Islands tied ~92% and even/blue-heavy splits ~4 pts worse. Override
    # for tuning via KRARK_ISLANDS / KRARK_MOUNTAINS (must sum to 20).
    isl = int(os.environ.get("KRARK_ISLANDS", "8"))
    mtn = int(os.environ.get("KRARK_MOUNTAINS", "12"))
    return ["Island"] * (isl - 1) + ["Mountain"] * (mtn - 1)   # registry already has 1 of each


_FILLER: list[str] = _basic_filler()


def build_deck() -> list[str]:
    """Return the 98-card non-commander decklist with correct basic land counts."""
    if not _cards.REGISTRY:
        _cards.load()
    named = [c.name for c in _cards.all_cards() if c.name not in COMMANDERS]
    n_filler = DECK_SIZE - len(named)
    if n_filler < 0:
        raise RuntimeError(
            f"Registry has {len(named)} non-commander cards (> {DECK_SIZE}); "
            "check that COMMANDERS contains the right names."
        )
    if n_filler != len(_FILLER):
        raise RuntimeError(
            f"Filler list has {len(_FILLER)} cards but {n_filler} slots remain; "
            "update _FILLER to match the actual basic land counts."
        )
    return named + _FILLER


_DECK_CACHE: list[str] = []


def generate(
    hand_size: int = 7,
    turn: int = 1,
    rng_seed: Optional[int] = None,
) -> str:
    """Shuffle the deck, deal `hand_size` cards, return a dryrun board-state string.

    The BOARD field is intentionally left empty — commanders are in the command
    zone and haven't been cast yet.  Edit BOARD: before calling run() to reflect
    what you've played by the target turn.
    """
    global _DECK_CACHE
    if not _DECK_CACHE:
        _DECK_CACHE = build_deck()

    deck = list(_DECK_CACHE)
    random.Random(rng_seed).shuffle(deck)

    hand = deck[:hand_size]
    library_size = DECK_SIZE - hand_size

    hand_str = ", ".join(hand) if hand else ""
    seed_comment = f"# seed={rng_seed}" if rng_seed is not None else "# seed=random"
    return (
        f"{seed_comment}  hand={hand_size}  T{turn}\n"
        f"MANA:\n"
        f"HAND: {hand_str}\n"
        f"BOARD:\n"
        f"LIBRARY: {library_size}\n"
        f"STORM: 0\n"
        f"TURN: my T{turn}, main 1\n"
        f"DEFENSE GRID: no\n"
    )


def _summarise(state_str: str) -> str:
    """One-line summary of key cards in the hand for quick scanning."""
    _cards.load() if not _cards.REGISTRY else None
    combo_tags = {
        "Krark's Thumb", "Twinflame", "Dualcaster Mage", "Jeska's Will",
        "Thassa's Oracle", "Grapeshot", "Brain Freeze", "Underworld Breach",
        "Lion's Eye Diamond", "Birgi, God of Storytelling",
    }
    ramp_types = {"artifact", "land"}
    hand_line = next(
        (l for l in state_str.splitlines() if l.startswith("HAND:")), ""
    )
    hand_cards = [c.strip() for c in hand_line.removeprefix("HAND:").split(",") if c.strip()]

    combo_hits = [c for c in hand_cards if c in combo_tags]
    lands = [c for c in hand_cards if _cards.REGISTRY.get(c) and
             any(t.value in ramp_types for t in _cards.REGISTRY[c].types)]
    return (
        f"  combo={len(combo_hits)} ({', '.join(combo_hits) or 'none'})  "
        f"mana-sources={len(lands)} ({', '.join(lands) or 'none'})"
    )


if __name__ == "__main__":
    parser = argparse.ArgumentParser(
        description="Generate random Krark/Sakashima opening hands."
    )
    parser.add_argument("--hand", type=int, default=7, metavar="N",
                        help="Hand size (default 7; use 6/5 for mulligans)")
    parser.add_argument("--turn", type=int, default=1, metavar="T",
                        help="TURN field value (default 1)")
    parser.add_argument("--seed", type=int, default=None, metavar="SEED",
                        help="RNG seed (default: system random)")
    parser.add_argument("--count", type=int, default=1, metavar="N",
                        help="Number of hands to generate (default 1)")
    args = parser.parse_args()

    _cards.load()

    for i in range(args.count):
        seed = (args.seed + i) if args.seed is not None else None
        state = generate(hand_size=args.hand, turn=args.turn, rng_seed=seed)
        print(state)
        print(_summarise(state))
        if i < args.count - 1:
            print("-" * 60)
