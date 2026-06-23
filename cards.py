"""
cards.py — Static card data, loaded authoritatively from krarkashima.txt.

NO STUBS. Every card resolves to a full CardDef:
  * name / mana_cost / mana_value / rules_text  -> parsed verbatim from the file
  * card type + creature subtypes               -> curated overlay (TYPES / SUBTYPES)
  * engine behaviour (flip body, doublers,       -> curated overlay (ENGINE)
    mana/draw/treasure/damage triggers)

Why overlays instead of parsing rules_text for behaviour:
  The file gives exact cost/text (authoritative, verified by the pilot), but does
  NOT carry card type or creature subtype consistently, and oracle text is not a
  safe thing to regex into a rules engine. Types/subtypes/engine flags are the
  hand-verified layer. Subtypes matter because Harmonic Prodigy only doubles
  Shaman/Wizard triggers — getting one wrong silently corrupts mana/draw EV.

VERIFY list (subtype/interaction not independently confirmed; engine refuses to
certify a kill that leans on these until checked) is exposed as cards.VERIFY.
"""
from __future__ import annotations

import os
import re
from dataclasses import dataclass, field
from enum import Enum
from typing import Dict, FrozenSet, Optional


# --------------------------------------------------------------------------- #
# Types / tags
# --------------------------------------------------------------------------- #

class CardType(str, Enum):
    INSTANT = "instant"
    SORCERY = "sorcery"
    CREATURE = "creature"
    ARTIFACT = "artifact"
    ENCHANTMENT = "enchantment"
    LAND = "land"
    PLANESWALKER = "planeswalker"


ManaCost = Dict[str, int]  # {"U":1,"R":1,"generic":2}; colorless uses key "C"


@dataclass(frozen=True)
class CardDef:
    name: str
    types: FrozenSet[CardType]
    cost: ManaCost = field(default_factory=dict)
    mana_value: int = 0
    rules_text: str = ""
    subtypes: FrozenSet[str] = frozenset()

    # ---- engine behaviour ----
    is_krark_body: bool = False        # Krark itself (a permanent is also a body if it copies Krark)
    is_trigger_doubler: bool = False   # Veyran / Harmonic Prodigy
    clones_sakashima_safe: bool = False

    # value/mana triggers. trigger_cause tells the multiplier logic what gates apply.
    #   "is_cast_or_copy" -> fires on casting OR copying an I/S (magecraft); Veyran-doublable
    #   "spell_cast"      -> fires on casting any spell (Birgi); Veyran-doublable only if the cast is I/S
    #   "is_cast"         -> fires on casting an I/S (Urabrask); Veyran-doublable
    #   "coin_flip_win"   -> fires per won coin flip (Tavern); NOT Veyran-doublable
    draw_per_trigger: int = 0
    treasure_per_trigger: int = 0
    mana_per_trigger: ManaCost = field(default_factory=dict)
    damage_per_trigger: int = 0
    treasure_per_flip_win: int = 0
    trigger_cause: Optional[str] = None
    fires_on_copy: bool = False        # does the value trigger also fire on Krark copies?

    @property
    def blue_pips(self) -> int:
        return self.cost.get("U", 0)

    @property
    def is_instant_or_sorcery(self) -> bool:
        return bool(self.types & {CardType.INSTANT, CardType.SORCERY})

    @property
    def is_permanent(self) -> bool:
        return bool(self.types & {CardType.CREATURE, CardType.ARTIFACT,
                                  CardType.ENCHANTMENT, CardType.LAND, CardType.PLANESWALKER})

    @property
    def is_shaman_or_wizard(self) -> bool:
        return bool(self.subtypes & {"Shaman", "Wizard"})

    @property
    def has_value_trigger(self) -> bool:
        return bool(self.draw_per_trigger or self.treasure_per_trigger
                    or self.mana_per_trigger or self.damage_per_trigger
                    or self.treasure_per_flip_win)


# --------------------------------------------------------------------------- #
# Mana-cost parser
# --------------------------------------------------------------------------- #

