"""The queue of jobs waiting for a worker slot.

Two heaps: `_delayed` holds queued jobs by the tick they become eligible, `_ready` holds eligible jobs in dispatch
order. Cancelled and replaced records are not removed from the heaps; their entries are dropped when they surface.
"""
import heapq

from .job import QUEUED


class ReadyQueue:
    def __init__(self, registry):
        self._registry = registry
        self._delayed = []
        self._ready = []

    def push(self, job):
        """Queue `job` (state QUEUED, `eligible_at` set)."""
        heapq.heappush(self._delayed, (job.eligible_at, job.seq, job.id))

    def _live(self, entry):
        job = self._registry.get(entry[-1])
        return job is not None and job.state == QUEUED

    def promote(self, now):
        """Move every job eligible at `now` into dispatch order."""
        while self._delayed and self._delayed[0][0] <= now:
            entry = heapq.heappop(self._delayed)
            if not self._live(entry):
                continue
            job = self._registry[entry[-1]]
            heapq.heappush(self._ready, (*job.sort_key(), job.id))

    def pop(self, now):
        """The best job eligible at `now`, removed from the queue, or None."""
        self.promote(now)
        while self._ready:
            entry = heapq.heappop(self._ready)
            if self._live(entry):
                return self._registry[entry[-1]]
        return None

    def next_time(self):
        """The earliest tick at which a queued job not yet eligible becomes eligible, or None."""
        while self._delayed and not self._live(self._delayed[0]):
            heapq.heappop(self._delayed)
        return self._delayed[0][0] if self._delayed else None

    def __len__(self):
        return sum(1 for entry in self._delayed + self._ready if self._live(entry))
