"""Hidden checks for services/delta (layered config): SPEC cases plus randomized comparison with a reference."""
import copy
import random
import re
import unittest

from confmerge import coerce, deep_merge, env_overrides, load


def ref_merge(base, override):
    result = copy.deepcopy(base)
    for key, value in override.items():
        if value is None:
            result.pop(key, None)
        elif isinstance(value, dict):
            current = result.get(key)
            result[key] = ref_merge(current if isinstance(current, dict) else {}, value)
        else:
            result[key] = copy.deepcopy(value)
    return result


def ref_coerce(text):
    s = text.strip()
    low = s.lower()
    if low in ("true", "false"):
        return low == "true"
    if low == "null":
        return None
    if re.fullmatch(r"-?\d+", s):
        return int(s)
    if re.fullmatch(r"-?\d+\.\d+", s):
        return float(s)
    return text


def ref_env(environ, prefix="APP__"):
    tree = {}
    for name in sorted(environ):
        if not name.startswith(prefix):
            continue
        path = [part.lower() for part in name[len(prefix):].split("__")]
        if not all(path):
            continue
        node = tree
        for part in path[:-1]:
            if not isinstance(node.get(part), dict):
                node[part] = {}
            node = node[part]
        node[path[-1]] = ref_coerce(environ[name])
    return tree


KEYS = ["db", "host", "port", "pool", "size", "max_connections", "debug", "tags", "log", "level"]
VALUES = ["true", "FALSE", " null ", "Null", "42", "-7", " 8 ", "0.5", "-2.25", "1.", ".5", "1e3", "hello", " spaced ", "-", "--3", "007"]


def random_tree(rnd, depth=0):
    tree = {}
    for _ in range(rnd.randint(0, 4)):
        key = rnd.choice(KEYS)
        roll = rnd.random()
        if roll < 0.3 and depth < 3:
            tree[key] = random_tree(rnd, depth + 1)
        elif roll < 0.4:
            tree[key] = None
        elif roll < 0.5:
            tree[key] = [rnd.randint(0, 3) for _ in range(rnd.randint(0, 3))]
        else:
            tree[key] = rnd.choice([1, "x", True, 2.5, "", 0])
    return tree


def strip_nones(tree):
    return {k: strip_nones(v) if isinstance(v, dict) else v for k, v in tree.items() if v is not None}


def random_env(rnd):
    env = {}
    for _ in range(rnd.randint(0, 6)):
        path = [rnd.choice(KEYS).upper() for _ in range(rnd.randint(1, 3))]
        name = rnd.choice(["APP__", "APP__", "APP__", "APP_", "OTHER__"]) + "__".join(path)
        if rnd.random() < 0.05:
            name += "____X"
        env[name] = rnd.choice(VALUES)
    return env


class DeltaHidden(unittest.TestCase):
    def test_coerce(self):
        for text in VALUES + ["TRUE", "  -12 ", "3.14", "-0.0", "abc 1", "12a", ""]:
            got, want = coerce(text), ref_coerce(text)
            self.assertEqual((type(got), got), (type(want), want), repr(text))

    def test_env_paths(self):
        env = {"APP__DB__MAX_CONNECTIONS": "20", "APP__LOG_LEVEL": "debug", "APP__DB____PORT": "1", "APP__": "x", "APPX": "y"}
        self.assertEqual(env_overrides(env), {"db": {"max_connections": 20}, "log_level": "debug"})
        self.assertEqual(env_overrides({"APP__A": "1", "APP__A__B": "2"}), {"a": {"b": 2}})

    def test_null_deletes(self):
        defaults = {"db": {"host": "h", "port": 1}, "debug": True}
        self.assertEqual(load(defaults, None, {"APP__DB__HOST": "null", "APP__DEBUG": "NULL"}), {"db": {"port": 1}})
        self.assertEqual(deep_merge({}, {"a": {"b": None, "c": 1}}), {"a": {"c": 1}})

    def test_inputs_never_modified(self):
        defaults = {"db": {"host": "localhost", "pool": {"size": 5}}}
        file_config = {"db": {"host": "file", "pool": {"size": 9}}}
        snapshot = copy.deepcopy((defaults, file_config))
        first = load(defaults, file_config, {"APP__DB__POOL__SIZE": "12"})
        self.assertEqual((defaults, file_config), snapshot)
        self.assertEqual(first, {"db": {"host": "file", "pool": {"size": 12}}})
        self.assertEqual(load(defaults, {}, {}), {"db": {"host": "localhost", "pool": {"size": 5}}})

    def test_random_against_reference(self):
        for seed in range(1500):
            rnd = random.Random(seed)
            defaults, file_config = strip_nones(random_tree(rnd)), random_tree(rnd)
            env = random_env(rnd)
            snapshot = copy.deepcopy((defaults, file_config, env))
            want = ref_merge(ref_merge(defaults, file_config), ref_env(env))
            context = f"seed {seed}: defaults {defaults}, file {file_config}, env {env}"
            self.assertEqual(deep_merge(defaults, file_config), ref_merge(defaults, file_config), context)
            self.assertEqual(env_overrides(env), ref_env(env), context)
            self.assertEqual(load(defaults, file_config, env), want, context)
            self.assertEqual((defaults, file_config, env), snapshot, f"inputs were modified; {context}")


if __name__ == "__main__":
    unittest.main()