def parse_mana_cost(s: str) -> ManaCost:
    """'{2}{U}{R}' -> {'generic':2,'U':1,'R':1}. Handles {0}, {R/P} (phyrexian,
    stored as the colored half), X (stored under 'X'). Lands ('Land'/'Basic Land')
    return {}."""
    s = s.strip()
    if s in ("Land", "Basic Land", ""):
        return {}
    cost: ManaCost = {}
    for tok in re.findall(r"\{([^}]+)\}", s):
        if tok.isdigit():
            cost["generic"] = cost.get("generic", 0) + int(tok)
        elif tok in ("W", "U", "B", "R", "G", "C"):
            cost[tok] = cost.get(tok, 0) + 1
        elif tok == "X":
            cost["X"] = cost.get("X", 0) + 1
        elif "/" in tok:  # hybrid / phyrexian, e.g. R/P, 2/U -> keep the colored half
            half = next((c for c in tok.split("/") if c in "WUBRG"), None)
            if half:
                cost[half] = cost.get(half, 0) + 1
            else:
                cost["generic"] = cost.get("generic", 0) + 1
    return cost


# --------------------------------------------------------------------------- #
# Overlays (the hand-verified layer)
# --------------------------------------------------------------------------- #

_CREATURES = {
    "Krark, the Thumbless", "Sakashima of a Thousand Faces", "Archmage Emeritus",
    "Baral, Chief of Compliance", "Birgi, God of Storytelling", "Dualcaster Mage",
    "Gale, Waterdeep Prodigy", "Harmonic Prodigy", "Imperial Recruiter",
    "Okaun, Eye of Chaos", "Ragavan, Nimble Pilferer", "Snapcaster Mage",
    "Spellseeker", "Storm-Kiln Artist", "Tavern Scoundrel", "Urabrask",
    "Veyran, Voice of Duality", "Vivi Ornitier", "Zndrsplt, Eye of Wisdom",
    "Glasspool Mimic", "Phantasmal Image", "Subtlety", "Thassa's Oracle",
    "Valley Floodcaller",
}
_SORCERIES = {
    "Quasiduplicate", "Gamble", "Jeska's Will", "Ponder", "Grapeshot",
    "Twinflame", "Strike It Rich",
    "Gitaxian Probe", "Rite of Flame", "Heat Shimmer",   # added 2026-06-21 (Heat Shimmer is a sorcery)
    "Serum Visions", "Preordain",                        # added 2026-06-21 (dig cantrips, cut4 swap)
}
_INSTANTS = {
    "Brainstorm", "Cyclonic Rift", "Brain Freeze", "Frantic Search", "Snap",
    "Gut Shot", "Desperate Ritual", "Pyretic Ritual", "Deflecting Swat",
    "Fierce Guardianship", "Flusterstorm", "Force of Will", "Pact of Negation",
    "An Offer You Can't Refuse", "Borne Upon a Wind",     # added 2026-06-21
    "Mogg Salvage", "Peek",
    "Opt", "Consider",                                    # added 2026-06-21 (dig cantrips, cut4 swap)
}
_ENCHANTMENTS = {"Underworld Breach", "Mystic Remora", "Rhystic Study"}
_ARTIFACTS = {
    "Defense Grid", "Arcane Signet", "Chrome Mox", "Krark's Thumb",
    "Lion's Eye Diamond", "Lotus Petal", "Mox Diamond", "Sol Ring",
    "Springleaf Drum", "The One Ring",
    "Mana Vault", "Mox Amber", "Relic of Legends",         # added 2026-06-21
}

# Creature subtypes — only those that matter for Harmonic's Shaman/Wizard gate
# (verified by search where it changes EV; others left empty = conservative).
SUBTYPES: Dict[str, FrozenSet[str]] = {
    "Krark, the Thumbless": frozenset({"Goblin", "Wizard"}),         # Wizard -> Harmonic applies
    "Archmage Emeritus": frozenset({"Human", "Wizard"}),             # verified Human Wizard
    "Storm-Kiln Artist": frozenset({"Dwarf", "Shaman"}),             # verified Dwarf Shaman
    "Veyran, Voice of Duality": frozenset({"Human", "Wizard"}),
    "Harmonic Prodigy": frozenset({"Human", "Shaman"}),
    "Birgi, God of Storytelling": frozenset({"God"}),                # NOT Shaman/Wizard
    "Urabrask": frozenset({"Phyrexian", "Praetor"}),                 # NOT Shaman/Wizard
    "Vivi Ornitier": frozenset({"Wizard"}),                          # Wizard -> Harmonic doubles its trigger
    # Tavern Scoundrel subtype NOT independently confirmed -> see VERIFY.
}

