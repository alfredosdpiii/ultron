import unittest

from confmerge import coerce, load

DEFAULTS = {"db": {"host": "localhost", "port": 5432, "pool": {"size": 5}}, "debug": False, "tags": ["a"]}


class QuickTest(unittest.TestCase):
    def test_layers(self):
        config = load(DEFAULTS, {"db": {"host": "db.internal"}, "tags": ["b"]}, {"APP__DEBUG": "true", "HOME": "/root"})
        self.assertEqual(config, {"db": {"host": "db.internal", "port": 5432, "pool": {"size": 5}}, "debug": True, "tags": ["b"]})

    def test_nested_env(self):
        config = load(DEFAULTS, {}, {"APP__DB__PORT": "6543"})
        self.assertEqual(config["db"]["port"], 6543)

    def test_coerce(self):
        self.assertEqual(coerce("12"), 12)
        self.assertEqual(coerce("0.5"), 0.5)
        self.assertEqual(coerce("hello"), "hello")


if __name__ == "__main__":
    unittest.main()
