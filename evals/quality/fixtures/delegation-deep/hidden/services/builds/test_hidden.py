"""Hidden checks for services/builds (depgraph): SPEC cases plus randomized comparison with reference models."""
import itertools
import random
import unittest
import zlib

from depgraph import (
    Constraint,
    CycleError,
    Graph,
    ResolutionError,
    Version,
    affected,
    find_cycle,
    rebuild,
    resolve,
    topo_order,
)

POOL = ["b1", "b2", "core", "gen", "io", "k", "lib", "m0", "net", "pkg", "tools", "x"]


def graph_of(spec, reads=None, rnd=None):
    graph = Graph()
    names = list(spec)
    if rnd is not None:
        rnd.shuffle(names)
    for name in names:
        graph.add(name, spec[name], (reads or {}).get(name, ()))
    return graph


def ref_order(spec, targets=None):
    """Reference: smallest ready target first, computed from scratch at every step."""
    if targets is None:
        keep = set(spec)
    else:
        keep, stack = set(), list(targets)
        while stack:
            name = stack.pop()
            if name not in keep:
                keep.add(name)
                stack.extend(spec[name])
    out = []
    while len(out) < len(keep):
        ready = sorted(n for n in keep if n not in out and set(spec[n]) <= set(out))
        if not ready:
            return None
        out.append(ready[0])
    return out


def ref_cycle(spec):
    best = None
    names = sorted(spec)
    for size in range(1, len(names) + 1):
        for combo in itertools.permutations(names, size):
            if combo[0] == min(combo) and all(combo[(i + 1) % size] in spec[combo[i]] for i in range(size)):
                if best is None or list(combo) < best:
                    best = list(combo)
    return best


def random_dag(rnd, size, repeats):
    names = rnd.sample(POOL, size)
    spec = {}
    for index, name in enumerate(names):
        earlier = names[:index]
        count = rnd.randint(0, min(4, len(earlier)))
        spec[name] = rnd.choices(earlier, k=count) if repeats else rnd.sample(earlier, count)
    return spec


def ref_rebuild(spec, reads, changed_inputs, build, previous):
    """Reference: memoized recursion over dependencies, then ordered by the reference build order."""
    memo = {}

    def outcome(name):
        if name not in memo:
            dep_state = {dep: outcome(dep) for dep in spec[name]}
            dirty = name not in previous or bool(set(reads.get(name, ())) & set(changed_inputs))
            if dirty or any(changed for _, changed, _ in dep_state.values()):
                output = build(name, {dep: state[0] for dep, state in dep_state.items()})
                memo[name] = (output, output != previous.get(name), True)
            else:
                memo[name] = (previous[name], False, False)
        return memo[name]

    order = ref_order(spec)
    for name in order:
        outcome(name)
    rebuilt = [n for n in order if memo[n][2]]
    changed = [n for n in order if memo[n][1]]
    return rebuilt, changed, {n: memo[n][0] for n in order}


def ref_matches(text, version):
    v = tuple(int(p) for p in version.split("."))
    for token in text.split():
        if token == "*":
            continue
        op = next((o for o in (">=", "<=", ">", "<", "=", "^", "~") if token.startswith(o)), "=")
        nums = [int(p) for p in token.lstrip("<>=^~").split(".")]
        low = tuple((nums + [0, 0, 0])[:3])
        if op == "=":
            ok = v == low
        elif op == ">=":
            ok = v >= low
        elif op == ">":
            ok = v > low
        elif op == "<=":
            ok = v <= low
        elif op == "<":
            ok = v < low
        elif op == "~":
            high = (low[0] + 1, 0, 0) if len(nums) == 1 else (low[0], low[1] + 1, 0)
            ok = low <= v < high
        else:
            nonzero = [i for i, n in enumerate(nums) if n]
            keep = nonzero[0] + 1 if nonzero else len(nums)
            ok = low <= v and v[:keep] == low[:keep]
        if not ok:
            return False
    return True


def ref_resolve(registry, roots):
    names = sorted(set(registry) | set(roots) | {d for rel in registry.values() for deps in rel.values() for d in deps})

    def key(choice):
        return [tuple(int(p) for p in choice[n].split(".")) if n in choice else (-1,) for n in names]

    best = None
    options = [[None] + list(registry.get(n, {})) for n in names]
    for combo in itertools.product(*options):
        choice = {n: v for n, v in zip(names, combo) if v is not None}
        if not all(n in choice and ref_matches(t, choice[n]) for n, t in roots.items()):
            continue
        if not all(
            d in choice and ref_matches(t, choice[d]) for n, v in choice.items() for d, t in registry[n][v].items()
        ):
            continue
        seen, stack = set(), list(roots)
        while stack:
            n = stack.pop()
            if n not in seen:
                seen.add(n)
                stack.extend(registry[n][choice[n]])
        if seen != set(choice):
            continue
        if best is None or key(choice) > key(best):
            best = choice
    return None if best is None else dict(sorted(best.items()))


