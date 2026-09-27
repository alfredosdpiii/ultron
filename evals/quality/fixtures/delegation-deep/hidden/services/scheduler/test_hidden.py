"""Hidden checks for services/scheduler (package jobqueue): SPEC cases plus randomized comparison with a reference."""
import random
import unittest

from jobqueue import Backoff, Scheduler

FINAL = ("succeeded", "failed", "cancelled")


class Rec:
    def __init__(self, job_id, seq, priority, run_at, duration, max_retries, deps):
        self.id, self.seq, self.priority, self.run_at = job_id, seq, priority, run_at
        self.duration, self.max_retries, self.deps = duration, max_retries, deps
        self.state, self.attempts, self.eligible, self.end, self.order = "waiting", 0, None, None, None


class RefScheduler:
    """Tick-by-tick model of the SPEC, written independently of the package."""

    def __init__(self, slots, outcome, base, cap):
        self.slots, self.outcome, self.base, self.cap = slots, outcome, base, cap
        self.now, self.recs, self.by_id, self.events, self.starts = 0, [], {}, [], 0
        self.st = dict(submitted=0, succeeded=0, failed=0, cancelled=0, retries=0, busy_ticks=0)

    def log(self, kind, rec):
        self.events.append((self.now, kind, rec.id))

    def submit(self, job_id, priority, run_at, duration, max_retries, deps):
        old = self.by_id.get(job_id)
        if old is not None and old.state not in FINAL:
            return "error"
        rec = Rec(job_id, len(self.recs), priority, self.now if run_at is None else run_at, duration, max_retries,
                  [self.by_id[d] for d in dict.fromkeys(deps)])
        self.recs.append(rec)
        self.by_id[job_id] = rec
        self.st["submitted"] += 1
        if any(d.state in ("failed", "cancelled") for d in rec.deps):
            self.drop(rec)
        elif all(d.state == "succeeded" for d in rec.deps):
            rec.state, rec.eligible = "queued", max(rec.run_at, self.now)
        return rec.state

    def drop(self, rec):
        rec.state = "cancelled"
        self.st["cancelled"] += 1
        self.log("cancel", rec)
        self.fallout(rec)

    def fallout(self, root):
        dead, grew = {root.seq}, True
        while grew:
            grew = False
            for rec in self.recs:
                if rec.state == "waiting" and rec.seq not in dead and any(d.seq in dead for d in rec.deps):
                    dead.add(rec.seq)
                    grew = True
        for rec in self.recs:
            if rec.seq in dead and rec is not root:
                rec.state = "cancelled"
                self.st["cancelled"] += 1
                self.log("cancel", rec)

    def cancel(self, job_id):
        rec = self.by_id.get(job_id)
        if rec is None or rec.state not in ("waiting", "queued"):
            return False
        self.drop(rec)
        return True

    def step(self):
        t = self.now
        for rec in sorted((r for r in self.recs if r.state == "running" and r.end == t), key=lambda r: r.order):
            attempt = rec.attempts - 1
            self.st["busy_ticks"] += rec.duration
            if self.outcome(rec.id, attempt):
                rec.state = "succeeded"
                self.st["succeeded"] += 1
                self.log("done", rec)
                for other in self.recs:
                    if other.state == "waiting" and all(d.state == "succeeded" for d in other.deps):
                        other.state, other.eligible = "queued", max(other.run_at, t)
            else:
                self.log("fail", rec)
                if attempt < rec.max_retries:
                    self.st["retries"] += 1
                    rec.state, rec.eligible = "queued", t + min(self.base * 2**attempt, self.cap)
                else:
                    rec.state = "failed"
                    self.st["failed"] += 1
                    self.fallout(rec)
        while sum(r.state == "running" for r in self.recs) < self.slots:
            ready = sorted((r for r in self.recs if r.state == "queued" and r.eligible <= t),
                           key=lambda r: (-r.priority, r.eligible, r.seq))
            if not ready:
                break
            rec = ready[0]
            rec.state, rec.end, rec.order = "running", t + rec.duration, self.starts
            rec.attempts += 1
            self.starts += 1
            self.log("start", rec)

    def run(self, until):
        while True:
            self.step()
            if not any(r.state in ("running", "queued") for r in self.recs):
                break
            if until is not None and self.now >= until:
                break
            self.now += 1
        if until is not None:
            self.now = until
        return self.now

    def stats(self):
        return {**self.st, "starts": self.starts}

    def jobs(self):
        return {job_id: rec.state for job_id, rec in sorted(self.by_id.items())}


def scenario(seed):
    rnd = random.Random(seed)
    slots = rnd.randint(1, 4)
    base, cap = rnd.randint(1, 5), rnd.randint(1, 50)
    fail_rate = rnd.choice([0.0, 0.2, 0.4, 0.6])
    ids = [f"t{n}" for n in range(rnd.randint(2, 8))]
    ops, known, now = [], [], 0
    for _ in range(rnd.randint(1, 5)):
        for _ in range(rnd.randint(1, 6)):
            job_id = rnd.choice(ids)
            deps = tuple(rnd.choice(known) for _ in range(rnd.randint(0, 3))) if known and rnd.random() < 0.5 else ()
            deps = tuple(d for d in deps if d != job_id)
            ops.append(("submit", job_id, rnd.randint(-1, 3), rnd.choice([None, now + rnd.randint(0, 8)]),
                        rnd.randint(1, 6), rnd.randint(0, 6), deps))
            if job_id not in known:
                known.append(job_id)
            if rnd.random() < 0.3:
                ops.append(("cancel", rnd.choice(ids)))
        now += rnd.randint(0, 6)
        ops.append(("run", now))
    ops.append(("run", None))
    outcome = lambda job_id, attempt: random.Random(f"h{seed}/{job_id}/{attempt}").random() >= fail_rate  # noqa: E731
    return slots, base, cap, outcome, ops


