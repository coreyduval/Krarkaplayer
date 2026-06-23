import random, cards
from game_state import GameState, Permanent, krark_body
from resolver import resolve_cast_sample
from loops import _winning_payoff

cards.load()
base = [
    krark_body("Krark, the Thumbless"),
    krark_body("Sakashima of a Thousand Faces", copy_of="Krark, the Thumbless"),
    Permanent("Veyran, Voice of Duality", summoning_sick=False),
    Permanent("Archmage Emeritus", summoning_sick=False),
    Permanent("Baral, Chief of Compliance", summoning_sick=False),
]
lib = ["Island"] * 30 + ["Thassa's Oracle"] + ["Island"] * 8 + ["Grapeshot"] + ["Island"] * 5
s = GameState(library=lib, hand=["Jeska's Will"], battlefield=base, opponent_life=(40, 40, 40))
s.mana.add("R", 1); s.mana.add("C", 2)

rng = random.Random(1)
iters = 0
while "Jeska's Will" in s.hand and iters < 80:
    cost = s.cast_cost("Jeska's Will")
    if not s.mana.can_pay(cost):
        print(f"mana ruin at iter {iters}, pool={s.mana.pool}")
        break
    s.mana.pay(cost)
    s, _ = resolve_cast_sample(s, "Jeska's Will", rng)
    iters += 1
    if len(s.library) == 0:
        break

print(f"iters={iters}  storm_count={s.storm_count}  library={len(s.library)}  devotion={s.blue_devotion}")
print(f"pool={s.mana.pool}")
tho = "Thassa's Oracle"
print(f"Thoracle in hand={tho in s.hand}  Grapeshot in hand={'Grapeshot' in s.hand}")
print(f"Grapeshot in gy={'Grapeshot' in s.graveyard}")
print(f"need_life=120; storm+1={s.storm_count+1}")
print(f"_winning_payoff -> {_winning_payoff(s, ('Grapeshot', tho), 120)}")
print(f"can pay UU? {s.mana.can_pay({'U':2})}   can pay R+1? {s.mana.can_pay({'R':1,'generic':1})}")
