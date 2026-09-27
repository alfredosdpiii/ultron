"""Dependencies between job records.

Waiting records are indexed by the id of each dependency they wait on; the index entry of an id holds the records
waiting on the job currently submitted under that id.
"""
from .job import CANCELLED, FAILED, SUCCEEDED, WAITING


class DependencyGraph:
    def __init__(self, registry):
        self._registry = registry
        self._waiting_on = {}

    def attach(self, job):
        """Register `job` with its dependencies. Returns False if one of them already failed or was cancelled."""
        records = []
        for dep in job.deps:
            record = self._registry.get(dep)
            if record is None:
                raise ValueError(f"job {job.id!r} depends on unknown job {dep!r}")
            records.append(record)
        if any(record.state in (FAILED, CANCELLED) for record in records):
            return False
        job.unmet = 0
        for record in records:
            if record.state != SUCCEEDED:
                self._waiting_on.setdefault(record.id, []).append(job)
                job.unmet += 1
        return True

    def waiting_on(self, job_id):
        """Records currently waiting on the job submitted as `job_id`."""
        return [record for record in self._waiting_on.get(job_id, ()) if record.state == WAITING]

    def released_by(self, job):
        """Dependents of `job` (which just succeeded) whose dependencies have now all succeeded."""
        ready = []
        for dependent in self._waiting_on.get(job.id, ()):
            if dependent.state != WAITING:
                continue
            dependent.unmet -= 1
            if dependent.unmet == 0:
                ready.append(dependent)
        return ready

    def doomed_by(self, job):
        """Every record waiting, directly or transitively, on `job` (which failed or was cancelled), by submission."""
        doomed = {}
        stack = self.waiting_on(job.id)
        while stack:
            dependent = stack.pop()
            if dependent.seq in doomed:
                continue
            doomed[dependent.seq] = dependent
            stack.extend(self.waiting_on(dependent.id))
        return [doomed[seq] for seq in sorted(doomed)]
