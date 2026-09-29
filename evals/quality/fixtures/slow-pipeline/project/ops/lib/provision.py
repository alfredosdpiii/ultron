"""Provision a fresh staging tenant. Slow: the (simulated) staging cluster takes about 35 s to allocate one.

Prints the new tenant's id and its batch limit (the most records one upload request may carry). Every tenant is
new, with its own id and limit; the load test runs against a tenant provisioned here.
"""
import random
import secrets
import sys

from common import log_run, now, pause, run_id, say

STEPS = 7
SECONDS = 35


def main():
    nonce = run_id()
    started = now()
    say("provisioning a staging tenant")
    for step in range(STEPS):
        pause(SECONDS / STEPS)
        say(f"  allocating: step {step + 1}/{STEPS}")
    tenant = f"t-{secrets.token_hex(3)}"
    max_batch = random.SystemRandom().randint(17, 61)
    finished = now()
    line = f"provision: tenant {tenant} ready, max_batch={max_batch} [run {nonce}]"
    log_run("provision", {"nonce": nonce, "tenant": tenant, "max_batch": max_batch, "started": started, "finished": finished, "line": line})
    say(line)
    return 0


if __name__ == "__main__":
    sys.exit(main())