def random_constraint(rnd):
    part = lambda: ".".join(str(rnd.choice([0, 0, 1, 2])) for _ in range(rnd.choice([1, 2, 3, 3])))
    full = lambda: ".".join(str(rnd.choice([0, 1, 2])) for _ in range(3))
    kind = rnd.choice("^^~crex*")
    if kind in "^~":
        return kind + part()
    if kind == "c":
        return rnd.choice([">=", ">", "<=", "<"]) + part()
    if kind == "r":
        return f">={part()} <{part()}"
    if kind in "ex":
        return rnd.choice(["", "="]) + full()
    return "*"


class GraphAndOrder(unittest.TestCase):
    def test_spec_examples(self):
        g = Graph().add("a", ["b", "c"]).add("b", ["a"]).add("c", ["a"])
        self.assertEqual(find_cycle(g), ["a", "b"])
        with self.assertRaises(CycleError) as raised:
            topo_order(g)
        self.assertEqual(raised.exception.cycle, ["a", "b"])
        g = Graph().add("c", ["d"]).add("d", ["c"]).add("b", ["b"]).add("a")
        self.assertEqual(find_cycle(g), ["b"])
        self.assertEqual(topo_order(g, ["a"]), ["a"])
        self.assertIsNone(find_cycle(g, ["a"]))

    def test_repeated_dependencies_count_once(self):
        g = Graph().add("app", ["lib", "lib", "zz"]).add("lib").add("zz", ["lib"])
        self.assertEqual(g.deps("app"), ("lib", "zz"))
        self.assertEqual(g.dependents("lib"), ("app", "zz"))
        self.assertEqual(topo_order(g), ["lib", "zz", "app"])

    def test_errors(self):
        g = Graph().add("a", ["missing"])
        with self.assertRaises(ValueError):
            topo_order(g)
        with self.assertRaises(KeyError):
            topo_order(g, ["nope"])
        with self.assertRaises(ValueError):
            g.add("a")
        with self.assertRaises(ValueError):
            Graph().add("")

    def test_random_orders(self):
        for seed in range(7000, 7400):
            rnd = random.Random(seed)
            spec = random_dag(rnd, rnd.randint(2, 11), repeats=True)
            targets = rnd.sample(list(spec), rnd.randint(1, min(3, len(spec)))) if rnd.random() < 0.3 else None
            self.assertEqual(topo_order(graph_of(spec, rnd=rnd), targets), ref_order(spec, targets), (seed, spec, targets))

    def test_random_cycles(self):
        for seed in range(7000, 7250):
            rnd = random.Random(seed)
            names = rnd.sample(POOL, rnd.randint(1, 6))
            spec = {n: [d for d in names if rnd.random() < 0.3] for n in names}
            self.assertEqual(find_cycle(graph_of(spec, rnd=rnd)), ref_cycle(spec), (seed, spec))


class Rebuilds(unittest.TestCase):
    def run_case(self, spec, reads, changed_inputs, previous, salt):
        def build(name, dep_outputs):
            calls.append((name, dict(dep_outputs)))
            text = repr((salt, name, sorted(dep_outputs.items()), [(i, i in changed_inputs) for i in reads.get(name, ())]))
            return zlib.crc32(text.encode()) % 3

        calls = []
        want_rebuilt, want_changed, want_outputs = ref_rebuild(spec, reads, changed_inputs, build, previous)
        want_calls, calls = calls, []
        got = rebuild(graph_of(spec, reads), changed_inputs, build, dict(previous))
        self.assertEqual(list(got.rebuilt), want_rebuilt)
        self.assertEqual(list(got.changed), want_changed)
        self.assertEqual(got.outputs, want_outputs)
        self.assertEqual(calls, sorted(want_calls, key=lambda call: want_rebuilt.index(call[0])))
        self.assertTrue(set(got.rebuilt) <= set(affected(graph_of(spec, reads), changed_inputs, previous)))

    def test_two_paths_to_a_target(self):
        # "top" is dirtied through "a" early and through "z" (via "m") later: it must be built once, after "z".
        spec = {"a": [], "m": [], "z": ["m"], "top": ["a", "z"]}
        reads = {"a": ["in1"], "m": ["in2"]}
        outputs = []

        def build(name, dep_outputs):
            outputs.append((name, dict(dep_outputs)))
            return {"a": "A2", "m": "M2", "z": "Z2", "top": "T2"}[name]

        previous = {"a": "A1", "m": "M1", "z": "Z1", "top": "T1"}
        got = rebuild(graph_of(spec, reads), ["in1", "in2"], build, previous)
        self.assertEqual(got.rebuilt, ("a", "m", "z", "top"))
        self.assertEqual(outputs[-1], ("top", {"a": "A2", "z": "Z2"}))

    def test_early_cutoff(self):
        spec = {"a": [], "b": ["a"], "c": ["b"]}
        got = rebuild(graph_of(spec, {"a": ["src"]}), ["src"], lambda n, d: "same", {"a": "same", "b": "x", "c": "y"})
        self.assertEqual((got.rebuilt, got.changed), (("a",), ()))
        self.assertEqual(got.outputs, {"a": "same", "b": "x", "c": "y"})

    def test_random_rebuilds(self):
        for seed in range(9000, 9300):
            rnd = random.Random(seed)
            spec = random_dag(rnd, rnd.randint(2, 10), repeats=False)
            reads = {n: rnd.sample(["p", "q", "r", "s", "t"], rnd.randint(0, 2)) for n in spec}
            previous = {n: rnd.randint(0, 2) for n in spec if rnd.random() < 0.93}
            with self.subTest(seed=seed):
                self.run_case(spec, reads, rnd.sample(["p", "q", "r", "s", "t"], rnd.randint(1, 3)), previous, seed)


