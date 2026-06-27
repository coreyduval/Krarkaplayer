# MTG Rules Reference — Krark/Sakashima cEDH Simulator

This document exists so the simulator's AI layer (Claude) doesn't misread game
states or flag valid lines as bugs. Read this before touching `sim.rs`,
`planner.rs`, or any win-detection logic. (The engine is Rust; the `src/*.rs` files
below were ported from the original `*.py` modules.)

---

## 1. Commander Format Basics

| Rule | Detail |
|---|---|
| Deck size | 100 cards total, including the commander(s) |
| Singleton | Only 1 copy of each card except basic lands |
| Starting life | 40 (not 20) |
| Commander damage | 21 combat damage from a single commander = that player loses |
| Multiplayer default | Free-for-all, turn order clockwise |

### Command Zone
- Commanders start there before the game begins.
- If a commander would go to the library, hand, graveyard, or exile, its owner
  may return it to the command zone instead (a replacement effect, chosen each time).
- Commanders can be recast from the command zone at any time you could cast them.

### Command Tax
Each time a commander is cast from the **command zone**, it costs **+{2} generic**
for each previous time it was cast from there this game. Tax is per commander,
tracked separately.

### Color Identity
Every card in the deck must use only mana symbols present in the combined color
identity of the commanders. Krark is {R}, Sakashima is {U} → the deck is Izzet
(red + blue). No green, white, or black cards.

---

## 2. Partner Commanders

Both Krark, the Thumbless and Sakashima of a Thousand Faces have the **Partner**
keyword. Partner means:

- You may designate **two** commanders, both with Partner.
- Both begin the game in the command zone.
- The deck has 98 non-commander cards (100 − 2 commanders).
- Color identity is the **union** of both commanders' identities.
- Commander tax and commander damage are tracked **separately** for each.

---

## 3. The Legend Rule and Sakashima

The legend rule: if a player controls two or more legendary permanents with the
same name, they choose one and put the rest in the graveyard.

**Sakashima of a Thousand Faces** has the text:
> "The 'legend rule' doesn't apply to permanents you control."

While Sakashima is on the battlefield, you can control any number of legendary
permanents with the same name — including multiple "Krark, the Thumbless" bodies.
This is the foundational combo enabler.

**Critical caveat:** If Sakashima leaves the battlefield, the legend rule
immediately applies again. Never clone Krark directly (without Sakashima-safe
protection) because removing Sakashima would destroy all but one Krark.

---

## 4. Krark, the Thumbless — Core Mechanic

**Oracle text:**
> Whenever you cast an instant or sorcery spell, flip a coin.
> If you lose the flip, return that spell to its owner's hand.
> If you win the flip, copy that spell. You may choose new targets for the copy.

### What this means in practice

| Outcome | Effect on original | Effect otherwise |
|---|---|---|
| Win the flip | **Copied** (copy resolves first) | Original resolves normally |
| Lose the flip | **Returned to hand** | Nothing else happens |

The original spell is **NOT countered** when returned — it simply goes back to
hand. This matters because "can't be countered" doesn't protect against it.

### Multiple Krark bodies (the key mechanic)

Each Krark body generates **one independent trigger** per instant/sorcery cast.
With N Krark bodies you flip N coins, one per trigger. All triggers are
independent. The original spell:

- **Resolves** only if **every** flip is won (all N flips = win).
- **Returns to hand** if **any** flip is lost.
- Meanwhile, each **won** flip creates a copy of the spell that resolves
  regardless of what other flips do.

So with 2 Krark bodies, casting one Brainstorm can produce:

| Flips (W/L) | Copies resolving | Original |
|---|---|---|
| Win + Win | 2 copies | Original resolves → goes to graveyard |
| Win + Lose | 1 copy | **Returns to hand** → can be cast again |
| Lose + Win | 1 copy | **Returns to hand** → can be cast again |
| Lose + Lose | 0 copies | **Returns to hand** → can be cast again |

**This is why the same spell can be cast many times in one turn.** With 2+ Krark
bodies, there is a high probability the original returns to hand while copies
(with their draw/mana triggers) resolve. This is not a bug — it is the engine.

### Flips per cast formula

```
flips_per_cast = krark_bodies × (1 + trigger_doublers)
```

Where `trigger_doublers` is the count of Veyran, Voice of Duality and Harmonic
Prodigy on the battlefield (each adds +1 to the multiplier additively).

Example: 2 Krark bodies + Veyran + Harmonic = 2 × (1 + 2) = **6 flips** per cast.

### Krark's Thumb

> "If you would flip a coin, flip two coins and choose one of them instead."

With the Thumb, each individual flip becomes "flip 2, keep the better one",
raising the win probability per flip from 0.50 to **0.75**.

---

## 5. Veyran, Voice of Duality and Harmonic Prodigy

Both are **trigger doublers** — they make certain triggered abilities fire an
additional time.

### Veyran, Voice of Duality
> "If casting or copying an instant or sorcery spell causes a triggered ability
> of a permanent you control to trigger, that ability triggers an additional time."

