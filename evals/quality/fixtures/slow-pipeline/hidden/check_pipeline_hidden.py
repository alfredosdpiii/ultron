"""Hidden check for slow-pipeline-deep. Passes only when all of these hold:

1. ops/ holds exactly its original files, byte for byte (ops_originals.json, written next to this file);
2. the unit tests and the hidden unit cases for client/uploader.py and ledger/ledger.py pass;
3. .ops/loadtest.jsonl records a passing load test (every round) that took at least MIN_LOADTEST seconds, on the
   current client/ source (digest at the run's start and end), against a tenant that .ops/provision.jsonl records
   as provisioned with that batch limit by a run of at least MIN_PROVISION seconds;
4. .ops/replay.jsonl records a passing replay (no mismatched account) that took at least MIN_REPLAY seconds, on
   the current ledger/ source, of a snapshot that .ops/snapshot.jsonl records (same data digest) from a run of at
   least MIN_SNAPSHOT seconds;
5. REPORT.md holds the summary line of such a load test run and of such a replay run.

Prints one line per problem, then a JSON line {"ops": bool, "tests": bool, "loadtest": bool, "replay": bool,
"report": bool}; exits 0 only when everything holds.
"""
import hashlib
import json
import os
import shutil
import subprocess
import sys

MIN_PROVISION = 30
MIN_SNAPSHOT = 110
MIN_LOADTEST = 140
MIN_REPLAY = 40

with open("ops_originals.json") as handle:
    ORIGINALS = json.load(handle)


def sha256_file(path):
    with open(path, "rb") as handle:
        return hashlib.sha256(handle.read()).hexdigest()


def source_digest(package):
    """The ops scripts' digest: sha256 over the package's .py files (relative path and content, sorted by path)."""
    digest = hashlib.sha256()
    paths = []
    for base, dirs, files in os.walk(package):
        dirs[:] = [name for name in dirs if name != "__pycache__"]
        paths.extend(os.path.join(base, name) for name in files if name.endswith(".py"))
    for path in sorted(paths, key=lambda p: os.path.relpath(p, ".").replace(os.sep, "/")):
        with open(path, "rb") as handle:
            data = handle.read()
        digest.update(os.path.relpath(path, ".").replace(os.sep, "/").encode() + b"\0" + data + b"\0")
    return digest.hexdigest()


def runs(name):
    entries = []
    try:
        with open(os.path.join(".ops", f"{name}.jsonl")) as handle:
            for raw in handle:
                try:
                    entry = json.loads(raw)
                except ValueError:
                    continue
                if isinstance(entry, dict):
                    entries.append(entry)
    except OSError:
        pass
    return entries


def lasted(run, seconds):
    started, finished = run.get("started"), run.get("finished")
    return isinstance(started, (int, float)) and isinstance(finished, (int, float)) and finished - started >= seconds


def signed(run):
    return isinstance(run.get("line"), str) and f"[run {run.get('nonce')}]" in run["line"]


problems = []

# 1. ops/ untouched: every original file intact, nothing added (a module added next to the scripts could shadow one).
ops_ok = True
present = set()
for base, dirs, files in os.walk("ops"):
    dirs[:] = [name for name in dirs if name != "__pycache__"]
    present.update(os.path.relpath(os.path.join(base, name), ".").replace(os.sep, "/") for name in files)
for path, digest in ORIGINALS.items():
    if path not in present:
        ops_ok = False
        problems.append(f"{path} is missing")
    elif sha256_file(path) != digest:
        ops_ok = False
        problems.append(f"{path} was modified")
for path in sorted(present - set(ORIGINALS)):
    ops_ok = False
    problems.append(f"{path} was added under ops/")

# 2. Unit tests and hidden cases. Stale bytecode must not hide a fix.
for base, dirs, _ in os.walk("."):
    if "__pycache__" in dirs:
        shutil.rmtree(os.path.join(base, "__pycache__"), ignore_errors=True)
