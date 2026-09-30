"""Hidden check for twelve-tickets: every ticket's hidden acceptance tests must pass on the working tree (committed
or not). Prints one line per ticket, then a JSON summary line ({"passed": k, "total": 12, "tickets": {...}}); exits
0 only when all twelve pass. Each ticket runs in its own interpreter, so one ticket's failure (even an import
error) is reported without hiding the others' results."""
import json
import os
import shutil
import subprocess
import sys

TICKETS = [f"T{number:02d}" for number in range(1, 13)]

# Stale bytecode must not hide a change.
for base, dirs, _ in os.walk("."):
    if "__pycache__" in dirs:
        shutil.rmtree(os.path.join(base, "__pycache__"), ignore_errors=True)

results = {}
for ticket in TICKETS:
    try:
        run = subprocess.run(
            [sys.executable, "-B", "-m", "unittest", f"hidden_tests.test_tickets.{ticket}"],
            capture_output=True,
            text=True,
            timeout=60,
        )
        ok, output = run.returncode == 0, run.stderr
    except subprocess.TimeoutExpired:
        ok, output = False, "timed out after 60 s"
    except OSError as error:
        ok, output = False, str(error)
    results[ticket] = ok
    lines = [line for line in output.strip().splitlines() if line.strip()]
    detail = "" if ok else " | ".join(line.strip() for line in lines if "Error" in line or line.startswith(("FAIL", "ERROR")))[:160]
    print(f"{ticket}: {'pass' if ok else 'FAIL'}{(' ' + detail) if detail else ''}")

passed = sum(results.values())
print(json.dumps({"passed": passed, "total": len(TICKETS), "tickets": results}, separators=(",", ":")))
sys.exit(0 if passed == len(TICKETS) else 1)
