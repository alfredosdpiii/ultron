"""Acceptance check of one module: `python3 check.py durations|ranges|semver`. Prints one JSON object."""
import json
import sys
import unittest

MODULES = ["durations", "ranges", "semver"]

if len(sys.argv) != 2 or sys.argv[1] not in MODULES:
    print(f"usage: python3 check.py {'|'.join(MODULES)}", file=sys.stderr)
    sys.exit(2)
module = sys.argv[1]
suite = unittest.defaultTestLoader.loadTestsFromName(f"tests.full.test_{module}")
result = unittest.TestResult()
suite.run(result)
failures = [f"{test.id().split('.')[-1]}: {trace.strip().splitlines()[-1]}" for test, trace in result.failures + result.errors]
print(json.dumps({"module": module, "passed": result.wasSuccessful(), "failures": failures}))
sys.exit(0 if result.wasSuccessful() else 1)
