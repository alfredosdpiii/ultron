# builds: dependency graph, incremental rebuilds, package versions

Package `depgraph` (standard library only). Everything below is importable from `depgraph`.

## Graph

- `Graph()` holds named build targets. `add(name, deps=(), inputs=())` defines target `name`, which depends on the
  targets named in `deps` and reads the source inputs named in `inputs`; it returns the graph.
  - Names, dependencies and inputs are non-empty strings without surrounding spaces, else `ValueError`. Defining
    a name twice raises `ValueError`.
  - A dependency may name a target defined later. Listing a dependency or an input more than once is the same as
    listing it once. A target may depend on itself (a cycle).
- `targets()`: every target name, sorted. `name in graph`, `len(graph)`.
- `deps(name)`, `inputs(name)`, `dependents(name)` (defined targets that depend on `name` directly): sorted tuples
  without repeats. `readers(item)`: the targets that read input `item`, sorted. Unknown names raise `KeyError`.
- `edges()`: every `(target, dependency)` pair, targets in name order. `undefined()`: sorted pairs whose
  dependency names no defined target.

## Order and cycles

- `closure(graph, targets=None)`: the set of the given targets (default: all) and everything they depend on,
  transitively. A requested target that is not defined raises `KeyError`; a dependency on an undefined target
  (among those reached) raises `ValueError`.
- `topo_order(graph, targets=None)`: a list of the closure's targets in build order. Every target comes after
  all of its dependencies. Whenever several targets are ready (all their dependencies placed), the one with the
  smallest name comes next. If the closure contains a cycle it raises `CycleError`, whose `cycle` attribute is
  `find_cycle(graph, targets)`.
- `find_cycle(graph, targets=None)`: `None` if the closure has no cycle, else its canonical cycle. A cycle is a
  list of distinct targets `[c0, c1, ..., ck]` where each depends on the next and `ck` depends on `c0`, written
  starting at its smallest name. The canonical cycle is the smallest of all the closure's cycles in Python list
  order (so `["a", "b"]` < `["a", "b", "c"]` < `["a", "c"]`). A self-dependency is the cycle `[name]`.

Example: `a` depends on `b` and `c`, `b` on `a`, `c` on `a`: `find_cycle` is `["a", "b"]`; with only
`c -> d -> c` and `b -> b` it is `["b"]`.

## Incremental rebuilds

`rebuild(graph, changed_inputs, build, previous)` returns a `Rebuild` with fields `rebuilt` and `changed` (tuples
of target names in build order) and `outputs` (a dict holding every target's output after the build, keys in
build order).

- `previous` maps targets to their outputs from the last build (entries for unknown targets are ignored).
  `build(name, dep_outputs)` builds one target and returns its output; `dep_outputs` maps each of the target's
  dependencies to its output as of this build (rebuilt this time, or else from `previous`).
- Targets are visited in `topo_order(graph)`. A target is rebuilt (by exactly one call to `build`) when
  - it is directly dirty: it reads an input in `changed_inputs`, or it has no entry in `previous`; or
  - the output of at least one of its dependencies changed in this build.
- A rebuilt target's output changed when it differs from its `previous` output (always, if it had none).
  Early cutoff: a rebuilt target whose output is unchanged does not make its dependents rebuild.
- Targets that are not rebuilt keep their `previous` output. `build` is called in build order, once per
  rebuilt target, and never for any other target.
- `directly_dirty(graph, changed_inputs, previous)`: the directly dirty targets in build order.
  `affected(graph, changed_inputs, previous)`: the directly dirty targets and all their transitive dependents,
  sorted (every target a rebuild could touch).

## Versions and constraints

- `Version.parse(text)`: `"MAJOR.MINOR.PATCH"`, three non-negative integers without leading zeros, else
  `ValueError`. Versions compare numerically, component by component; `str(version)` gives the text back.
- `Constraint(text)`: one or more tokens separated by whitespace; a version matches when it matches every token.
  `matches(version)` takes a `Version` or a version string. Malformed text raises `ValueError`. `P` below is a
  partial version (`X`, `X.Y` or `X.Y.Z`); missing components count as 0 for comparisons.

| Token | Matches |
| --- | --- |
| `*` | every version |
| `X.Y.Z` or `=X.Y.Z` | exactly that version (a partial version here is malformed) |
| `>=P`, `>P`, `<=P`, `<P` | the comparison with P padded with zeros |
| `~X` | `>=X.0.0 <(X+1).0.0` |
| `~X.Y`, `~X.Y.Z` | `>=X.Y.0` (or `>=X.Y.Z`) and `<X.(Y+1).0` |
| `^P` | `>=P` and below the next change to the leftmost non-zero component given: `^1.2.3` is `<2.0.0`, `^0.2.3` is `<0.3.0`, `^0.0.3` is `<0.0.4`. If every given component is zero, the given components may not change: `^0` is `<1.0.0`, `^0.0` is `<0.1.0`, `^0.0.0` is `<0.0.1`. |

- No spaces inside a token (`>= 1.0` is malformed). `best(versions)`: the highest of the version strings that
  matches, as a string, or `None`.

## Resolution

`resolve(registry, requirements)` chooses versions of external packages.

- `registry`: `{package: {version text: {dependency package: constraint text}}}`: every release of every package
  and what it requires. `requirements`: `{package: constraint text}`.
- A choice gives each package either one of its releases or nothing. It is valid when every required package is
  chosen and matches its requirement; every dependency of every chosen release is chosen and matches that
  release's constraint; and every chosen package is needed: reachable from the requirements through the
  dependencies of chosen releases. Packages named anywhere but missing from `registry` have no releases.
- Among valid choices, the result is the best one: compare choices package by package in name order (over every
  package named in the registry or the requirements); a higher version beats a lower one, and any version beats
  nothing.
- Returns `{package: version text}` for the chosen packages, sorted by name. No valid choice raises
  `ResolutionError`; a malformed version or constraint raises `ValueError`.