# Engine behaviour overlay.
ENGINE: Dict[str, dict] = {
    "Krark, the Thumbless": dict(is_krark_body=True),
    "Sakashima of a Thousand Faces": dict(clones_sakashima_safe=True),
    "Veyran, Voice of Duality": dict(is_trigger_doubler=True),
    "Harmonic Prodigy": dict(is_trigger_doubler=True),
    "Archmage Emeritus": dict(draw_per_trigger=1, trigger_cause="is_cast_or_copy", fires_on_copy=True),
    "Storm-Kiln Artist": dict(treasure_per_trigger=1, trigger_cause="is_cast_or_copy", fires_on_copy=True),
    "Birgi, God of Storytelling": dict(mana_per_trigger={"R": 1}, trigger_cause="spell_cast"),
    "Urabrask": dict(mana_per_trigger={"R": 1}, damage_per_trigger=1, trigger_cause="is_cast"),
    "Tavern Scoundrel": dict(treasure_per_flip_win=2, trigger_cause="coin_flip_win"),
    # Vivi Ornitier: "whenever you cast a noncreature spell ... it deals 1 damage to EACH opponent"
    # -> 3 to the single 160-life pool (3 opponents). PLUS it grows +1/+1 per cast and taps for
    # X=power mana once/turn; amortized that's ~1 mana per cast (cast N -> tap for ~N once), and the
    # doublers that fire its trigger twice also grow its power twice as fast — so modeling the mana
    # as mana_per_trigger=1 (Veyran/Harmonic-doubled) matches the {0}:add X in expectation. Both
    # burn and mana are is_cast (Veyran-doublable) and Vivi is a Wizard (Harmonic-doublable).
    "Vivi Ornitier": dict(damage_per_trigger=3, mana_per_trigger={"*": 1}, trigger_cause="is_cast"),
    # Draw a card on every won coin flip — a dig engine with Krark's flips (not a
    # Wizard/Shaman, so Harmonic doesn't double it; coin-flip triggers aren't Veyran-doublable).
    "Zndrsplt, Eye of Wisdom": dict(draw_per_trigger=1, trigger_cause="coin_flip_win"),
}

# Interactions not independently confirmed; engine must not certify a kill on these.
VERIFY = {
    "Tavern Scoundrel": "creature subtype (Harmonic gate) + Krark's-Thumb-doubles-physical-coins ruling",
}


# --------------------------------------------------------------------------- #
# Registry + loader
# --------------------------------------------------------------------------- #

REGISTRY: Dict[str, CardDef] = {}
_DEFAULT_FILE = os.path.join(os.path.dirname(__file__), "krarkashima.txt")


def _classify(name: str, raw_cost: str, rules: str) -> FrozenSet[CardType]:
    if raw_cost in ("Land", "Basic Land"):
        return frozenset({CardType.LAND})
    if name in _CREATURES:
        return frozenset({CardType.CREATURE})
    if name in _SORCERIES:
        return frozenset({CardType.SORCERY})
    if name in _INSTANTS:
        return frozenset({CardType.INSTANT})
    if name in _ENCHANTMENTS:
        return frozenset({CardType.ENCHANTMENT})
    if name in _ARTIFACTS:
        return frozenset({CardType.ARTIFACT})
    # last resort: read a leading "... Creature/Artifact/... —" type line
    m = re.match(r"^(?:Legendary |Basic |Universes Beyond: [^—]*— )?"
                 r"(Creature|Artifact|Enchantment|Land|Sorcery|Instant|Planeswalker)", rules)
    if m:
        return frozenset({CardType(m.group(1).lower())})
    raise ValueError(f"Cannot classify card type for {name!r}; add it to an overlay set.")


def load(path: str = _DEFAULT_FILE) -> None:
    REGISTRY.clear()
    with open(path, "r", encoding="utf-8") as fh:
        header = fh.readline()  # name|mana_cost|mana_value|rules_text
        assert header.strip().startswith("name|"), "unexpected header"
        for line in fh:
            line = line.rstrip("\n")
            if not line.strip():
                continue
            name, raw_cost, mv, rules = line.split("|", 3)
            name = name.strip()
            types = _classify(name, raw_cost.strip(), rules)
            base = dict(
                name=name,
                types=types,
                cost=parse_mana_cost(raw_cost),
                mana_value=int(mv) if mv.strip().isdigit() else 0,
                rules_text=rules.strip(),
                subtypes=SUBTYPES.get(name, frozenset()),
            )
            base.update(ENGINE.get(name, {}))
            REGISTRY[name] = CardDef(**base)


