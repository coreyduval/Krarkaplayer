import cards; cards.load()
from game_state import GameState, Permanent, krark_body

bf = [krark_body("Krark, the Thumbless")]
s = GameState(battlefield=bf)
print("cast_cost WITH commander out:")
for n in ["Pact of Negation", "Fierce Guardianship", "Deflecting Swat"]:
    print(f"  {n:20} cost={s.cast_cost(n)}  castable={cards.castable_in_solitaire(n, False)}")

s2 = GameState(battlefield=[Permanent("Island")])
print("cast_cost NO commander (Fierce/Deflecting keep printed cost):")
for n in ["Fierce Guardianship", "Deflecting Swat"]:
    print(f"  {n:20} cost={s2.cast_cost(n)}")

still_excluded = [n for n in ["Force of Will", "Flusterstorm", "Cyclonic Rift",
                              "Swan Song", "Pyroblast", "Subtlety"]
                  if not cards.castable_in_solitaire(n, True)]
print("still no-target (excluded):", still_excluded)

# Free-counter value loop -> Thoracle: Krark + Archmage(draw) + Storm-Kiln + Pact in
# hand, Thoracle in hand, smallish library. Looping free Pact draws to Thoracle.
from planner import solve
base = [krark_body("Krark, the Thumbless"),
        krark_body("Sakashima of a Thousand Faces", copy_of="Krark, the Thumbless"),
        Permanent("Veyran, Voice of Duality", summoning_sick=False),
        Permanent("Archmage Emeritus", summoning_sick=False),
        Permanent("Storm-Kiln Artist", summoning_sick=False)]
g = GameState(library=["Island"] * 14, hand=["Pact of Negation", "Thassa's Oracle"],
              battlefield=base, opponent_life=(40, 40, 40))
g.mana.add("U", 2)
print("\nfree Pact loop -> Thoracle:", solve(g))