class VersionsAndResolve(unittest.TestCase):
    def test_spec_table(self):
        cases = {
            "^1.2.3": ("1.2.3", "1.9.0", "!2.0.0", "!1.2.2"),
            "^0.2.3": ("0.2.9", "!0.3.0", "!0.2.2"),
            "^0.0.3": ("0.0.3", "!0.0.4"),
            "^0": ("0.9.9", "!1.0.0"),
            "^0.0": ("0.0.7", "!0.1.0"),
            "^0.0.0": ("0.0.0", "!0.0.1"),
            "~1": ("1.9.9", "!2.0.0"),
            "~1.2": ("1.2.0", "1.2.9", "!1.3.0"),
            ">=1.0 <2.0": ("1.0.0", "1.99.0", "!2.0.0", "!0.9.9"),
            "=1.4.2": ("1.4.2", "!1.4.3"),
            "*": ("0.0.0", "9.9.9"),
        }
        for text, versions in cases.items():
            for version in versions:
                want = not version.startswith("!")
                self.assertEqual(Constraint(text).matches(version.lstrip("!")), want, (text, version))
        self.assertEqual(Constraint("^1.0").best(["0.9.0", "1.4.0", "1.10.0", "2.0.0"]), "1.10.0")
        for bad in ["", "1.2", ">= 1.0", "^", "1.02.3", "v1.0.0", "~1.2.3.4"]:
            with self.assertRaises(ValueError, msg=bad):
                Constraint(bad)
        self.assertLess(Version.parse("1.9.0"), Version.parse("1.10.0"))

    def test_random_constraints(self):
        grid = [f"{a}.{b}.{c}" for a in range(3) for b in range(3) for c in range(3)]
        for seed in range(4000, 4600):
            rnd = random.Random(seed)
            text = random_constraint(rnd)
            if rnd.random() < 0.4:
                text += " " + random_constraint(rnd)
            for version in grid:
                self.assertEqual(Constraint(text).matches(version), ref_matches(text, version), (text, version))

    def test_backtracking_forgets_abandoned_constraints(self):
        registry = {
            "a": {"2.0.0": {"c": "<2.0.0", "b": "^9.0.0"}, "1.0.0": {"c": ">=1.0.0"}},
            "b": {"1.0.0": {}},
            "c": {"2.0.0": {}, "1.0.0": {}},
        }
        self.assertEqual(resolve(registry, {"a": "*", "c": "*"}), {"a": "1.0.0", "c": "2.0.0"})
        with self.assertRaises(ResolutionError):
            resolve({"a": {"1.0.0": {}}}, {"a": "^2"})

    def test_random_resolutions(self):
        names = ["alpha", "beta", "gamma", "delta", "eps"]
        for seed in range(2000, 2160):
            rnd = random.Random(seed)
            packages = rnd.sample(names, rnd.randint(2, 4))
            registry = {}
            for p in packages:
                registry[p] = {}
                for v in sorted({f"{rnd.randint(0, 2)}.{rnd.randint(0, 1)}.{rnd.randint(0, 2)}" for _ in range(rnd.randint(1, 3))}):
                    others = [o for o in packages if o != p]
                    registry[p][v] = {d: random_constraint(rnd) for d in rnd.sample(others, rnd.randint(0, min(2, len(others))))}
            roots = {p: random_constraint(rnd) for p in rnd.sample(packages, rnd.randint(1, 2))}
            try:
                got = resolve(registry, roots)
            except ResolutionError:
                got = None
            self.assertEqual(got, ref_resolve(registry, roots), (seed, registry, roots))


if __name__ == "__main__":
    unittest.main()
