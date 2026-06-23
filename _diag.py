import time, cards
from game_state import GameState, Permanent, krark_body, ManaPool
from planner import solve

cards.load()
s = GameState(
    library=["Island"]*80,
    hand=["Pact of Negation", "Quasiduplicate", "Jeska's Will"],
    battlefield=[krark_body("Krark, the Thumbless"),
                 Permanent("Baral, Chief of Compliance", summoning_sick=False),
                 Permanent("Archmage Emeritus", summoning_sick=False)],
)
s.mana.add("R",1); s.mana.add("U",2)
print("calling solve()...", flush=True)
t=time.time()
line = solve(s)
print(f"done in {time.time()-t:.1f}s -> {line}", flush=True)
