"""Hidden checks for services/patch (linediff): SPEC examples plus randomized comparison with independent models."""
import random
import unittest

from linediff import (
    Hunk,
    PatchError,
    apply_hunks,
    apply_patch,
    diff_lines,
    format_hunks,
    format_unified,
    lcs_length,
    locate,
    make_hunks,
    offsets,
    parse_unified,
    reverse_hunks,
)

WORDS = ["a", "b", "c", "", "-- q", "++ r", "--- s", "+++ t", "@@ u"]


def lcs(a, b):
    table = [[0] * (len(b) + 1) for _ in range(len(a) + 1)]
    for i, x in enumerate(a):
        for j, y in enumerate(b):
            table[i + 1][j + 1] = table[i][j] + 1 if x == y else max(table[i][j + 1], table[i + 1][j])
    return table[-1][-1]


def ref_groups(a, b, context):
    """Old-file change regions from diff_lines, merged by the SPEC rule, as (first change, end of last change)."""
    regions = []
    for op in diff_lines(a, b):
        if op.tag == "equal":
            continue
        if regions and regions[-1][1] == op.i1:
            regions[-1][1] = op.i2
        else:
            regions.append([op.i1, op.i2])
    merged = []
    for start, end in regions:
        if merged and start - merged[-1][1] <= 2 * context:
            merged[-1][1] = end
        else:
            merged.append([start, end])
    return merged


def ref_apply(lines, hunks):
    out, floor, offset = [], 0, 0
    for index, hunk in enumerate(hunks):
        old = [t for tag, t in hunk.lines if tag in " -"]
        new = [t for tag, t in hunk.lines if tag in " +"]
        anchor = hunk.old_start - 1 if hunk.old_count > 0 else hunk.old_start
        want = anchor + offset
        best = None
        for pos in range(floor, len(lines) - len(old) + 1):
            if lines[pos:pos + len(old)] == old and (best is None or abs(pos - want) < abs(best - want)):
                best = pos
        if best is None:
            return ("error", index)
        out.extend(lines[floor:best])
        out.extend(new)
        floor, offset = best + len(old), best - anchor
    return out + lines[floor:]


def run_apply(lines, hunks):
    try:
        return apply_hunks(lines, hunks)
    except PatchError as error:
        return ("error", error.index)


def mutate(rnd, a, words, edits=4):
    b = list(a)
    for _ in range(rnd.randint(0, edits)):
        roll = rnd.random()
        if roll < 0.35 and b:
            b.pop(rnd.randrange(len(b)))
        elif roll < 0.7:
            b.insert(rnd.randint(0, len(b)), rnd.choice(words))
        elif b:
            b[rnd.randrange(len(b))] = rnd.choice(words)
    return b


