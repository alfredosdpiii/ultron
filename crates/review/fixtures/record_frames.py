"""Records autoreview_api.py's Frames scheduler for crates/review tests: scripted `rlm.map` replies on a virtual
clock, with a fixed random sequence. Each scenario runs its jobs one by one (the fake map never suspends, so the
Python tasks run in order); recorded: every request the fake saw, the results, sleeps, timings, usage, the token
grants left held and the model limits. Run by generate.mjs; prints frames.json.
"""
import asyncio
import json
import sys

sys.path.insert(0, sys.argv[1])
import autoreview_api as aa  # noqa: E402
import infer_api  # noqa: E402


class Results(list):
    pass


class FakeRlm:
    def __init__(self, script, clock):
        self.script = list(script)
        self.clock = clock
        self.requests = []

    async def map(self, tasks, items, *, context=None, contract=None, budget=None, model=None, thinking=None,
                  shared_prefix=False, concurrency=None, timeout_ms=None):
        self.requests.append({"task": tasks[0], "item": items[0], "context": context, "contract": contract,
                              "budget": None if budget is None else budget.tokens, "model": model,
                              "thinking": thinking, "shared_prefix": shared_prefix, "concurrency": concurrency,
                              "timeout_ms": timeout_ms})
        step = self.script.pop(0) if self.script else {"value": "default"}
        self.clock.now += step.get("seconds", 1.0)
        if "raise" in step:
            raise RuntimeError(step["raise"])
        results = Results()
        if "value" in step:
            results.append(step["value"])
        elif "incomplete" in step:
            results.append(infer_api.Incomplete({"reason": step["incomplete"], "detail": "d"}))
        elif "error" in step:
            results.append(infer_api.FrameError({"error": step["error"]}))
        results.spent = {"calls": step.get("calls", 1), "tokens": step.get("tokens", 100)}
        results.usage = step.get("usage", {})
        return results


class Clock:
    def __init__(self):
        self.now = 1000.0
        self.sleeps = []

    def __call__(self):
        return self.now

    async def sleep(self, seconds):
        self.sleeps.append(seconds)
        self.now += seconds


def rng_from(values):
    values = list(values)
    state = {"i": 0}

    def rng():
        value = values[state["i"] % len(values)]
        state["i"] += 1
        return value
    return rng


def result_json(result):
    if isinstance(result, infer_api.Incomplete):
        return {"incomplete": result.status, "detail": result.detail}
    if isinstance(result, infer_api.FrameError):
        return {"error": result.error}
    return {"value": result}


