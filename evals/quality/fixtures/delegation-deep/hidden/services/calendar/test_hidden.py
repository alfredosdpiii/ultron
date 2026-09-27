"""Hidden checks for services/calendar (meeting slots): SPEC cases plus randomized comparison with a minute-bitmap
reference."""
import random
import unittest

from slots import Attendee, IntervalSet, WeeklyHours, align_up, common_free, find_slot, free_slots

DAY = 1440
WEEK = 7 * DAY


def runs(bits, base=0):
    out, start = [], None
    for index, bit in enumerate(list(bits) + [False]):
        if bit and start is None:
            start = index
        elif not bit and start is not None:
            out.append((base + start, base + index))
            start = None
    return out


class Person:
    """Reference attendee: answers "is minute m free?" directly from the definition."""

    def __init__(self, ranges, offset, busy, buffer):
        self.ranges, self.offset, self.busy, self.buffer = ranges, offset, busy, buffer

    def free(self, minute):
        local = minute + self.offset
        clock = local % DAY
        if not any(s <= clock < e for s, e in self.ranges.get((local // DAY) % 7, [])):
            return False
        return not any(s - self.buffer <= minute < e + self.buffer for s, e in self.busy)

    def real(self):
        return Attendee("p", WeeklyHours(self.ranges, self.offset), self.busy, self.buffer)


def random_person(rnd, start, end):
    ranges = {}
    for day in range(7):
        if rnd.random() < 0.2:
            continue
        spans = []
        for _ in range(rnd.randint(1, 2)):
            low = rnd.choice([0, 30 * rnd.randint(0, 46)])
            spans.append((low, rnd.choice([DAY, min(DAY, low + 30 * rnd.randint(1, 24))])))
        ranges[day] = spans
    offset = rnd.choice([0, 120, -120, 345, -480, 600, 30 * rnd.randint(-24, 28)])
    busy = []
    for _ in range(rnd.randint(0, 6)):
        low = rnd.randint(start - 60, end + 20)
        busy.append((low, low + rnd.randint(1, 90)))
    return Person(ranges, offset, busy, rnd.choice([0, 5, 10, 20, 45]))


class IntervalSetTests(unittest.TestCase):
    def test_spec_examples(self):
        self.assertEqual(IntervalSet([(5, 10), (0, 3), (3, 4), (8, 12), (7, 7)]).intervals(), [(0, 4), (5, 12)])
        self.assertEqual(IntervalSet([(0, 5)]).union(IntervalSet([(5, 9)])).intervals(), [(0, 9)])
        self.assertEqual(IntervalSet([(0, 10)]).subtract(IntervalSet([(0, 2), (4, 5), (9, 12)])).intervals(), [(2, 4), (5, 9)])
        self.assertEqual(IntervalSet([(0, 10)]).intersect(IntervalSet([(10, 20)])).intervals(), [])
        self.assertTrue(IntervalSet([(0, 5)]).union(IntervalSet([(5, 9)])).contains(2, 8))
        with self.assertRaises(ValueError):
            IntervalSet([(0, 5)]).contains(3, 3)

    def test_touching_union_merges(self):
        left = IntervalSet([(0, 10), (20, 30)])
        right = IntervalSet([(10, 20), (30, 31)])
        self.assertEqual(left.union(right).intervals(), [(0, 31)])
        self.assertEqual(len(right.union(left)), 1)

    def test_results_are_independent_of_their_operands(self):
        empty, some = IntervalSet(), IntervalSet([(0, 5)])
        for make in (lambda: some.union(empty), lambda: empty.union(some), lambda: some.intersect(some), lambda: some.subtract(empty)):
            result = make()
            result.add(10, 20)
            self.assertEqual(some.intervals(), [(0, 5)])
            self.assertEqual(empty.intervals(), [])
            self.assertEqual(result.intervals(), [(0, 5), (10, 20)])
        grown = empty.union(empty)
        empty.add(3, 4)
        self.assertEqual(grown.intervals(), [])
        self.assertTrue(empty.contains(3, 4))

    def test_random_programs_with_add(self):
        for seed in range(300):
            rnd = random.Random(51000 + seed)
            sets, models = [], []
            for _ in range(rnd.randint(1, 3)):
                pairs = [(s, s + rnd.randint(1, 10)) for s in (rnd.randint(0, 50) for _ in range(rnd.randint(0, 3)))]
                sets.append(IntervalSet(pairs))
                models.append({m for a, b in pairs for m in range(a, b)})
            for _ in range(10):
                roll = rnd.random()
                if roll < 0.45:
                    i, j = rnd.randrange(len(sets)), rnd.randrange(len(sets))
                    op = rnd.choice(["union", "intersect", "subtract"])
                    sets.append(getattr(sets[i], op)(sets[j]))
                    models.append({"union": models[i] | models[j], "intersect": models[i] & models[j], "subtract": models[i] - models[j]}[op])
                elif roll < 0.6:
                    sets.append(IntervalSet())
                    models.append(set())
                else:
                    i = rnd.randrange(len(sets))
                    low = rnd.randint(0, 60)
                    high = low + rnd.randint(1, 15)
                    sets[i].add(low, high)
                    models[i] |= set(range(low, high))
            for got, model in zip(sets, models):
                self.assertEqual(got.intervals(), runs(m in model for m in range(90)), f"seed {seed}")

    def test_random_against_bitmap(self):
        for seed in range(500):
            rnd = random.Random(7000 + seed)
            a_pairs = [(s, s + rnd.randint(1, 12)) for s in (rnd.randint(0, 60) for _ in range(rnd.randint(0, 6)))]
            b_pairs = [(s, s + rnd.randint(1, 12)) for s in (rnd.randint(0, 60) for _ in range(rnd.randint(0, 6)))]
            a, b = IntervalSet(a_pairs), IntervalSet(b_pairs)
            in_a = [any(s <= m < e for s, e in a_pairs) for m in range(80)]
            in_b = [any(s <= m < e for s, e in b_pairs) for m in range(80)]
            for op, bits in [
                ("union", [x or y for x, y in zip(in_a, in_b)]),
                ("intersect", [x and y for x, y in zip(in_a, in_b)]),
                ("subtract", [x and not y for x, y in zip(in_a, in_b)]),
            ]:
                got = getattr(a, op)(b)
                self.assertEqual(got.intervals(), runs(bits), f"seed {seed} {op} {a_pairs} {b_pairs}")
                self.assertEqual(got.total(), sum(bits))


class HoursAndFreeTests(unittest.TestCase):
    def test_offset_moves_hours_across_midnight(self):
        # Monday 09:00-17:00 at UTC+10 is Sunday 23:00 UTC (previous week) to Monday 07:00 UTC.
        hours = WeeklyHours({"mon": [("09:00", "17:00")]}, utc_offset=600)
        self.assertEqual(hours.to_utc(0, WEEK).intervals(), [(0, 420), (WEEK - 60, WEEK)])
        with self.assertRaises(ValueError):
            WeeklyHours({0: [(600, 600)]})

    def test_buffer_of_block_just_before_window(self):
        person = Attendee("p", WeeklyHours({0: [(0, DAY)]}), [(500, 600)], buffer=30)
        self.assertEqual(person.free(600, 700).intervals(), [(630, 700)])
        self.assertEqual(person.free(400, 500).intervals(), [(400, 470)])
        self.assertEqual(person.free(0, DAY).intervals(), [(0, 470), (630, DAY)])

    def test_busy_added_after_queries(self):
        person = Attendee("p", WeeklyHours({d: [(0, DAY)] for d in range(7)}), buffer=30)
        self.assertEqual(person.free(0, 3 * DAY).intervals(), [(0, 3 * DAY)])
        person.add_busy(DAY + 5, DAY + 10)
        self.assertEqual(person.free(0, DAY).intervals(), [(0, DAY - 25)])
        person.add_busy(2 * DAY - 10, 2 * DAY - 1)
        self.assertEqual(person.free(2 * DAY, 3 * DAY).intervals(), [(2 * DAY + 29, 3 * DAY)])
        self.assertEqual(person.free(DAY - 40, 2 * DAY + 40).intervals(), [(DAY - 40, DAY - 25), (DAY + 40, 2 * DAY - 40), (2 * DAY + 29, 2 * DAY + 40)])
        self.assertEqual(person.busy(), [(DAY + 5, DAY + 10), (2 * DAY - 10, 2 * DAY - 1)])

    def test_random_incremental_against_reference(self):
        for seed in range(200):
            rnd = random.Random(64000 + seed)
            start = rnd.randint(0, 2 * WEEK)
            end = start + rnd.randint(60, 2 * DAY)
            person = random_person(rnd, start, end)
            real = person.real()
            for _ in range(4):
                low = rnd.randint(start - DAY // 2, end)
                high = low + rnd.randint(30, DAY)
                self.assertEqual(real.free(low, high).intervals(), runs([person.free(m) for m in range(low, high)], low), f"seed {seed}")
                boundary = (rnd.randint(start, end) // DAY + 1) * DAY
                block_start = rnd.choice([rnd.randint(start, end), boundary - rnd.randint(1, 40), boundary + rnd.randint(0, 30)])
                block = (block_start, block_start + rnd.randint(1, 50))
                real.add_busy(*block)
                person.busy.append(block)

    def test_random_free_against_reference(self):
        for seed in range(250):
            rnd = random.Random(9100 + seed)
            start = rnd.randint(0, 2 * WEEK)
            end = start + rnd.randint(20, DAY)
            person = random_person(rnd, start, end)
            expected = runs([person.free(m) for m in range(start, end)], start)
            self.assertEqual(person.real().free(start, end).intervals(), expected, f"seed {seed}")

    def test_common_free_and_slots(self):
        for seed in range(80):
            rnd = random.Random(12000 + seed)
            start = rnd.randint(0, 2 * WEEK)
            end = start + rnd.randint(60, DAY)
            people = [random_person(rnd, start, end) for _ in range(rnd.randint(1, 3))]
            bits = [all(p.free(m) for p in people) for m in range(start, end)]
            real = [p.real() for p in people]
            self.assertEqual(common_free(real, start, end).intervals(), runs(bits, start), f"seed {seed}")
            self.assertEqual(free_slots(real, start, end, 20), [r for r in runs(bits, start) if r[1] - r[0] >= 20])


class FindSlotTests(unittest.TestCase):
    def test_spec_examples(self):
        office = WeeklyHours.office()  # Monday to Friday, 09:00-17:00 UTC
        alice = Attendee("alice", office, [(540, 600)])
        self.assertEqual(find_slot([alice], 30, 0, 15), 600)
        # Starts are multiples of the granularity in UTC, not counted from `earliest`.
        self.assertEqual(find_slot([alice], 30, 607, 15), 615)
        self.assertEqual(find_slot([alice], 30, 600, 15), 600)
        # Friday 16:45 is too late for an hour: the next start is Monday 09:00 of the next week.
        self.assertEqual(find_slot([alice], 60, 4 * DAY + 1005, 15), WEEK + 540)
        self.assertIsNone(find_slot([alice], 60, 0, 15, horizon=100))
        with self.assertRaises(ValueError):
            find_slot([], 30, 0)
        with self.assertRaises(ValueError):
            find_slot([alice], 0, 0)

    def test_align_up(self):
        self.assertEqual([align_up(v, 15) for v in (0, 1, 14, 15, 16)], [0, 15, 15, 15, 30])

    def test_fresh_attendees_are_not_confused(self):
        office = WeeklyHours.office()
        for minute in range(1, 400, 7):
            busy_until = 540 + minute
            got = find_slot([Attendee("x", office, [(540, busy_until)])], 30, 0, 1)
            self.assertEqual(got, busy_until, busy_until)
        person = Attendee("y", office)
        self.assertEqual(find_slot([person], 30, 0), 540)
        person.add_busy(540, 700)
        self.assertEqual(find_slot([person], 30, 0), 700)

    def test_random_against_reference(self):
        for seed in range(120):
            rnd = random.Random(33000 + seed)
            earliest = rnd.randint(0, 2 * WEEK)
            horizon = rnd.randint(DAY // 3, int(1.5 * DAY))
            people = [random_person(rnd, earliest, earliest + horizon) for _ in range(rnd.randint(1, 3))]
            duration = rnd.choice([10, 30, 45, 60, 100])
            granularity = rnd.choice([1, 7, 15, 20, 30])
            bits = [all(p.free(m) for p in people) for m in range(earliest, earliest + horizon)]
            expected = None
            t = -(-earliest // granularity) * granularity
            while t + duration <= earliest + horizon:
                if all(bits[t - earliest : t - earliest + duration]):
                    expected = t
                    break
                t += granularity
            got = find_slot([p.real() for p in people], duration, earliest, granularity, horizon)
            self.assertEqual(got, expected, f"seed {seed}: duration {duration} granularity {granularity}")


if __name__ == "__main__":
    unittest.main()
