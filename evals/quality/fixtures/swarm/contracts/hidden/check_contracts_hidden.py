"""Hidden check for contracts: each module's full acceptance suite (a fresh copy, so edits to the visible one do not
count) plus extra cases. Prints one line per module, then {"passed": k, "total": 3, "modules": {...}}."""
import json
import os
import shutil
import subprocess
import sys

MODULES = ["durations", "ranges", "semver"]
EXTRA = {
    "durations": "from lib.durations import parse_duration as p\nassert p('2d') == 172800\nassert p('1m 1s') == 61\n"
                 "for bad in ['1s1m', '1d2d', 'x', '2d 2d']:\n    try:\n        p(bad)\n    except ValueError:\n        pass\n"
                 "    else:\n        raise AssertionError(bad)\n",
    "ranges": "from lib.ranges import merge_ranges as m\nassert m([(3, 4), (1, 1)]) == [(1, 1), (3, 4)]\n"
              "assert m([(0, 0), (1, 1), (2, 2)]) == [(0, 2)]\nassert m([(-5, -1), (-3, 2)]) == [(-5, 2)]\n",
    "semver": "from lib.semver import compare as c\nassert c('0.0.1-2', '0.0.1-10') == -1\nassert c('1.0.0-a', '1.0.0-1') == 1\n"
              "assert c('2.0.0', '2.0.0') == 0\nassert c('1.0.0-alpha.1', '1.0.0-alpha') == 1\n",
}

for base, dirs, _ in os.walk("."):
    if "__pycache__" in dirs:
        shutil.rmtree(os.path.join(base, "__pycache__"), ignore_errors=True)
results = {}
for module in MODULES:
    ok, detail = True, ""
    for args in ([sys.executable, "-B", "-m", "unittest", f"hidden_full.test_{module}"], [sys.executable, "-B", "-c", EXTRA[module]]):
        try:
            run = subprocess.run(args, capture_output=True, text=True, timeout=60)
        except subprocess.TimeoutExpired:
            ok, detail = False, "timed out"
            break
        if run.returncode != 0:
            ok = False
            detail = " | ".join(line.strip() for line in run.stderr.splitlines() if "Error" in line or line.startswith(("FAIL", "ERROR")))[:200]
            break
    results[module] = ok
    print(f"{module}: {'pass' if ok else 'FAIL'}{(' ' + detail) if detail else ''}")
passed = sum(results.values())
print(json.dumps({"passed": passed, "total": len(MODULES), "modules": results}, separators=(",", ":")))
sys.exit(0 if passed == len(MODULES) else 1)
