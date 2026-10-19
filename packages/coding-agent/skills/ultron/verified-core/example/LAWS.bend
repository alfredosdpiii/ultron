import Base
import ./replay.bend as R

law eqn_refl:
  for +k: Nat
  {True{} == R.eqn(k, k) : Bool}

# Last write wins: after writing v to k, reading k gives v, whatever came before.
law last_write_wins:
  for es: List<&2, R.Put>
  for +k: Nat
  for v: Nat
  {R.get(Con{R.Put{k, v}, es}, k) == Some{v} : Maybe<&2, Nat>}
