"""Job records and their lifecycle states."""
from dataclasses import dataclass, field

WAITING = "waiting"
QUEUED = "queued"
RUNNING = "running"
SUCCEEDED = "succeeded"
FAILED = "failed"
CANCELLED = "cancelled"

FINAL_STATES = frozenset({SUCCEEDED, FAILED, CANCELLED})


def check_int(name, value, minimum=None):
    """`value` as an int (bools are not ints here), at least `minimum` when one is given."""
    if isinstance(value, bool) or not isinstance(value, int):
        raise ValueError(f"{name} must be an integer, got {value!r}")
    if minimum is not None and value < minimum:
        raise ValueError(f"{name} must be at least {minimum}, got {value}")
    return value


@dataclass
class Job:
    """One submission of a job id. A resubmitted id gets a new record with a new sequence number."""

    id: str
    seq: int
    priority: int
    run_at: int
    duration: int
    max_retries: int
    deps: tuple
    state: str = WAITING
    attempts: int = 0
    eligible_at: int = 0
    unmet: int = 0
    dependents: list = field(default_factory=list)

    @property
    def finished(self):
        return self.state in FINAL_STATES

    def sort_key(self):
        """Dispatch order: higher priority first, then earlier eligibility, then earlier submission."""
        return (-self.priority, self.eligible_at, self.seq)

    def can_retry(self):
        """Whether the attempt that just failed may be followed by another one."""
        return self.attempts <= self.max_retries


def make_job(job_id, seq, now, *, priority, run_at, duration, max_retries, deps):
    """Validate a submission and build its record (state and eligibility are settled by the scheduler)."""
    if not isinstance(job_id, str) or not job_id:
        raise ValueError(f"job id must be a non-empty string, got {job_id!r}")
    check_int("priority", priority)
    if run_at is None:
        run_at = now
    check_int("run_at", run_at, 0)
    check_int("duration", duration, 1)
    check_int("max_retries", max_retries, 0)
    if isinstance(deps, str):
        raise ValueError("deps must be a collection of job ids, not a string")
    unique = []
    for dep in deps:
        if not isinstance(dep, str) or not dep:
            raise ValueError(f"dependency ids must be non-empty strings, got {dep!r}")
        if dep == job_id:
            raise ValueError(f"job {job_id!r} cannot depend on itself")
        if dep not in unique:
            unique.append(dep)
    return Job(
        id=job_id,
        seq=seq,
        priority=priority,
        run_at=run_at,
        duration=duration,
        max_retries=max_retries,
        deps=tuple(unique),
        eligible_at=max(run_at, now),
    )
