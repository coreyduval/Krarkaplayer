"""Emit a machine-comparable JSON of all deterministic Python card data:
classification (types), parsed cost, mana_value, subtypes, and the ENGINE overlay
fields. This is the golden reference the Rust registry must match exactly.

Run from repo root:  python rust/verify/py_card_dump.py > rust/verify/py_cards.json
"""
import json, sys, os
sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", ".."))
import cards

cards.load()

def dump_card(c):
    return {
        "name": c.name,
        "types": sorted(t.value for t in c.types),
        "cost": {k: v for k, v in sorted(c.cost.items())},
        "mana_value": c.mana_value,
        "subtypes": sorted(c.subtypes),
        "is_krark_body": c.is_krark_body,
        "is_trigger_doubler": c.is_trigger_doubler,
        "clones_sakashima_safe": c.clones_sakashima_safe,
        "draw_per_trigger": c.draw_per_trigger,
        "treasure_per_trigger": c.treasure_per_trigger,
        "mana_per_trigger": {k: v for k, v in sorted(c.mana_per_trigger.items())},
        "damage_per_trigger": c.damage_per_trigger,
        "treasure_per_flip_win": c.treasure_per_flip_win,
        "trigger_cause": c.trigger_cause,
        "fires_on_copy": c.fires_on_copy,
        "blue_pips": c.blue_pips,
        "is_instant_or_sorcery": c.is_instant_or_sorcery,
        "is_permanent": c.is_permanent,
        "is_shaman_or_wizard": c.is_shaman_or_wizard,
        "has_value_trigger": c.has_value_trigger,
    }

out = {name: dump_card(cards.REGISTRY[name]) for name in sorted(cards.REGISTRY)}
# also targeting-legality flags
targeting = {}
for name in sorted(cards.REGISTRY):
    targeting[name] = {
        "no_target": name in cards._NO_SOLITAIRE_TARGET,
        "free_counter": name in cards.FREE_COUNTERS,
        "needs_own_creature": name in cards._NEEDS_OWN_CREATURE,
    }
print(json.dumps({"cards": out, "targeting": targeting, "n": len(out)}, indent=1))