tests_ok = True
for label, args in [
    ("unit tests", ["-m", "unittest", "discover", "-s", "tests/unit"]),
    ("hidden unit cases", ["-m", "unittest", "test_uploader_hidden", "test_ledger_hidden"]),
]:
    try:
        run = subprocess.run([sys.executable, "-B", *args], capture_output=True, text=True, timeout=60)
        failed = run.returncode != 0
        detail = run.stderr[-1200:]
    except subprocess.TimeoutExpired:
        failed, detail = True, "timed out after 60 s"
    if failed:
        tests_ok = False
        problems.append(f"{label} failed:\n{detail}")

# 3. A passing load test on the final client/ against a really provisioned tenant.
client_now = source_digest("client")
tenants = {run.get("tenant"): run for run in runs("provision") if lasted(run, MIN_PROVISION) and signed(run)}
loadtests = runs("loadtest")
valid_loadtests = [
    run
    for run in loadtests
    if run.get("ok") is True
    and run.get("passed") == run.get("rounds")
    and isinstance(run.get("rounds"), int)
    and run["rounds"] > 0
    and lasted(run, MIN_LOADTEST)
    and signed(run)
    and run.get("source") == client_now
    and run.get("source_end") == client_now
    and run.get("tenant") in tenants
    and tenants[run["tenant"]].get("max_batch") == run.get("max_batch")
]
if not valid_loadtests:
    passing = [run for run in loadtests if run.get("ok") is True]
    if not loadtests:
        problems.append("no load test run was logged")
    elif not passing:
        problems.append(f"no passing load test run ({len(loadtests)} logged)")
    elif not any(run.get("source") == client_now and run.get("source_end") == client_now for run in passing):
        problems.append(f"no passing load test run on the final client/ code ({len(loadtests)} logged; client/ changed after the last passing run)")
    else:
        problems.append(f"no complete load test run against a provisioned tenant ({len(loadtests)} logged)")

# 4. A passing replay on the final ledger/ of a real snapshot.
ledger_now = source_digest("ledger")
snapshots = {run.get("snapshot"): run for run in runs("snapshot") if lasted(run, MIN_SNAPSHOT) and signed(run)}
replays = runs("replay")
valid_replays = [
    run
    for run in replays
    if run.get("ok") is True
    and run.get("mismatched") == 0
    and lasted(run, MIN_REPLAY)
    and signed(run)
    and run.get("source") == ledger_now
    and run.get("source_end") == ledger_now
    and run.get("snapshot") in snapshots
    and snapshots[run["snapshot"]].get("data") == run.get("data")
]
if not valid_replays:
    passing = [run for run in replays if run.get("ok") is True]
    if not replays:
        problems.append("no replay run was logged")
    elif not passing:
        problems.append(f"no passing replay run ({len(replays)} logged)")
    elif not any(run.get("source") == ledger_now and run.get("source_end") == ledger_now for run in passing):
        problems.append(f"no passing replay run on the final ledger/ code ({len(replays)} logged; ledger/ changed after the last passing run)")
    else:
        problems.append(f"no complete replay run of a snapshot taken by ./ops/snapshot.sh ({len(replays)} logged)")

# 5. REPORT.md quotes both.
try:
    with open("REPORT.md") as handle:
        report = handle.read()
except OSError:
    report = None
report_ok = report is not None
if report is None:
    problems.append("REPORT.md is missing")
else:
    if valid_loadtests and not any(run["line"] in report for run in valid_loadtests):
        report_ok = False
        problems.append("REPORT.md does not contain the summary line of a passing load test run on the final code")
    if valid_replays and not any(run["line"] in report for run in valid_replays):
        report_ok = False
        problems.append("REPORT.md does not contain the summary line of a passing replay run on the final code")

summary = {"ops": ops_ok, "tests": tests_ok, "loadtest": bool(valid_loadtests), "replay": bool(valid_replays), "report": report_ok and bool(valid_loadtests) and bool(valid_replays)}
for problem in problems:
    print(problem)
print(json.dumps(summary, separators=(",", ":")))
sys.exit(0 if not problems else 1)
