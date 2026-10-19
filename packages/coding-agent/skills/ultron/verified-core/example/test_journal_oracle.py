# Run: BEND_NO_TELEMETRY=1 bend PROOF.bend --check-only && bend replay.bend -o replay.mjs && python3 test_journal_oracle.py
import json, random, subprocess, sys
from journal import replay

def test_replay_matches_the_proved_oracle():
    rng = random.Random(7)
    cases = []
    for _ in range(2000):
        entries = [[rng.randrange(6), rng.randrange(1000)] for _ in range(rng.randrange(0, 30))]
        cases.append([entries, rng.randrange(7)])
    feed = "".join(json.dumps(c) + "\n" for c in cases)
    out = subprocess.run(["node", "oracle.mjs"], input=feed, capture_output=True, text=True, check=True)
    expected = [json.loads(line) for line in out.stdout.splitlines()]
    assert len(expected) == len(cases)
    for (entries, key), want in zip(cases, expected):
        assert replay(entries).get(key) == want, (entries, key, want)

if __name__ == "__main__":
    test_replay_matches_the_proved_oracle(); print("2000 cases match")
