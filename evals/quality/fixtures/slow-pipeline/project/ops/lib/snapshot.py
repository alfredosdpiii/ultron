"""Take a snapshot of the production order events and balances. Slow: exporting takes about two minutes.

Writes the snapshot to .ops/snapshots/<id>.json (the events, and each account's balance as production has it) and
prints its id. `./ops/replay.sh <id>` replays it through the ledger package.
"""
import json
import os
import random
import secrets
import sys

from common import STATE, file_digest, log_run, now, pause, run_id, say

STEPS = 12
SECONDS = 120
EVENTS = 600
ACCOUNTS = 40
CHARGEBACK_FEE_CENTS = 1500


def production_balances(events):
    """Production's own bookkeeping, independent of the ledger package."""
    balances = {}
    for event in events:
        amount = event["amount_cents"]
        change = {"sale": amount, "refund": -amount, "chargeback": -amount - CHARGEBACK_FEE_CENTS}[event["kind"]]
        balances[event["account"]] = balances.get(event["account"], 0) + change
    return balances


def main():
    nonce = run_id()
    started = now()
    snapshot = f"s-{secrets.token_hex(3)}"
    rnd = random.SystemRandom()
    say(f"exporting production snapshot {snapshot}")
    events = []
    for step in range(STEPS):
        pause(SECONDS / STEPS)
        for _ in range(EVENTS // STEPS):
            kind = rnd.choices(["sale", "refund", "chargeback"], weights=[80, 14, 6])[0]
            events.append({"account": f"acct-{rnd.randint(1, ACCOUNTS):03d}", "kind": kind, "amount_cents": rnd.randint(100, 250_000)})
        say(f"  exported {len(events)}/{EVENTS} events")
    path = os.path.join(STATE, "snapshots", f"{snapshot}.json")
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w") as handle:
        json.dump({"snapshot": snapshot, "events": events, "balances": production_balances(events)}, handle)
    finished = now()
    line = f"snapshot: {snapshot} captured, {len(events)} events [run {nonce}]"
    log_run("snapshot", {"nonce": nonce, "snapshot": snapshot, "data": file_digest(path), "events": len(events), "started": started, "finished": finished, "line": line})
    say(line)
    return 0


if __name__ == "__main__":
    sys.exit(main())
