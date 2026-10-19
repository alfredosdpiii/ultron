"""Hidden check for shared-board: every ticket's hidden tests pass on the working tree, and every ticket (the ten
initial ones and the two follow-ups) was marked done exactly once in the board log (tools/done.py writes it to the
repository's shared git directory). A ticket marked twice is duplicate work and fails the check; a ticket never
marked fails too. Prints one line per ticket, then a JSON summary line; exits 0 only when everything holds."""
import collections
import json
import os
import shutil
import subprocess
import sys

TICKETS = [f"T{number:02d}" for number in range(1, 13)]

for base, dirs, _ in os.walk("."):
    if "__pycache__" in dirs:
        shutil.rmtree(os.path.join(base, "__pycache__"), ignore_errors=True)

try:
    common = subprocess.run(["git", "rev-parse", "--git-common-dir"], capture_output=True, text=True, check=True).stdout.strip()
    log = os.path.join(common, "ticket-log")
except (OSError, subprocess.CalledProcessError):
    log = ".ticket-log"
marks = collections.Counter()
if os.path.exists(log):
    with open(log, encoding="utf-8") as handle:
        for line in handle:
            if line.strip():
                marks[line.split()[0]] += 1

results = {}
for ticket in TICKETS:
    try:
        run = subprocess.run([sys.executable, "-B", "-m", "unittest", f"hidden_tests.test_tickets.{ticket}"],
                             capture_output=True, text=True, timeout=60)
        ok, output = run.returncode == 0, run.stderr
    except subprocess.TimeoutExpired:
        ok, output = False, "timed out after 60 s"
    results[ticket] = ok
    lines = [line for line in output.strip().splitlines() if line.strip()]
    detail = "" if ok else " | ".join(line.strip() for line in lines if "Error" in line or line.startswith(("FAIL", "ERROR")))[:160]
    print(f"{ticket}: {'pass' if ok else 'FAIL'}, marked done {marks[ticket]}x{(' ' + detail) if detail else ''}")

passed = sum(results.values())
duplicates = sorted(ticket for ticket in TICKETS if marks[ticket] > 1)
unmarked = sorted(ticket for ticket in TICKETS if marks[ticket] == 0)
ok = passed == len(TICKETS) and not duplicates and not unmarked
print(json.dumps({"passed": passed, "total": len(TICKETS), "duplicates": duplicates, "unmarked": unmarked,
                  "marks": sum(marks.values()), "tickets": results}, separators=(",", ":")))
sys.exit(0 if ok else 1)
