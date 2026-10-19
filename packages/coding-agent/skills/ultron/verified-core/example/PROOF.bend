import Base
import ./replay.bend as R
import ./LAWS.bend as L

def L.eqn_refl(k):
  match k:
    case 0n:
      {==}
    case 1n+p:
      L.eqn_refl(p)

def L.last_write_wins(es, k, v):
  %L.eqn_refl(k) : {R.pick(_, v, R.get(es, k)) == Some{v} : Maybe<&2, Nat>}
  {==}