class PatchHidden(unittest.TestCase):
    def test_spec_text_example(self):
        a = ["one", "two", "three", "four"]
        b = ["one", "2", "three", "four", "five"]
        self.assertEqual(
            format_unified(a, b, 1),
            "--- a\n+++ b\n@@ -1,4 +1,5 @@\n one\n-two\n+2\n three\n four\n+five\n",
        )
        self.assertEqual(format_unified(a, a), "")
        self.assertEqual(format_unified([], ["x"], 3, "old", "new"), "--- old\n+++ new\n@@ -0,0 +1,1 @@\n+x\n")
        self.assertEqual(make_hunks(["x"], [], 0), [Hunk(1, 1, 0, 0, [("-", "x")])])
        with self.assertRaises(ValueError):
            make_hunks(a, b, -1)

    def test_body_lines_that_look_like_headers(self):
        a = ["keep", "-- gone", "keep2"]
        b = ["keep", "++ added", "keep2"]
        text = format_unified(a, b, 1)
        self.assertIn("\n--- gone\n", text)
        self.assertIn("\n+++ added\n", text)
        hunks = parse_unified(text)
        self.assertEqual(hunks, make_hunks(a, b, 1))
        self.assertEqual(apply_patch(a, text), b)
        body = "@@ -1,2 +1,2 @@\n--- x\n+@@ -9,9 +9,9 @@\n y\n"
        self.assertEqual(parse_unified(body), [Hunk(1, 2, 1, 2, [("-", "-- x"), ("+", "@@ -9,9 +9,9 @@"), (" ", "y")])])

    def test_parse_errors(self):
        for text in ["@@ -1,2 +1,2 @@\n a\n", "@@ -1,1 +1,1 @@\n?a\n", "garbage\n", "@@ -1,1 +1,1 @@\n-a\n+b\n c\n"]:
            with self.assertRaises(ValueError, msg=text):
                parse_unified(text)

    def test_touching_regions_merge(self):
        a = [str(n) for n in range(10)]
        b = list(a)
        b[2] = "two"
        b[6] = "six"
        # three unchanged lines between the changes: context 1 keeps them apart, context 2 (gap 3 <= 4) merges
        self.assertEqual(len(make_hunks(a, b, 1)), 2)
        self.assertEqual(len(make_hunks(a, b, 2)), 1)
        b = list(a)
        b[2] = "two"
        b[5] = "five"
        # two unchanged lines between the changes: with context 1 the contexts touch, so one hunk
        self.assertEqual([h.header() for h in make_hunks(a, b, 1)], ["@@ -2,6 +2,6 @@"])
        # the same files asked again with less context split again
        self.assertEqual([h.header() for h in make_hunks(a, b, 0)], ["@@ -3,1 +3,1 @@", "@@ -6,1 +6,1 @@"])

    def test_offset_ties_toward_earlier(self):
        hunk = Hunk(3, 1, 2, 0, [("-", "x")])
        self.assertEqual(apply_hunks(["x", "o", "o", "o", "x"], [hunk]), ["o", "o", "o", "x"])
        self.assertEqual(offsets(["x", "o", "o", "o", "x"], [hunk]), [-2])
        self.assertEqual(locate(["m", "k", "m"], ["m"], 1), 0)
        self.assertEqual(apply_hunks(["o", "x", "o", "x"], [hunk]), ["o", "o", "x"])
        with self.assertRaises(PatchError) as caught:
            apply_hunks(["o"], [Hunk(1, 1, 1, 1, [(" ", "o")]), hunk])
        self.assertEqual(caught.exception.index, 1)

    def test_random_scripts_are_minimal(self):
        rnd = random.Random(7_331)
        for _ in range(250):
            a = [rnd.choice("abc") for _ in range(rnd.randint(0, 10))]
            b = [rnd.choice("abc") for _ in range(rnd.randint(0, 10))]
            ops = diff_lines(a, b)
            i = j = 0
            for op in ops:
                self.assertEqual((op.i1, op.j1), (i, j))
                if op.tag == "equal":
                    self.assertEqual(a[op.i1:op.i2], b[op.j1:op.j2])
                i, j = op.i2, op.j2
            self.assertEqual((i, j), (len(a), len(b)))
            self.assertEqual(sum(op.i2 - op.i1 for op in ops if op.tag == "equal"), lcs(a, b))
            self.assertEqual(lcs_length(a, b), lcs(a, b))

    def test_random_round_trips_and_shapes(self):
        rnd = random.Random(90_210)
        for _ in range(400):
            a = [rnd.choice(WORDS) for _ in range(rnd.randint(0, 14))]
            b = mutate(rnd, mutate(rnd, a, WORDS), WORDS)
            for context in (3, 2, 1, 0):
                self.check_pair(a, b, context)

    def check_pair(self, a, b, context):
        hunks = make_hunks(a, b, context)
        text = format_hunks(hunks)
        self.assertEqual(parse_unified(text), hunks)
        self.assertEqual(apply_patch(a, text), b, (a, b, context))
        self.assertEqual(apply_hunks(b, reverse_hunks(hunks)), a)
        self.assertEqual(reverse_hunks(reverse_hunks(hunks)), hunks)
        groups = ref_groups(a, b, context)
        self.assertEqual(len(hunks), len(groups), (a, b, context))
        for hunk, (start, end) in zip(hunks, groups):
            lo, hi = max(0, start - context), min(len(a), end + context)
            self.assertEqual(hunk.old_count, hi - lo)
            self.assertEqual(hunk.old_start, lo + 1 if hi > lo else lo)

    def test_random_offsets_against_model(self):
        rnd = random.Random(4_242)
        for _ in range(500):
            words = ["p", "q", "r"]
            a = [rnd.choice(words) for _ in range(rnd.randint(2, 12))]
            b = mutate(rnd, a, words)
            hunks = make_hunks(a, b, rnd.randint(1, 2))
            moved = mutate(rnd, a, words, 3)
            self.assertEqual(run_apply(moved, hunks), ref_apply(moved, hunks), (a, b, moved))


if __name__ == "__main__":
    unittest.main()
