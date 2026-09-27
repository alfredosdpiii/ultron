"""Hidden checks for services/kvstore (segstore): SPEC cases plus randomized comparison with a reference model."""
import random
import unittest

from segstore import HEADER_SIZE, CorruptSegment, Store, TornRecord, decode, encode, recover

KEYS = [f"{p}{i}" for p in ("k", "m/", "z") for i in range(6)]


class RefLog:
    """Reference: the live contents as a plain dict."""

    def __init__(self):
        self.state = {}

    def put(self, key, value):
        self.state[key] = value

    def delete(self, key):
        return self.state.pop(key, None) is not None


def ops(r, count):
    out = []
    for _ in range(count):
        x = r.random()
        key = r.choice(KEYS)
        if x < 0.6:
            out.append(("put", key, bytes(r.randrange(256) for _ in range(r.randint(0, 16)))))
        elif x < 0.85:
            out.append(("delete", key))
        else:
            out.append(("get", key))
    return out


def play(test, store, ref, sequence, context):
    for op in sequence:
        if op[0] == "put":
            store.put(op[1], op[2])
            ref.put(op[1], op[2])
        elif op[0] == "delete":
            test.assertEqual(store.delete(op[1]), ref.delete(op[1]), context)
        else:
            test.assertEqual(store.get(op[1]), ref.state.get(op[1]), context)


def same(test, store, ref, context):
    test.assertEqual(dict(store.scan()), ref.state, context)
    test.assertEqual(store.keys(), sorted(ref.state), context)
    for key, value in ref.state.items():
        test.assertEqual(store.get(key), value, context)
    test.assertEqual(len(store), len(ref.state), context)


class KvstoreHidden(unittest.TestCase):
    def test_record_format(self):
        raw = encode("ab", b"xyz")
        self.assertEqual(len(raw), HEADER_SIZE + 5)
        self.assertEqual(HEADER_SIZE, 11)
        record, following = decode(raw)
        self.assertEqual((record.key, record.value, record.tombstone, following), ("ab", b"xyz", False, 16))
        tomb, _ = decode(encode("ab", tombstone=True))
        self.assertTrue(tomb.tombstone)
        self.assertEqual(tomb.value, b"")
        with self.assertRaises(TornRecord):
            decode(raw[:12])

    def test_spec_example(self):
        store = Store(segment_limit=30)
        store.put("a", b"1")
        store.put("b", b"2")
        store.put("a", b"3")  # 13 bytes each: the third record starts a new segment
        self.assertEqual(store.get("a"), b"3")
        self.assertEqual(store.stats()["segments"], 2)
        self.assertTrue(store.delete("b"))
        self.assertFalse(store.delete("b"))
        self.assertEqual(store.scan(), [("a", b"3")])
        with self.assertRaises(ValueError):
            store.put("big", b"x" * 20)

    def test_rollover_reads(self):
        store = Store(segment_limit=30)
        for index in range(20):
            store.put(f"k{index % 4}", bytes([index]) * (index % 5))
            self.assertEqual(store.get(f"k{index % 4}"), bytes([index]) * (index % 5), index)

    def test_scan(self):
        store = Store(100)
        for key in ["b/2", "a/1", "b/1", "c", "b/10"]:
            store.put(key, key.encode())
        self.assertEqual([k for k, _ in store.scan(prefix="b/")], ["b/1", "b/10", "b/2"])
        self.assertEqual([k for k, _ in store.scan(start="b/1", end="b/2")], ["b/1", "b/10"])
        self.assertEqual([k for k, _ in store.scan(start="a", end="c", prefix="b/1")], ["b/1", "b/10"])

    def test_random_reads(self):
        for seed in range(5000, 5250):
            r = random.Random(seed)
            limit = r.randint(35, 120)
            store, ref = Store(limit), RefLog()
            play(self, store, ref, ops(r, r.randint(10, 50)), f"seed {seed}")
            same(self, store, ref, f"seed {seed}")

    def test_compaction_same_segment_rewrites(self):
        store = Store(60)
        store.put("x", b"old")
        store.put("x", b"new")
        store.put("y", b"")
        store.put("filler", b"f" * 20)  # rolls over: the first segment is closed
        store.compact()
        self.assertEqual(store.get("x"), b"new")
        self.assertEqual(store.get("y"), b"")

    def test_random_compaction(self):
        for seed in range(7000, 7250):
            r = random.Random(seed)
            limit = r.randint(45, 150)
            store, ref = Store(limit), RefLog()
            for _ in range(r.randint(1, 3)):
                play(self, store, ref, ops(r, r.randint(10, 45)), f"seed {seed}")
                before = store.stats()["bytes"]
                store.compact()
                self.assertLessEqual(store.stats()["bytes"], before, f"seed {seed}")
                same(self, store, ref, f"seed {seed} after compaction")

    def test_recover_after_compaction(self):
        store = Store(40)
        store.put("a", b"1")
        store.put("b", b"2")
        store.put("c", b"3")
        store.put("a", b"4")
        store.compact()
        store.delete("b")
        store.put("c", b"5")
        exported = store.export()
        recovered = recover(list(reversed(exported)), 40)
        self.assertEqual(dict(recovered.scan()), {"a": b"4", "c": b"5"})
        ids = [segment_id for segment_id, _ in exported]
        self.assertEqual(ids, sorted(ids))

    def test_random_compact_recover(self):
        for seed in range(9000, 9250):
            r = random.Random(seed)
            limit = r.randint(45, 150)
            store, ref = Store(limit), RefLog()
            for _ in range(r.randint(1, 3)):
                play(self, store, ref, ops(r, r.randint(8, 40)), f"seed {seed}")
                store.compact()
            play(self, store, ref, ops(r, r.randint(0, 15)), f"seed {seed}")
            exported = store.export()
            r.shuffle(exported)
            recovered = recover(exported, limit)
            same(self, recovered, ref, f"seed {seed} recovered")
            play(self, recovered, ref, ops(r, 10), f"seed {seed} recovered")
            same(self, recovered, ref, f"seed {seed} recovered then written")

    def test_torn_tail(self):
        store = Store(200)
        store.put("a", b"1")
        store.put("b", b"22")
        store.delete("a")
        [(segment_id, data)] = store.export()
        first = len(encode("a", b"1"))
        second = first + len(encode("b", b"22"))
        for cut, expected, kept in [
            (0, {}, 0),
            (first - 1, {}, 0),
            (first, {"a": b"1"}, first),
            (first + HEADER_SIZE, {"a": b"1"}, first),
            (second, {"a": b"1", "b": b"22"}, second),
            (len(data) - 1, {"a": b"1", "b": b"22"}, second),
            (len(data), {"b": b"22"}, len(data)),
        ]:
            recovered = recover([(segment_id, data[:cut])], 200)
            self.assertEqual(dict(recovered.scan()), expected, cut)
            self.assertEqual(recovered.stats()["bytes"], kept, cut)
        flipped = bytearray(data)
        flipped[first + 3] ^= 0x10
        self.assertEqual(dict(recover([(segment_id, bytes(flipped))], 200).scan()), {"a": b"1"})

    def test_damage_before_last_segment(self):
        store = Store(30)
        for index in range(6):
            store.put(f"k{index}", b"v" * 5)
        exported = store.export()
        segment_id, data = exported[0]
        exported[0] = (segment_id, data[:-1])
        with self.assertRaises(CorruptSegment):
            recover(exported, 30)


if __name__ == "__main__":
    unittest.main()
