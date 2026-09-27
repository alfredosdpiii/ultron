"""Dependencies between job records."""
from .job import CANCELLED, FAILED, SUCCEEDED, WAITING


class DependencyGraph:
    def __init__(self, registry):
        self._registry = registry

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
                record.dependents.append(job)
                job.unmet += 1
        return True

    def released_by(self, job):
        """Dependents of `job` (which just succeeded) whose dependencies have now all succeeded."""
        ready = []
        for dependent in job.dependents:
            if dependent.state != WAITING:
                continue
            dependent.unmet -= 1
            if dependent.unmet == 0:
                ready.append(dependent)
        return ready

    def doomed_by(self, job):
        """Every record waiting, directly or transitively, on `job` (which failed or was cancelled), by submission."""
        doomed = {}
        stack = list(job.dependents)
        while stack:
            dependent = stack.pop()
            if dependent.state != WAITING or dependent.seq in doomed:
                continue
            doomed[dependent.seq] = dependent
            stack.extend(dependent.dependents)
        return [doomed[seq] for seq in sorted(doomed)]
