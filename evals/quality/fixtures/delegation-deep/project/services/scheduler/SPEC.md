# scheduler: deterministic job scheduler

A discrete-event scheduler over integer ticks (package `jobqueue`). Nothing reads a real clock: time is the
scheduler's `now`, which starts at 0 and only moves inside `run`.

## Scheduler(slots, outcome, backoff_base=1, backoff_cap=16)

- `slots` (int >= 1) worker slots; each running attempt occupies one slot.
- `outcome(job_id, attempt)` is called once when an attempt ends and returns `True` (success) or `False`
  (failure). `attempt` counts from 0.
- `backoff_base` and `backoff_cap` are ints >= 1. Invalid arguments raise `ValueError`.

## Jobs

`submit(job_id, *, priority=0, run_at=None, duration=1, max_retries=0, deps=())` submits a job at the current tick
and returns its state.

- `job_id` is a non-empty string. An id may be submitted again only once its previous job is finished (succeeded,
  failed or cancelled); the new submission replaces it (new record, new submission order). Submitting an id whose
  job is not finished raises `ValueError`.
- `priority` is any int (higher runs first). `run_at` (int >= 0, default `now`) is the earliest tick the job may
  start. `duration` (int >= 1) is how many ticks an attempt occupies its slot. `max_retries` (int >= 0) is how many
  times a failed attempt is retried.
- `deps` are ids of already submitted jobs (unknown ids raise `ValueError`; duplicates count once). The job waits
  until every dependency has succeeded. If a dependency is failed or cancelled at submission, the job is cancelled
  immediately.
- States: `waiting` (dependencies not all succeeded), `queued` (waiting for its eligible tick or a free slot),
  `running`, `succeeded`, `failed` (its last allowed attempt failed), `cancelled`.

Every queued job has an **eligible tick**: for a job queued at submission, `max(run_at, now)`; for a job whose last
dependency succeeds at tick `t`, `max(run_at, t)`; for a retry, the tick given by the backoff below.

## run(until=None)

Processes the current tick, then every later tick at which something happens, up to `until` inclusive
(`until < now` raises `ValueError`). Afterwards `now` is `until`; with `until=None` it runs until nothing is
running or queued and `now` is the last tick processed. A tick may be processed again by a later `run` (after new
submissions); attempts that already ended are not ended twice. Returns `now`.

Processing tick `t`:

1. **Completions.** Every running attempt with `start + duration == t` ends, in the order the attempts started
   (an attempt that started earlier, or earlier within the same tick, ends first). Its slot is free again, and for
   each ending attempt, in that order:
   - success: the job is `succeeded`; every waiting job whose dependencies have now all succeeded becomes queued
     (eligible tick `max(run_at, t)`).
   - failure with retries left (attempt `a < max_retries`): the job is queued again with eligible tick
     `t + min(backoff_base * 2**a, backoff_cap)`.
   - failure of the last allowed attempt: the job is `failed`.
2. **Dispatch.** While a slot is free and some queued job has eligible tick `<= t`, start the best one: highest
   `priority`, then earliest eligible tick, then earliest submission. A slot freed in step 1 is used in the same
   tick. Retries keep the job's submission order.

## Cancellation

- `cancel(job_id)` cancels a `waiting` or `queued` job and returns `True`; for a running, finished or unknown job it
  returns `False` and changes nothing.
- When a job is cancelled or fails (its last attempt), every job waiting on it, directly or through other waiting
  jobs, is cancelled at the same tick, in submission order.

## Events and stats

`events` is the list of `(tick, kind, job_id)` in the order things happened. Kinds: `"start"` (an attempt started),
`"done"` (an attempt succeeded), `"fail"` (an attempt failed, retried or not), `"cancel"` (a job was cancelled;
the cancelled job first, then the jobs cancelled with it). Within a tick: each completion's `done`/`fail` event
followed by the `cancel` events it causes, then the `start` events in dispatch order.

`stats()` returns a dict: `submitted`, `starts` (attempts started), `succeeded`, `failed` (jobs), `cancelled`
(jobs), `retries` (failed attempts that were retried), `busy_ticks` (sum of the durations of attempts that ended).

Also: `now`, `state(job_id)`, `attempts(job_id)` (attempts started; unknown ids raise `KeyError`), and `jobs()`
(`{job_id: state}` for the current record of every id).

## Example

```python
s = Scheduler(1, lambda job_id, attempt: attempt > 0, backoff_base=2, backoff_cap=16)
s.submit("a", max_retries=1)   # "queued"
s.submit("b", deps=["a"])      # "waiting"
s.run()                        # 5
s.events == [(0, "start", "a"), (1, "fail", "a"), (3, "start", "a"), (4, "done", "a"),
             (4, "start", "b"), (5, "fail", "b")]
s.state("b") == "failed"
```
