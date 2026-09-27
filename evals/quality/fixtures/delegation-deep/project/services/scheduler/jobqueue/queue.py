"""The queue of jobs waiting for a worker slot.

Two heaps: `_delayed` holds queued jobs by the tick they become eligible, `_ready` holds eligible jobs in dispatch
order. Cancelled and replaced records are not removed from the heaps; their entries are dropped when they surface.
The earliest eligible tick of the delayed heap is kept in `_next`, so the scheduler can ask for its next wake-up
tick without touching the heap.
"""
import heapq

from .job import QUEUED


class ReadyQueue:
    def __init__(self, registry):
        self._registry = registry
        self._delayed = []
        self._ready = []
        self._next = None

    def push(self, job):
        """Queue `job` (state QUEUED, `eligible_at` set)."""
        entry = (job.eligible_at, job.seq, job.id)
        heapq.heappush(self._delayed, entry)
        if self._next is not None and entry[0] < self._next:
            self._next = entry[0]

    def _live(self, entry):
        job = self._registry.get(entry[-1])
        return job is not None and job.seq == entry[-2] and job.state == QUEUED

    def _refresh(self):
        while self._delayed and not self._live(self._delayed[0]):
            heapq.heappop(self._delayed)
        self._next = self._delayed[0][0] if self._delayed else None

    def promote(self, now):
        """Move every job eligible at `now` into dispatch order."""
        moved = False
        while self._delayed and self._delayed[0][0] <= now:
            entry = heapq.heappop(self._delayed)
            moved = True
            if not self._live(entry):
                continue
            job = self._registry[entry[-1]]
            heapq.heappush(self._ready, (*job.sort_key(), job.id))
        if moved:
            self._refresh()

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
        if self._delayed and not self._live(self._delayed[0]):
            self._refresh()
        return self._next

    def __len__(self):
        return sum(1 for entry in self._delayed + self._ready if self._live(entry))
