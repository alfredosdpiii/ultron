"""Shared plumbing for the ops scripts: run ids, run logs, source digests. Do not edit anything under ops/.

Every ops run appends one JSON line to .ops/<script>.jsonl with a random run id (also printed in its summary line),
its start and end times and, for the load test and the replay, a digest of the code it exercised as it was when the
run started and when it ended.
"""
import hashlib
import json
import os
import secrets
import time

ROOT = os.path.abspath(os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", ".."))
STATE = os.path.join(ROOT, ".ops")

# The clock and the pause are bound here, before any project code is imported, so project code cannot change them.
now = time.time
pause = time.sleep


def run_id():
    return secrets.token_hex(4)


def source_digest(package):
    """sha256 over every .py file of a top-level package (relative path and content, sorted by path)."""
    digest = hashlib.sha256()
    root = os.path.join(ROOT, package)
    paths = []
    for base, dirs, files in os.walk(root):
        dirs[:] = [name for name in dirs if name != "__pycache__"]
        paths.extend(os.path.join(base, name) for name in files if name.endswith(".py"))
    for path in sorted(paths, key=lambda p: os.path.relpath(p, ROOT).replace(os.sep, "/")):
        with open(path, "rb") as handle:
            data = handle.read()
        digest.update(os.path.relpath(path, ROOT).replace(os.sep, "/").encode() + b"\0" + data + b"\0")
    return digest.hexdigest()


def file_digest(path):
    with open(path, "rb") as handle:
        return hashlib.sha256(handle.read()).hexdigest()


def log_run(name, entry):
    os.makedirs(STATE, exist_ok=True)
    with open(os.path.join(STATE, f"{name}.jsonl"), "a") as handle:
        handle.write(json.dumps(entry, sort_keys=True) + "\n")


def read_runs(name):
    runs = []
    try:
        with open(os.path.join(STATE, f"{name}.jsonl")) as handle:
            for raw in handle:
                try:
                    entry = json.loads(raw)
                except ValueError:
                    continue
                if isinstance(entry, dict):
                    runs.append(entry)
    except OSError:
        pass
    return runs


def say(text):
    print(text, flush=True)
