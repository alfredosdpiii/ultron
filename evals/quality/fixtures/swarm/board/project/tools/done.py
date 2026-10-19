"""Mark a ticket done: `python3 tools/done.py T03`. Run it exactly once per ticket, when the ticket is finished.

The board's log lives in the repository's shared git directory, so every checkout and worktree of this repository
writes to the same log. Marking some tickets done opens follow-up tickets: they are printed and written to tickets/.
"""
import os
import subprocess
import sys
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
INITIAL = [f"T{number:02d}" for number in range(1, 11)]

FOLLOW_UPS = {
    "T03": (
        "T11",
        """# T11: Weighted splits (follow-up of T03)

**Area:** `ledger/money.py`, a new function `split_weighted(cents, weights)`

## Acceptance criteria

- Shares proportional to `weights` (non-negative integers) that add up to `cents` exactly: each share is first rounded
  down, then the remaining cents go one at a time to the shares with the largest fractional parts, ties to the
  earlier share. `split_weighted(1000, [1, 2]) == [333, 667]`, `split_weighted(100, [1, 1, 1]) == [34, 33, 33]`,
  `split_weighted(10, [0, 1]) == [0, 10]`.
- An empty list, a negative weight or weights that sum to zero raise `ValueError`.
""",
    ),
    "T07": (
        "T12",
        """# T12: Normalize typed account ids (follow-up of T07)

**Area:** `ledger/accounts.py`, a new function `normalize_account_id(text)`

## Acceptance criteria

- Returns the canonical id `AC-` + seven digits for what people type: any case, surrounding spaces, and spaces or a
  missing dash between `AC` and the digits: `normalize_account_id(" ac 1234561 ") == "AC-1234561"`,
  `normalize_account_id("AC1234561") == "AC-1234561"`.
- Returns `None` when the result is not a valid id by `account_id_valid` (T07), e.g. a wrong check digit.
""",
    ),
}
PARENT = {child: parent for parent, (child, _) in FOLLOW_UPS.items()}


def log_path():
    try:
        common = subprocess.run(
            ["git", "rev-parse", "--git-common-dir"], cwd=ROOT, capture_output=True, text=True, check=True
        ).stdout.strip()
        return os.path.join(ROOT, common, "ticket-log")
    except (OSError, subprocess.CalledProcessError):
        return os.path.join(ROOT, ".ticket-log")


def done_so_far(path):
    if not os.path.exists(path):
        return []
    with open(path, encoding="utf-8") as handle:
        return [line.split()[0] for line in handle if line.strip()]


def main(argv):
    if len(argv) != 2:
        print("usage: python3 tools/done.py T03", file=sys.stderr)
        return 2
    ticket = argv[1].strip().upper()
    path = log_path()
    done = done_so_far(path)
    if ticket not in INITIAL and ticket not in PARENT:
        print(f"unknown ticket {ticket}", file=sys.stderr)
        return 2
    if ticket in PARENT and PARENT[ticket] not in done:
        print(f"{ticket} is not open yet (it follows {PARENT[ticket]})", file=sys.stderr)
        return 2
    with open(path, "a", encoding="utf-8") as handle:
        handle.write(f"{ticket} {time.time():.3f}\n")
    if ticket in done:
        print(f"{ticket} was already marked done: this is duplicate work")
    else:
        print(f"{ticket} marked done")
    if ticket in FOLLOW_UPS:
        child, text = FOLLOW_UPS[ticket]
        target = os.path.join(ROOT, "tickets", f"{child}.md")
        with open(target, "w", encoding="utf-8") as handle:
            handle.write(text)
        print(f"Follow-up ticket opened: tickets/{child}.md")
        print(text)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