JOB = ("bugs", "find bugs", "the diff")
SCENARIOS = [
    {"name": "values", "options": {}, "runs": [{"phase": "find", "jobs": [JOB, ("ai", "find ai", ["a", "bc"])],
                                               "model": "m/x", "thinking": "low", "context": ["shared view"]}],
     "script": [{"value": [1]}, {"value": {"k": "v"}, "tokens": 250, "usage": {"input_tokens": 5, "output_tokens": "7",
                                                                            "cost": 0.25}}]},
    {"name": "cap", "options": {"cap": 70_000}, "runs": [{"phase": "verify", "jobs": [JOB, ("t", "x" * 30_000, "y" * 3_000),
                                                                                    JOB, JOB], "model": None}],
     "script": [{"value": 1, "tokens": 30_000}, {"value": 2, "tokens": 30_000}, {"value": 3, "tokens": 9_000}]},
    {"name": "retries", "options": {"retry_base_s": 2.0},
     "runs": [{"phase": "find", "jobs": [JOB, JOB, JOB, JOB], "model": "m/x"}],
     "script": [{"error": "HTTP 429 Too Many Requests; retry-after: 3"}, {"value": "ok after throttle"},
                {"error": "status 503"}, {"error": "status 503"}, {"error": "status 503"},
                {"error": "Not logged in"}, {"value": "ok after auth"},
                {"error": "invalid request: bad schema"}]},
    {"name": "no_base", "options": {"retry_base_s": 0.0},
     "runs": [{"phase": "find", "jobs": [JOB], "model": None}],
     "script": [{"error": "401 Unauthorized"}, {"error": "socket hang up"}, {"value": 1}]},
    {"name": "timeouts", "options": {"frame_timeout_s": 50.0},
     "runs": [{"phase": "deep", "jobs": [JOB, JOB], "model": "m/y", "cutoff_in": 500.0},
              {"phase": "deep", "jobs": [JOB], "model": "m/y", "cutoff_in": 30.0}],
     "script": [{"error": "cancelled", "seconds": 50.0}, {"value": "second try"}, {"error": "cancelled", "seconds": 50.0},
                {"error": "cancelled", "seconds": 50.0}, {"error": "cancelled", "seconds": 50.0},
                {"error": "cancelled", "seconds": 29.0}]},
    # A retry that would leave less than MIN_START_S before the cutoff is not taken; a cutoff exactly at the frame's
    # own timeout makes a cancelled frame a deadline.
    {"name": "near_cutoff", "options": {"frame_timeout_s": 50.0},
     "runs": [{"phase": "find", "jobs": [JOB], "model": None, "cutoff_in": 5.0},
              {"phase": "find", "jobs": [JOB], "model": None, "cutoff_in": 50.0}],
     "script": [{"error": "status 503", "seconds": 1.0}, {"error": "cancelled", "seconds": 50.0}]},
    {"name": "cutoff", "options": {},
     "runs": [{"phase": "find", "jobs": [JOB], "model": None, "cutoff_in": 2.5},
              {"phase": "find", "jobs": [JOB, JOB], "model": None, "cutoff_in": 10.0}],
     "script": [{"error": "overloaded_error", "seconds": 1.0}, {"value": "late", "seconds": 1.0}]},
    {"name": "failures", "options": {"cap": 1_000_000},
     "runs": [{"phase": "find", "jobs": [JOB, JOB, JOB, JOB], "model": "m/x", "contracts": [{"type": "array"}, None,
                                                                                            {"type": "object"}, 5]}],
     "script": [{"raise": "bridge down"}, {"incomplete": "budget_exhausted"}, {"error": aa.DEADLINE_ERROR},
                {"error": "x" * 300}]},
    {"name": "limits", "options": {"model_concurrency": {"m/x": 40, "m/y": 0, "m/z": 3}, "concurrency": 99},
     "runs": [{"phase": "find", "jobs": [JOB, JOB, JOB], "model": "m/z"}, {"phase": "find", "jobs": [JOB], "model": None}],
     "script": [{"error": "model_cooldown: reset_seconds=0.5"}, {"value": 1}, {"error": "rate limit"},
                {"error": "rate limit"}, {"error": "rate limit"}, {"error": "429"}, {"value": 2}]},
    {"name": "prefix", "options": {"system_prefix": True},
     "runs": [{"phase": "find", "jobs": [JOB], "model": None, "context": ["v1", "v2"]},
              {"phase": "find", "jobs": [JOB], "model": None, "context": []}],
     "script": [{"value": 1}, {"value": 2}]},
    {"name": "empty", "options": {}, "runs": [{"phase": "find", "jobs": [JOB], "model": None}],
     "script": [{"calls": 0, "tokens": 0}]},
]


async def scenario(spec):
    clock = Clock()
    rlm = FakeRlm(spec["script"], clock)
    options = dict(spec["options"])
    usage = aa._Usage()
    frames = aa.Frames(rlm, cap=options.pop("cap", None), usage=usage, clock=clock, sleep=clock.sleep,
                       rng=rng_from([0.25, 0.75, 0.5]), concurrency=1 if "concurrency" not in options else options.pop("concurrency"),
                       **options)
    runs = []
    for run in spec["runs"]:
        cutoff = clock.now + run["cutoff_in"] if "cutoff_in" in run else None
        results = await frames.run(run["phase"], [tuple(job) for job in run["jobs"]], contract={"type": "string"},
                                   model=run["model"], thinking=run.get("thinking"), context=run.get("context"),
                                   cutoff=cutoff, contracts=run.get("contracts"))
        runs.append([result_json(r) for r in results])
    return {"name": spec["name"], "options": spec["options"], "runs_spec": spec["runs"], "script": spec["script"],
            "requests": rlm.requests, "results": runs, "sleeps": clock.sleeps, "end": clock.now,
            "timings": frames.timings, "usage": {"frames": usage.frames, "tokens": usage.tokens, "input": usage.input,
                                                 "output": usage.output, "cost": usage.cost, "by_phase": usage.by_phase},
            "held": frames.held, "model_limits": frames.model_limits, "throttled": frames.throttled,
            "concurrency": frames.concurrency}


out = [asyncio.run(scenario(spec)) for spec in SCENARIOS]
print(json.dumps({"start": 1000.0, "rng": [0.25, 0.75, 0.5], "scenarios": out}, ensure_ascii=False, indent=1))