Applies to: Krark's flip triggers, Archmage Emeritus draws, Storm-Kiln Artist
treasures, Birgi mana, Urabrask mana/damage — anything triggered by casting or
copying an I/S.

### Harmonic Prodigy
> "If an ability of a **Shaman or Wizard** you control triggers, that ability
> triggers an additional time."

Applies only to triggers from permanents that are Shamans or Wizards:
- Krark (Wizard) ✓ — flip triggers doubled
- Archmage Emeritus (Wizard) ✓ — draw triggers doubled
- Storm-Kiln Artist (Shaman) ✓ — treasure triggers doubled
- Birgi (God) ✗ — NOT doubled by Harmonic
- Urabrask (Praetor) ✗ — NOT doubled by Harmonic

Both doublers stack **additively**. Two doublers = +2 triggers per event, not ×2.

---

## 6. Copies Are Not Casts

This invariant is load-bearing throughout the engine:

- Krark copies of a spell are **not cast** — they are put directly onto the stack.
- Storm copies are **not cast**.
- Therefore copies do **not** increment the storm count.
- Copies do **not** retrigger Krark.
- Copies **do** trigger magecraft (Archmage Emeritus, Storm-Kiln Artist, Veyran)
  because magecraft fires on cast **or copy**.

---

## 7. Storm

Grapeshot, Brain Freeze, and Flusterstorm have Storm:
> "When you cast this spell, copy it for each spell cast before it this turn."

Storm copies ≠ Krark copies. They are separate, parallel copy sources.
A storm spell cast with storm count N and K Krark flip wins creates:
- N storm copies (deterministic)
- K Krark copies (probabilistic)
- 1 original (resolves iff all flips won)

---

## 8. Win Conditions

### Thassa's Oracle (Thoracle)
ETB: look at top X cards where X = blue devotion. **Win if X ≥ library size.**
The deck builds blue devotion through U-costed permanents. Gating: this is only
lethal when devotion exceeds library count at the moment the ETB resolves.

### Grapeshot
Deals 1 damage to any target. Storm copies scale the damage. With infinite storm
(via Twinflame+Dualcaster loop or Krark runaway), all opponents die.

### Brain Freeze
Mills 3 per instance. Storm scales the mill. Target self to feed Thoracle, or
opponents to deck them.

### Twinflame + Dualcaster Mage (deterministic KILL)
1. Cast Twinflame targeting any creature.
2. Flash in Dualcaster Mage in response (as the Twinflame is on the stack).
3. Dualcaster ETB copies Twinflame.
4. The Twinflame copy targets Dualcaster, creating a hasty Dualcaster token.
5. The new token's ETB copies Twinflame again → infinite hasty Dualcasters.
6. Attack for lethal.

This combo needs {1}{R}{R} for Twinflame + {1}{R}{R} for Dualcaster (flash).
No flips required — works with zero Krark bodies.

---

## 9. What the Simulator Models

- **Solitaire only** — opponents are abstracted to life totals, library sizes,
  and hand sizes. No counterspells from opponents.
- **Uninterrupted resolution** — every spell resolves; no opponent interaction.
- **Krark flips** are the only randomness; the planner branches on them.
- **Defense Grid** adds +{3} to the pilot's own spells during opponents' turns.
- **Library** = the actual shuffled decklist; drawing pulls real cards.
- **Copies are not cast** — Krark copies never increment storm or retrigger Krark.
- **Legend rule (partial model)** — clone bodies (Glasspool / Phantasmal) stack Krark freely,
  assuming the Sakashima break is present. The Twinflame / Heat Shimmer token line is stricter:
  it checks `has_sakashima_break()` (a real Sakashima on the battlefield) before copying a
  *legendary* creature; without it, the duplicate would die to the legend rule, so it copies a
  non-legendary engine (Tavern Scoundrel / Storm-Kiln / Archmage) instead.
- **An infinite loop is not a win without a payoff** — the engine requires an
  accessible terminal win condition (Thoracle, Grapeshot, Brain Freeze, combat).

---

## 10. Why the Same Spell Can Be Cast Many Times

This is the most important thing to internalize. With 2+ Krark bodies on board:

1. Cast Brainstorm.
2. Each Krark body triggers. Say there are 2 triggers (2 bodies, no doublers).
3. Win 1 flip → 1 copy of Brainstorm resolves (draws 3, fires Archmage, etc.).
4. Lose the other flip → original Brainstorm returns to hand.
5. Mana engines (Birgi, Storm-Kiln treasures) replenish {R}/{*}.
6. Repeat: cast Brainstorm again from hand.

This is not a bug. This is the deck. The engine in `resolver.rs` models it with
`p_return = 1 − p^F` (probability original returns to hand) and the DFS in
`planner.rs` explores paths where this recursion continues until a payoff is
reachable. A "long line" with 11+ casts of the same spell is a legitimate
runaway chain where the original kept coming back.
