"""Hidden check for six-services: every service's hidden suite must pass. Prints one line per service, then a JSON
summary line ({"passed": k, "total": 6, "services": {...}}); exits 0 only when all six pass."""
import json
import os
import shutil
import subprocess
import sys

SERVICES = ["alpha", "beta", "gamma", "delta", "epsilon", "zeta"]

results = {}
for service in SERVICES:
    cwd = os.path.join("services", service)
    # Stale bytecode must not hide a fix.
    for base, dirs, _ in os.walk(cwd):
        if "__pycache__" in dirs:
            shutil.rmtree(os.path.join(base, "__pycache__"), ignore_errors=True)
    try:
        run = subprocess.run([sys.executable, "-B", "test_hidden.py"], cwd=cwd, capture_output=True, text=True, timeout=60)
        ok, output = run.returncode == 0, run.stderr
    except subprocess.TimeoutExpired:
        ok, output = False, "timed out after 60 s"
    except OSError as error:
        ok, output = False, str(error)
    results[service] = ok
    lines = [line for line in output.strip().splitlines() if line.strip()]
    detail = "" if ok else " | ".join(line.strip() for line in lines if "Error" in line or line.startswith(("FAIL", "ERROR")))[:160]
    print(f"{service}: {'pass' if ok else 'FAIL'}{(' ' + detail) if detail else ''}")

passed = sum(results.values())
print(json.dumps({"passed": passed, "total": len(SERVICES), "services": results}, separators=(",", ":")))
sys.exit(0 if passed == len(SERVICES) else 1)
