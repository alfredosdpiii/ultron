"""Replay a production snapshot through the ledger package. Slow: about 45 s. Usage: ./ops/replay.sh <snapshot-id>

Feeds the snapshot's events to ledger.ledger.apply in chunks (each chunk waits for the replay store), then compares
every account's closing balance with the balance production recorded in the snapshot.
"""
import json
import os
import sys

from common import ROOT, STATE, file_digest, log_run, now, pause, read_runs, run_id, say, source_digest

CHUNKS = 15
SECONDS = 45

# Project code is importable only after every ops module is loaded, so it cannot shadow one.
sys.path.insert(0, ROOT)


def main(argv):
    if len(argv) != 1:
        say("usage: ./ops/replay.sh <snapshot-id>   (take a snapshot with ./ops/snapshot.sh)")
        return 2
    snapshot = argv[0]
    taken = {run.get("snapshot"): run for run in read_runs("snapshot")}
    path = os.path.join(STATE, "snapshots", f"{snapshot}.json")
    if snapshot not in taken or not os.path.exists(path):
        say(f"replay: FAILED: no snapshot {snapshot!r} was taken; run ./ops/snapshot.sh and use the id it prints")
        return 2
    data = file_digest(path)
    if data != taken[snapshot].get("data"):
        say(f"replay: FAILED: snapshot {snapshot} was modified after it was taken")
        return 2
    nonce = run_id()
    started = now()
    source = source_digest("ledger")
    from ledger.ledger import apply

    with open(path) as handle:
        recorded = json.load(handle)
    events = recorded["events"]
    say(f"replaying snapshot {snapshot}: {len(events)} events in {CHUNKS} chunks")
    balances = {}
    error = None
    size = -(-len(events) // CHUNKS)
    for index in range(CHUNKS):
        pause(SECONDS / CHUNKS)
        if error is None:
            try:
                for event in events[index * size : (index + 1) * size]:
                    apply(balances, dict(event))
            except Exception as problem:  # noqa: BLE001 - any failure of the ledger fails the replay
                error = f"apply raised {type(problem).__name__}: {problem}"
        say(f"  chunk {index + 1:2d}/{CHUNKS} {'ok' if error is None else 'FAILED'}")
    expected = recorded["balances"]
    accounts = sorted(set(expected) | set(balances))
    mismatched = [account for account in accounts if balances.get(account) != expected.get(account)]
    finished = now()
    ok = error is None and not mismatched
    if error:
        say(f"replay error: {error}")
    for account in mismatched[:5]:
        say(f"  {account}: ledger {balances.get(account)} != production {expected.get(account)}")
    line = f"replay: snapshot {snapshot}, {len(events)} events, {len(accounts)} accounts, {len(mismatched)} mismatched in {finished - started:.1f}s [run {nonce}]"
    log_run(
        "replay",
        {
            "nonce": nonce,
            "snapshot": snapshot,
            "data": data,
            "ok": ok,
            "mismatched": len(mismatched),
            "error": error,
            "source": source,
            "source_end": source_digest("ledger"),
            "started": started,
            "finished": finished,
            "line": line,
        },
    )
    say(line)
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