# --------------------------------------------------------------------------- #
# Targeting legality in the solitaire model
# --------------------------------------------------------------------------- #
# Opponents are abstracted to life / library / hand totals — there is no opponent
# stack and no opponent permanents, and spells resolve one at a time (uninterrupted).
# So a spell whose only legal target is an opponent's spell-on-the-stack or an
# opponent's permanent has NO legal target and cannot be cast for value. (A free
# spell WITH real targets that returns to hand is a fine infinite engine — the gate
# is about targets, not cost.)

# Counters/redirects/removal whose ONLY legal target is an opponent's spell or
# permanent — no opponent stack or board means no target, so they can't be cast.
# (Cyclonic Rift returns a permanent you DON'T control; Swan Song/Flusterstorm/etc.
# could in principle target your own spell, but they cost mana every loop, so they
# aren't free engines and aren't worth crediting as solitaire value here.)
_NO_SOLITAIRE_TARGET = frozenset({
    "Force of Will",
    "Subtlety", "Cyclonic Rift",
    "Pact of Negation",                       # combo-only; loses the game if not paid
    "An Offer You Can't Refuse",              # counter an opp's noncreature spell — no target solo
    "Mogg Salvage",                           # destroy an opp's artifact — no target solo
    # HARD COUNTERS DON'T LOOP: a counter CONSUMES its target on resolution, and Krark's copies
    # are created one-at-a-time (can't be stockpiled to feed each other), so even targeting your
    # own spell self-terminates after ~1 cast. Fierce Guardianship is free w/ a commander but it
    # still counters (destroys) — so it's a dead solitaire engine, NOT a repeatable value loop.
    "Fierce Guardianship",
    # NOTE: Flusterstorm is NOT here — it's a STORM spell. Storm makes all its copies at once
    # (they coexist and can target one another, unlike Krark's sequential copies), and each copy
    # is a "copy of an I/S" -> a magecraft trigger when CREATED, regardless of the (soft) counter.
    # So with a value engine it's real storm/Treasure fuel; gated in loops._MAGECRAFT_FUEL.
})
# FREE, NON-DESTRUCTIVE redirect: Deflecting Swat (free w/ commander) only CHANGES targets — it
# doesn't destroy anything — so it can be recast repeatedly to fire Krark + magecraft as long as
# there's a spell/ability on the stack to point at. That makes it real storm/magecraft fuel (a
# WIN only when it feeds an accessible payoff). Fierce Guardianship is NOT here — it counters
# (destroys), so it can't loop (see _NO_SOLITAIRE_TARGET above).
FREE_COUNTERS = frozenset({"Deflecting Swat"})
# Need a creature YOU control as a target.
_NEEDS_OWN_CREATURE = frozenset({"Twinflame", "Heat Shimmer", "Quasiduplicate", "Snap"})


def castable_in_solitaire(name: str, has_own_creature: bool) -> bool:
    """Whether instant/sorcery `name` has a legal target in the solitaire model.
    Counters/redirects/opponent-permanent removal whose only target is the opponent
    have none; FREE counters can target your own spell (see FREE_COUNTERS); 'target
    creature you control' spells need a creature on your side."""
    if name in _NO_SOLITAIRE_TARGET:
        return False
    if name in _NEEDS_OWN_CREATURE:
        return has_own_creature
    return True


def get(name: str) -> CardDef:
    if not REGISTRY:
        load()
    try:
        return REGISTRY[name]
    except KeyError:
        raise KeyError(f"Unknown card {name!r}. Is it in krarkashima.txt?")


def all_cards():
    if not REGISTRY:
        load()
    return list(REGISTRY.values())


if __name__ == "__main__":
    load()
    print(f"loaded {len(REGISTRY)} cards, 0 stubs")
    by_type: Dict[str, int] = {}
    for c in REGISTRY.values():
        for t in c.types:
            by_type[t.value] = by_type.get(t.value, 0) + 1
    print("by type:", dict(sorted(by_type.items())))
    sw = [c.name for c in REGISTRY.values() if c.is_shaman_or_wizard]
    print("Shaman/Wizard (Harmonic doubles):", sw)
    print("VERIFY:", list(VERIFY))
