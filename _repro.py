import cards
from game_state import GameState, Permanent, krark_body
from planner import solve
from loops import estimate_p_lethal, analyze_runaway

cards.load()

def board(*p): return list(p)

base = board(
    krark_body("Krark, the Thumbless"),
    krark_body("Sakashima of a Thousand Faces", copy_of="Krark, the Thumbless"),
    Permanent("Veyran, Voice of Duality", summoning_sick=False),
    Permanent("Archmage Emeritus", summoning_sick=False),
    Permanent("Baral, Chief of Compliance", summoning_sick=False),
)

# Realistic library: payoffs are BURIED in the library, NOT in hand.
lib = ["Island"] * 30 + ["Thassa's Oracle"] + ["Island"] * 8 + ["Grapeshot"] + ["Island"] * 5
s = GameState(library=lib, hand=["Jeska's Will"], battlefield=base,
              opponent_life=(40, 40, 40))
s.mana.add("R", 1); s.mana.add("C", 2)

print("flips/cast:", s.flips_per_cast, " blue_devotion:", s.blue_devotion)
ra = analyze_runaway(s, "Jeska's Will")
print(ra.summary())

print("\n-- estimate_p_lethal (payoff only in LIBRARY) --")
est = estimate_p_lethal(s, "Jeska's Will", n_sims=3000, seed=1)
print(f"p_win={est['p_win']:.3f}  by_payoff={est['by_payoff']}  "
      f"deckout_no_win={est['p_deckout_no_win']:.3f}  mean_chain={est['mean_chain_len']:.1f}")

print("\n-- full solve() --")
line = solve(s)
print(line)
