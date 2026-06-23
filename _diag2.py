import time, cards
from game_state import GameState, Permanent, krark_body
from planner import solve

cards.load()
lands = ["Island","Island","Island","Mountain","Mountain","Mountain","Mountain","Sol Ring"]
bf = [krark_body("Krark, the Thumbless"),
      krark_body("Sakashima of a Thousand Faces", copy_of="Krark, the Thumbless"),
      Permanent("Baral, Chief of Compliance", summoning_sick=False),
      Permanent("Archmage Emeritus", summoning_sick=False),
      Permanent("Veyran, Voice of Duality", summoning_sick=False)]
for L in lands:
    bf.append(Permanent(L, summoning_sick=False, tapped=False))
s = GameState(library=["Island"]*70,
              hand=["Pact of Negation","Quasiduplicate","Jeska's Will","Brainstorm"],
              battlefield=bf)
print(f"untapped mana sources on board, calling solve()...", flush=True)
t=time.time()
line = solve(s)
print(f"done in {time.time()-t:.1f}s -> {line}", flush=True)

from planner import DeterministicKillSearch, ProbabilisticPlanner
print("--- with tightened budget ---", flush=True)
t=time.time()
line = solve(s, deterministic=DeterministicKillSearch(max_depth=6, node_budget=1500))
print(f"done in {time.time()-t:.2f}s -> {line}", flush=True)

print("--- tightened DFS + fewer MC sims ---", flush=True)
t=time.time()
line = solve(s, deterministic=DeterministicKillSearch(max_depth=6, node_budget=1500),
             probabilistic=ProbabilisticPlanner(mc_sims=300))
print(f"done in {time.time()-t:.2f}s -> {line}", flush=True)
