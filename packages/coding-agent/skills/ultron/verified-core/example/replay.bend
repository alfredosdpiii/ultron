import Base

# Key equality, written out so its reflexivity can be proved.
def eqn(a: Nat, b: Nat) -> Bool:
  match a b:
    case 0n 0n:
      True{}
    case 0n 1n+bp:
      False{}
    case 1n+ap 0n:
      False{}
    case 1n+ap 1n+bp:
      eqn(ap, bp)

type Put is Data:
  Put{key: Nat, val: Nat}

def pick(found: Bool, v: Nat, rest: Maybe<&2, Nat>) -> Maybe<&2, Nat>:
  match found:
    case True{}:
      Some{v}
    case False{}:
      rest

# The value of key k after replaying a journal given newest entry first.
def get(es: List<&2, Put>, +k: Nat) -> Maybe<&2, Nat>:
  match es:
    case Nil{}:
      None{}
    case Con{Put{k2, v}, rest}:
      pick(eqn(k, k2), v, get(rest, k))
