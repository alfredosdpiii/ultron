"""Records crates/sandbox/fixtures/autoreview-argv.json: the argv autoreview_tests.py's `Sandbox.wrap` builds for
fixed inputs, its `sandbox_env`, `describe`, and `detect_sandbox`'s choice under scripted `which`/probe results.

The host filesystem layout (which of /usr, /bin, /lib... are symlinks or directories), the uid/gid and the home
directory are faked, so the output does not depend on the machine. The Rust launcher replays the same inputs with
the same fake layout and must produce the same argv.
Usage: /usr/bin/python3 crates/sandbox/fixtures/autoreview_argv.py
"""
import json
import os
import shutil
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[2]
sys.path.insert(0, str(ROOT / "packages/coding-agent/src/ultron/rlm"))
import autoreview_tests as at  # noqa: E402

LAYOUT = {"/usr": "dir", "/bin": "link:usr/bin", "/sbin": "link:usr/bin", "/lib": "dir", "/lib64": "link:usr/lib"}
os.path.islink = lambda p: LAYOUT.get(p, "").startswith("link:")
os.path.isdir = lambda p: LAYOUT.get(p) == "dir"
os.readlink = lambda p: LAYOUT[p][len("link:"):]
os.getuid = lambda: 1234
os.getgid = lambda: 5678
HOME = "/home/tester"
# Whether setpriv is on PATH (Docker's client is tied to its parent with it): scripted per case.
SETPRIV = {"present": True}
real_which = shutil.which
shutil.which = lambda name, *a, **k: ("/usr/bin/setpriv" if SETPRIV["present"] else None) if name == "setpriv" \
    else real_which(name, *a, **k)

ENVS = {
    "sandbox": at.sandbox_env(),
    "sandbox_extra": at.sandbox_env("/work/.venv/bin"),
    "small": {"A": "1", "B": "two words"},
}
WRAPS = [
    {"command": ["true"], "workdir": "/w", "env": "sandbox", "ro_binds": None, "cwd": ""},
    {"command": ["python3", "-m", "pytest", "-q", "tests/x.py"], "workdir": "/tmp/export", "env": "sandbox_extra",
     "ro_binds": ["/tmp/export/.venv", ["/home/tester/repo/node_modules", "/tmp/export/node_modules"], ""],
     "cwd": "pkg/sub"},
    {"command": ["sh", "-c", "echo $HOME"], "workdir": "/x y", "env": "small", "ro_binds": ["/opt/py"], "cwd": ""},
    {"command": ["npm", "test"], "workdir": "/w", "env": "small", "ro_binds": [], "cwd": "a"},
]

out = {"layout": LAYOUT, "uid": 1234, "gid": 5678, "home": HOME,
       "envs": {name: [list(item) for item in env.items()] for name, env in ENVS.items()}, "wraps": [], "describe": {},
       "detect": []}
for mechanism, setpriv in (("bwrap", True), ("unshare", True), ("docker", True), ("docker", False)):
    SETPRIV["present"] = setpriv
    sandbox = at.Sandbox(mechanism, "img:1" if mechanism == "docker" else None, home=HOME)
    out["describe"][mechanism] = sandbox.describe()
    for case in WRAPS:
        binds = [tuple(b) if isinstance(b, list) else b for b in (case["ro_binds"] or [])] if case["ro_binds"] is not None else None
        argv = sandbox.wrap(list(case["command"]), case["workdir"], ENVS[case["env"]], binds, case["cwd"])
        out["wraps"].append({"mechanism": mechanism, "image": sandbox.image, "setpriv": setpriv, **case, "argv": argv})
SETPRIV["present"] = True

# detect_sandbox: which binaries exist, which probes succeed, whether an image is configured, `only`.
DETECT = [
    {"present": ["bwrap", "unshare", "docker"], "working": ["bwrap", "unshare", "docker"], "image": "img", "only": None},
    {"present": ["bwrap", "unshare", "docker"], "working": ["unshare", "docker"], "image": "img", "only": None},
    {"present": ["unshare", "docker"], "working": ["docker"], "image": "img", "only": None},
    {"present": ["unshare", "docker"], "working": ["docker"], "image": None, "only": None},
    {"present": ["bwrap", "unshare"], "working": ["bwrap", "unshare"], "image": None, "only": "unshare"},
    {"present": ["bwrap"], "working": ["bwrap"], "image": None, "only": "docker"},
    {"present": [], "working": [], "image": "img", "only": None},
]
for case in DETECT:
    probes = []

    def executor(argv, cwd, env, timeout, case=case, probes=probes):
        # The launcher is the first mechanism named (Docker's argv starts with its setpriv/bash wrapper).
        launcher = next(part for part in argv if part in ("bwrap", "unshare", "docker"))
        probes.append(launcher)
        return (0, "") if launcher in case["working"] else (1, "nope")

    found = at.detect_sandbox(image=case["image"], which=lambda name, case=case: f"/usr/bin/{name}" if name in case["present"] else None,
                              executor=executor, only=case["only"])
    out["detect"].append({**case, "probed": probes, "mechanism": found.mechanism if found else None})

print(json.dumps(out, indent=1, sort_keys=True))
