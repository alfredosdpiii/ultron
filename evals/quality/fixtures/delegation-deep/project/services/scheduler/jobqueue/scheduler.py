"""The scheduler: submissions, the tick loop, outcomes, retries, cancellation and the event log."""
from .backoff import Backoff
from .graph import DependencyGraph
from .job import CANCELLED, FAILED, QUEUED, RUNNING, SUCCEEDED, WAITING, check_int, make_job
from .pool import WorkerPool
from .queue import ReadyQueue


class Scheduler:
    """Runs jobs on `slots` worker slots. `outcome(job_id, attempt)` decides whether an attempt succeeded."""

    def __init__(self, slots, outcome, backoff_base=1, backoff_cap=16):
        check_int("slots", slots, 1)
        if not callable(outcome):
            raise ValueError("outcome must be callable")
        self._outcome = outcome
        self._backoff = Backoff(backoff_base, backoff_cap)
        self._registry = {}
        self._queue = ReadyQueue(self._registry)
        self._pool = WorkerPool(slots)
        self._graph = DependencyGraph(self._registry)
        self._retry_delays = {}
        self._now = 0
        self._seq = 0
        self._events = []
        self._stats = {"submitted": 0, "succeeded": 0, "failed": 0, "cancelled": 0, "retries": 0, "busy_ticks": 0}

    @property
    def now(self):
        return self._now

    @property
    def events(self):
        return list(self._events)

    def stats(self):
        return {**self._stats, "starts": self._pool.starts}

    def state(self, job_id):
        return self._job(job_id).state

    def attempts(self, job_id):
        return self._job(job_id).attempts

    def jobs(self):
        return {job_id: job.state for job_id, job in sorted(self._registry.items())}

    def _job(self, job_id):
        try:
            return self._registry[job_id]
        except KeyError:
            raise KeyError(f"unknown job {job_id!r}") from None

    def _log(self, kind, job):
        self._events.append((self._now, kind, job.id))

    # Submission and cancellation --------------------------------------------------------------------------

    def submit(self, job_id, *, priority=0, run_at=None, duration=1, max_retries=0, deps=()):
        """Submit a job at the current tick; returns its state (queued, waiting or cancelled)."""
        previous = self._registry.get(job_id)
        if previous is not None and not previous.finished:
            raise ValueError(f"job {job_id!r} is still {previous.state}")
        job = make_job(
            job_id,
            self._seq,
            self._now,
            priority=priority,
            run_at=run_at,
            duration=duration,
            max_retries=max_retries,
            deps=deps,
        )
        for dep in job.deps:
            if dep not in self._registry:
                raise ValueError(f"job {job_id!r} depends on unknown job {dep!r}")
        self._seq += 1
        self._stats["submitted"] += 1
        self._registry[job_id] = job
        if not self._graph.attach(job):
            self._cancel(job)
        elif job.unmet == 0:
            self._enqueue(job, job.eligible_at)
        return job.state

    def cancel(self, job_id):
        """Cancel a waiting or queued job and everything waiting on it. Returns whether `job_id` was cancelled."""
        job = self._registry.get(job_id)
        if job is None or job.state not in (WAITING, QUEUED):
            return False
        self._cancel(job)
        return True

    def _cancel(self, job):
        job.state = CANCELLED
        self._stats["cancelled"] += 1
        self._log("cancel", job)
        self._doom(job)

    def _doom(self, job):
        for dependent in self._graph.doomed_by(job):
            dependent.state = CANCELLED
            self._stats["cancelled"] += 1
            self._log("cancel", dependent)

    def _enqueue(self, job, eligible_at):
        job.state = QUEUED
        job.eligible_at = eligible_at
        self._queue.push(job)

    # The tick loop -------------------------------------------------------------------------------------------

    def run(self, until=None):
        """Process the current tick and every later tick at which something happens, up to `until` inclusive.

        Afterwards `now` is `until`, or with `until=None` the last tick processed once nothing is left to do.
        """
        if until is not None:
            check_int("until", until, self._now)
        while True:
            self._tick()
            upcoming = [t for t in (self._pool.next_end(), self._queue.next_time()) if t is not None]
            if not upcoming:
                break
            following = min(upcoming)
            if until is not None and following > until:
                break
            self._now = following
        if until is not None:
            self._now = until
        return self._now

    def _tick(self):
        for job in self._pool.due(self._now):
            self._complete(job)
        self._queue.promote(self._now)
        while self._pool.free > 0:
            job = self._queue.pop(self._now)
            if job is None:
                break
            job.state = RUNNING
            job.attempts += 1
            self._pool.start(job, self._now)
            self._log("start", job)

    def _complete(self, job):
        attempt = job.attempts - 1
        self._stats["busy_ticks"] += job.duration
        if self._outcome(job.id, attempt):
            job.state = SUCCEEDED
            self._stats["succeeded"] += 1
            self._log("done", job)
            self._retry_delays.pop(job.id, None)
            for dependent in self._graph.released_by(job):
                self._enqueue(dependent, max(dependent.run_at, self._now))
            return
        self._log("fail", job)
        if job.can_retry():
            self._stats["retries"] += 1
            delays = self._retry_delays.setdefault(job.id, self._backoff.schedule())
            self._enqueue(job, self._now + next(delays))
            return
        self._retry_delays.pop(job.id, None)
        job.state = FAILED
        self._stats["failed"] += 1
        self._doom(job)
