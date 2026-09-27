"""Worker slots and the attempts running on them."""
import heapq


class WorkerPool:
    def __init__(self, slots):
        self.slots = slots
        self._running = []
        self._busy = {}
        self._starts = 0

    @property
    def free(self):
        return self.slots - len(self._busy)

    @property
    def starts(self):
        return self._starts

    def running(self):
        return sorted(self._busy)

    def start(self, job, now):
        """Occupy a slot with `job` from `now` for its duration; returns the tick it ends."""
        if self.free <= 0:
            raise RuntimeError("no free worker slot")
        end = now + job.duration
        self._starts += 1
        heapq.heappush(self._running, (end, job.seq, job.id))
        self._busy[job.id] = job
        return end

    def due(self, now):
        """Attempts ending at or before `now`, in the order they complete; their slots are free again."""
        done = []
        while self._running and self._running[0][0] <= now:
            _, _, job_id = heapq.heappop(self._running)
            done.append(self._busy.pop(job_id))
        return done

    def next_end(self):
        return self._running[0][0] if self._running else None

    def __len__(self):
        return len(self._busy)