def apply(target, op, reference):
    if op[0] == "submit":
        _, job_id, priority, run_at, duration, max_retries, deps = op
        if reference:
            return target.submit(job_id, priority, run_at, duration, max_retries, deps)
        try:
            return target.submit(job_id, priority=priority, run_at=run_at, duration=duration,
                                 max_retries=max_retries, deps=deps)
        except ValueError:
            return "error"
    if op[0] == "cancel":
        return target.cancel(op[1])
    return target.run(op[1])


class SchedulerHidden(unittest.TestCase):
    def test_spec_example(self):
        s = Scheduler(1, lambda job_id, attempt: attempt > 0, backoff_base=2, backoff_cap=16)
        self.assertEqual(s.submit("a", max_retries=1), "queued")
        self.assertEqual(s.submit("b", deps=["a"]), "waiting")
        self.assertEqual(s.run(), 5)
        self.assertEqual(s.events, [(0, "start", "a"), (1, "fail", "a"), (3, "start", "a"), (4, "done", "a"),
                                    (4, "start", "b"), (5, "fail", "b")])
        self.assertEqual(s.state("b"), "failed")
        self.assertEqual(s.attempts("a"), 2)

    def test_same_tick_completions_in_start_order(self):
        s = Scheduler(2, lambda job_id, attempt: True)
        s.submit("b", priority=0, duration=2)
        s.submit("a", priority=5, duration=3)
        s.submit("c", priority=4, duration=1)
        s.run()
        # "a" started at 0 and "b" at 1 (on the slot "c" freed): both end at 3, "a" first.
        self.assertEqual(s.events, [(0, "start", "a"), (0, "start", "c"), (1, "done", "c"), (1, "start", "b"),
                                    (3, "done", "a"), (3, "done", "b")])

    def test_cancel_then_resubmit_uses_the_new_submission(self):
        s = Scheduler(1, lambda job_id, attempt: True)
        s.submit("x", priority=9, run_at=2)
        s.submit("y", priority=1, run_at=2, duration=2)
        self.assertTrue(s.cancel("x"))
        self.assertEqual(s.submit("x", priority=0, run_at=3), "queued")
        s.run()
        self.assertEqual(s.events, [(0, "cancel", "x"), (2, "start", "y"), (4, "done", "y"), (4, "start", "x"),
                                    (5, "done", "x")])
        s2 = Scheduler(1, lambda job_id, attempt: True)
        s2.submit("x", run_at=1)
        s2.cancel("x")
        s2.submit("x", run_at=6)
        s2.run()
        self.assertEqual(s2.events, [(0, "cancel", "x"), (6, "start", "x"), (7, "done", "x")])

    def test_backoff_delays(self):
        self.assertEqual(Backoff(1, 10).delays(6), [1, 2, 4, 8, 10, 10])
        self.assertEqual(Backoff(3, 26).delays(5), [3, 6, 12, 24, 26])
        self.assertEqual(Backoff(2, 16).delays(5), [2, 4, 8, 16, 16])
        self.assertEqual(Backoff(5, 3).delays(3), [3, 3, 3])
        self.assertEqual(Backoff(3, 7).delays(3), [3, 6, 7])
        s = Scheduler(1, lambda job_id, attempt: attempt >= 3, backoff_base=1, backoff_cap=6)
        s.submit("r", max_retries=3)
        s.run()
        self.assertEqual([t for t, kind, _ in s.events if kind == "start"], [0, 2, 5, 10])

    def test_dependencies_and_cascade(self):
        s = Scheduler(2, lambda job_id, attempt: job_id != "a")
        s.submit("a", duration=2)
        s.submit("b", deps=["a", "a"])
        s.submit("c", deps=["b"])
        s.submit("d")
        s.submit("e", deps=["d"], run_at=0)
        s.run()
        self.assertEqual(s.jobs(), {"a": "failed", "b": "cancelled", "c": "cancelled", "d": "succeeded",
                                    "e": "succeeded"})
        self.assertIn((2, "cancel", "b"), s.events)
        self.assertEqual(s.submit("f", deps=["c"]), "cancelled")

    def test_validation(self):
        s = Scheduler(1, lambda job_id, attempt: True)
        for kwargs in ({"duration": 0}, {"max_retries": -1}, {"run_at": -1}, {"deps": ["missing"]}):
            with self.assertRaises(ValueError):
                s.submit("v", **kwargs)
        s.submit("v", duration=3)
        with self.assertRaises(ValueError):
            s.submit("v")
        self.assertFalse(s.cancel("nope"))
        for args in ((0, lambda j, a: True), (1, None)):
            with self.assertRaises(ValueError):
                Scheduler(*args)

    def test_random_against_reference(self):
        for seed in range(700):
            slots, base, cap, outcome, ops = scenario(seed)
            impl, ref = Scheduler(slots, outcome, backoff_base=base, backoff_cap=cap), RefScheduler(slots, outcome, base, cap)
            for index, op in enumerate(ops):
                got, want = apply(impl, op, False), apply(ref, op, True)
                ctx = f"seed {seed} (slots {slots}, base {base}, cap {cap}), op {index} {op}"
                self.assertEqual(impl.events, ref.events, ctx)
                self.assertEqual(got, want, ctx)
            self.assertEqual(impl.jobs(), ref.jobs(), f"seed {seed}")
            self.assertEqual(impl.stats(), ref.stats(), f"seed {seed}")
            for job_id in ref.by_id:
                self.assertEqual(impl.attempts(job_id), ref.by_id[job_id].attempts, f"seed {seed} {job_id}")


if __name__ == "__main__":
    unittest.main()
