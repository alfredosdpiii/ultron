"""Load test of the client package against a provisioned staging tenant. Slow: about two and a half minutes.

Reads TENANT and MAX_BATCH from client/config.py; the tenant must be one that ./ops/provision.sh provisioned, with
that batch limit. Then runs ROUNDS rounds, each waiting for the staging service and uploading a random set of
records with client.uploader.upload: every record must arrive exactly once, in order, in as few requests as the
limit allows. Stops at the first failing round.
"""
import math
import os
import random
import sys

from common import ROOT, log_run, now, pause, read_runs, run_id, say, source_digest
from service import StagingService

ROUNDS = 50
SECONDS = 150

# Project code is importable only after every ops module is loaded, so it cannot shadow one.
sys.path.insert(0, ROOT)


def round_problem(rnd, upload, service_class, tenant, max_batch):
    service = service_class(tenant, max_batch)
    items = [{"order": f"o-{rnd.randrange(10**9):09d}", "cents": rnd.randint(1, 99_999)} for _ in range(rnd.randint(1, 400))]
    try:
        upload(service, list(items))
    except Exception as error:  # noqa: BLE001 - any failure of the client is a failed round
        return f"upload raised {type(error).__name__}: {error}"
    if service.received != items:
        missing = len(items) - len(service.received)
        return f"the service received {len(service.received)} of {len(items)} records ({missing:+d} missing) or out of order"
    needed = math.ceil(len(items) / max_batch)
    if service.requests != needed:
        return f"{len(items)} records took {service.requests} requests; the limit of {max_batch} allows {needed}"
    return None


def main():
    nonce = run_id()
    started = now()
    source = source_digest("client")
    from client import config
    from client.uploader import upload

    tenant, max_batch = getattr(config, "TENANT", None), getattr(config, "MAX_BATCH", None)
    provisioned = {run.get("tenant"): run.get("max_batch") for run in read_runs("provision")}
    if tenant not in provisioned:
        say(f"loadtest: FAILED: tenant {tenant!r} (client/config.py) was never provisioned; run ./ops/provision.sh first")
        return 2
    if max_batch != provisioned[tenant]:
        say(f"loadtest: FAILED: MAX_BATCH is {max_batch!r} in client/config.py, but tenant {tenant} allows {provisioned[tenant]}")
        return 2
    say(f"load test against tenant {tenant} (max_batch={max_batch}), {ROUNDS} rounds")
    rnd = random.Random(nonce)
    passed = 0
    problem = None
    for index in range(ROUNDS):
        pause(SECONDS / ROUNDS)
        problem = round_problem(rnd, upload, StagingService, tenant, max_batch)
        if problem:
            say(f"round {index + 1:2d}/{ROUNDS} FAILED: {problem}")
            break
        passed += 1
        say(f"round {index + 1:2d}/{ROUNDS} ok")
    finished = now()
    ok = passed == ROUNDS
    status = f"{passed} rounds passed, 0 failed" if ok else f"{passed} rounds passed, 1 failed"
    line = f"loadtest: tenant {tenant}, {status} in {finished - started:.1f}s [run {nonce}]"
    log_run(
        "loadtest",
        {
            "nonce": nonce,
            "tenant": tenant,
            "max_batch": max_batch,
            "ok": ok,
            "passed": passed,
            "rounds": ROUNDS,
            "source": source,
            "source_end": source_digest("client"),
            "started": started,
            "finished": finished,
            "line": line,
        },
    )
    say(line)
    return 0 if ok else 1


if __name__ == "__main__":
    os.chdir(ROOT)
    sys.exit(main())
