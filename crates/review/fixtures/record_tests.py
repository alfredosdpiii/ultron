"""Records autoreview_tests.py (running the reviewed project's tests, sandboxed) for crates/review tests: the
sandbox command line per mechanism (system directory layouts and user ids scripted), the sandbox and launcher
environments, mechanism detection and the self-check with a scripted `which` and executor, the doctor report and
the command line, environment discovery over trees built in a temp dir, runner detection, outcome parsing, test
sessions with a scripted executor and export, summaries, real `run_process` runs and a real `git archive` export.
Run by generate_tests.mjs; prints tests.json. Temp paths are recorded as `<root>` (trees), `<head>`/`<base>`
(exports), `<probe>`/`<work>`/`<canary>` (detection and self-check).
"""
import glob
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile

RLM = os.path.abspath(sys.argv[1])
sys.path.insert(0, RLM)
import autoreview_tests as t  # noqa: E402

ROOT = os.path.realpath(tempfile.mkdtemp(prefix="review-tests-"))
HOME = os.path.join(ROOT, "fakehome")
FIXED_PATH = "/usr/bin:/bin"
# The recorder's own environment is fixed: the launcher environment and every default home read from it.
for name in list(os.environ):
    if name not in ("PATH", "LANG", "LC_ALL", "TMPDIR"):
        del os.environ[name]
os.environ["PATH"] = FIXED_PATH
os.environ["HOME"] = HOME
os.makedirs(HOME)

out = {}

# --- System directory layouts and user ids, scripted inside Sandbox.wrap -------------------------------------

LAYOUTS = {
    "merged": [["/usr", "dir"], ["/bin", "link", "usr/bin"], ["/sbin", "link", "usr/bin"], ["/lib", "link", "usr/lib"],
               ["/lib64", "link", "usr/lib"], ["/lib32", "absent"]],
    "classic": [["/usr", "dir"], ["/bin", "dir"], ["/sbin", "dir"], ["/lib", "dir"], ["/lib64", "dir"],
                ["/lib32", "dir"]],
    "sparse": [["/usr", "dir"], ["/bin", "link", "/usr/bin"], ["/sbin", "absent"], ["/lib", "dir"],
               ["/lib64", "absent"], ["/lib32", "absent"]],
}
out["layouts"] = LAYOUTS
USER = [1234, 567]
current = {"layout": "merged", "setpriv": True}
real_islink, real_isdir, real_readlink = os.path.islink, os.path.isdir, os.readlink
real_getuid, real_getgid = os.getuid, os.getgid


def layout_of(path):
    return next((item for item in LAYOUTS[current["layout"]] if item[0] == path), None)


def fake_islink(path):
    item = layout_of(path)
    return real_islink(path) if item is None else item[1] == "link"


def fake_isdir(path):
    item = layout_of(path)
    return real_isdir(path) if item is None else item[1] == "dir"


def fake_readlink(path, *args, **kwargs):
    item = layout_of(path)
    return real_readlink(path, *args, **kwargs) if item is None else item[2]


real_wrap = t.Sandbox.wrap
real_which = shutil.which


def fake_which(name, *args, **kwargs):
    # Only whether setpriv exists is scripted (Docker's client is tied to its parent with it).
    if name == "setpriv":
        return "/usr/bin/setpriv" if current["setpriv"] else None
    return real_which(name, *args, **kwargs)


def wrap(self, *args, **kwargs):
    os.path.islink, os.path.isdir, os.readlink = fake_islink, fake_isdir, fake_readlink
    os.getuid, os.getgid = (lambda: USER[0]), (lambda: USER[1])
    shutil.which = fake_which
    try:
        return real_wrap(self, *args, **kwargs)
    finally:
        os.path.islink, os.path.isdir, os.readlink = real_islink, real_isdir, real_readlink
        os.getuid, os.getgid = real_getuid, real_getgid
        shutil.which = real_which


t.Sandbox.wrap = wrap

# --- Environments ---------------------------------------------------------------------------------------------

out["sandbox_env"] = [{"extra": extra, "env": t.sandbox_env(extra)}
                      for extra in [None, "", "/opt/venv/bin", "/a/bin:/b/bin"]]


def launch(environ):
    saved = dict(os.environ)
    os.environ.clear()
    os.environ.update(environ)
    try:
        return t._launch_env()
    finally:
        os.environ.clear()
        os.environ.update(saved)


out["launch_env"] = [{"environ": environ, "env": launch(environ)} for environ in [
    {"PATH": "/a:/b"}, {}, {"PATH": "/x", "DOCKER_HOST": "tcp://h:2375", "GH_TOKEN": "ghp_x", "SSH_AUTH_SOCK": "/s"},
    {"PATH": ""}, {"DOCKER_HOST": ""},
]]

# --- Sandbox command lines ------------------------------------------------------------------------------------

ENV = t.sandbox_env("/opt/venv/bin")
WRAPS = [
    ["bwrap", None, "/home/someone", "merged", ["python3", "-m", "pytest"], "/tmp/export-1", ENV, ["/opt/venv"], ""],
    ["bwrap", None, "/home/someone", "classic", ["make", "test"], "/tmp/export-1", t.sandbox_env(),
     [["/src/.venv", "/tmp/export-1/.venv"], "", None, "/opt/py"], "web/pkg"],
    ["bwrap", None, "/home/someone", "sparse", ["x"], "/w", {}, None, ""],
    ["unshare", None, "/home/someone", "merged", ["make", "test"], "/tmp/export-1", t.sandbox_env(), ["/opt/venv"], ""],
    ["unshare", None, "/root", "merged", ["npm", "test"], "/tmp/e", ENV, None, "pkg"],
    ["unshare", None, "/home", "merged", ["true"], "/tmp/e", {"A": "1 2", "B": ""}, None, ""],
    ["docker", "python:3.12", "/home/someone", "merged", ["make", "test"], "/tmp/export-1", t.sandbox_env(),
     ["/opt/venv", ["/src/node_modules", "/tmp/export-1/node_modules"]], ""],
    ["docker", None, "/home/someone", "merged", ["go", "test"], "/tmp/e", {}, None, "svc"],
]
out["wrap"] = []
for setpriv in (True, False):
    for mechanism, image, home, layout, command, workdir, env, binds, cwd in WRAPS:
        if not setpriv and mechanism != "docker":
            continue
        current["layout"], current["setpriv"] = layout, setpriv
        sandbox = t.Sandbox(mechanism, image, home=home)
        ro = None if binds is None else [tuple(item) if isinstance(item, list) else item for item in binds]
        out["wrap"].append({"mechanism": mechanism, "image": image, "home": home, "layout": layout,
                            "command": command, "workdir": workdir, "env": env, "binds": binds, "cwd": cwd,
                            "setpriv": setpriv, "argv": sandbox.wrap(command, workdir, env, ro, cwd)})
current["layout"], current["setpriv"] = "merged", True
out["user"] = f"{USER[0]}:{USER[1]}"
out["describe"] = [{"mechanism": m, "image": i, "text": t.Sandbox(m, i, home=HOME).describe()}
                   for m, i in [("bwrap", None), ("unshare", None), ("docker", "python:3.12"), ("docker", None)]]

# --- Detection ------------------------------------------------------------------------------------------------


def detect_case(have, executor, image=None, only=None):
    calls, probes = [], []

    def run(argv, cwd, env, timeout):
        probes.append(cwd)
        calls.append({"argv": [part.replace(cwd, "<probe>") for part in argv], "cwd": "<probe>", "env": env,
                      "timeout": timeout, "probeExists": os.path.isdir(cwd)})
        if executor == "ok":
            return 0, ""
        if executor == "broken":
            return 1, "bwrap: setting up uid map: Permission denied"
        return (0 if argv[0] == executor.split(":")[1] else 1), ""

    sandbox = t.detect_sandbox(image=image, which=lambda name: "/usr/bin/" + name if name in have else None,
                               executor=run, only=only)
    return {"have": have, "executor": executor, "image": image, "only": only,
            "picked": None if sandbox is None else [sandbox.mechanism, sandbox.image], "calls": calls,
            "probesLeft": sum(os.path.exists(path) for path in probes)}


out["detect"] = [
    detect_case(["bwrap", "unshare", "docker"], "ok", "img"),
    detect_case(["unshare", "docker"], "ok", "img"),
    detect_case(["bwrap", "unshare"], "only:unshare"),
    detect_case(["docker"], "ok", "img"),
    detect_case(["docker"], "ok"),
    detect_case(["docker"], "ok", ""),
    detect_case(["bwrap", "unshare"], "broken"),
    detect_case([], "ok"),
    detect_case(["bwrap", "unshare", "docker"], "only:docker", "img"),
    detect_case(["bwrap", "unshare", "docker"], "ok", "img", "unshare"),
    detect_case(["bwrap", "unshare"], "ok", None, "docker"),
    detect_case(["bwrap"], "ok", None, "nothing"),
]

# --- Self-check -----------------------------------------------------------------------------------------------

GOOD = {"network": "unreachable", "canary": "unreadable", "token": "absent", "credentialVariables": [],
        "home": "/tmp/home", "workdir": "writable", "system": "read-only", "dockerSocket": "hidden"}
CANARY = re.compile(r"\.ultron-autoreview-canary-[0-9a-f]{8}")


def check_case(mechanism, code, output):
    calls = []

    def run(argv, cwd, env, timeout):
        canary = argv[-1]
        calls.append({"argv": [CANARY.sub("<canary>", part.replace(cwd, "<work>")) for part in argv],
                      "env": env, "timeout": timeout, "canaryExists": os.path.exists(canary),
                      "canary": CANARY.sub("<canary>", canary)})
        return code, output

    result = t.self_check(t.Sandbox(mechanism, "img" if mechanism == "docker" else None), run)
    leftovers = glob.glob(os.path.join(HOME, ".ultron-autoreview-canary-*"))
    return {"mechanism": mechanism, "code": code, "output": output, "result": result, "calls": calls,
            "left": len(leftovers)}


line = lambda value: "noise\nSELFCHECK " + json.dumps(value) + "\n"  # noqa: E731
out["self_check"] = [
    check_case("bwrap", 0, line(GOOD)),
    check_case("unshare", 0, line(dict(GOOD, network="reachable"))),
    check_case("bwrap", 0, line(dict(GOOD, credentialVariables=["GH_TOKEN"]))),
    check_case("docker", 0, line(dict(GOOD, home="/root", dockerSocket="visible"))),
    check_case("bwrap", 1, "bwrap: No permissions to create new namespace\n" + "x" * 400),
    check_case("bwrap", 0, "SELFCHECK-ish\n"),
    check_case("bwrap", 0, "first\r\nSELFCHECK " + json.dumps(GOOD) + "\r\n"),
]

# --- Doctor and the command line ------------------------------------------------------------------------------

real_detect, real_self_check = t.detect_sandbox, t.self_check


def doctor_case(have, check_output, image=None):
    t.detect_sandbox = lambda image=None: real_detect(
        image=image, which=lambda name: "/usr/bin/" + name if name in have else None,
        executor=lambda argv, cwd, env, timeout: (0, ""))
    t.self_check = lambda sandbox: real_self_check(sandbox, lambda argv, cwd, env, timeout: (0, check_output))
    try:
        report = t.doctor(image)
    finally:
        t.detect_sandbox, t.self_check = real_detect, real_self_check
    return {"have": have, "image": image, "output": check_output, "report": report, "json": json.dumps(report)}


out["doctor"] = [
    doctor_case([], ""),
    doctor_case(["bwrap"], line(GOOD)),
    doctor_case(["docker"], line(dict(GOOD, network="reachable")), "python:3.12"),
    doctor_case(["unshare"], "nothing here"),
]

# --- Trees: checkouts with environments -----------------------------------------------------------------------


def build(base, tree):
    for entry in tree:
        path = os.path.join(base, entry[1])
        os.makedirs(os.path.dirname(path), exist_ok=True)
        if entry[0] == "dir":
            os.makedirs(path, exist_ok=True)
        elif entry[0] == "link":
            os.symlink(entry[2].replace("<root>", ROOT), path)
        else:
            with open(path, "w", encoding="utf-8", newline="") as handle:
                handle.write(entry[2])


UV = "fakehome/.local/share/uv/python/cpython-3.12"
NODE = "fakehome/.local/share/mise/installs/node/20"
TREE = [
    # The TypeScript test's checkout: a virtualenv, node_modules at the root and in a package, a secret, a decoy.
    ["dir", "co/.venv/bin"], ["dir", "co/.venv/lib"], ["dir", "co/node_modules/.bin"], ["dir", "co/web/src"],
    ["dir", "co/.git"], ["dir", "co/outside-env/bin"], ["link", "co/.venv/bin/python", "/usr/bin/python-none"],
    ["file", "co/.venv/lib/marker.txt", "site-packages\n"], ["file", "co/web/node_modules/.bin/vitest", "#!/bin/sh\n"],
    ["file", "co/.env", "SECRET=hunter2\n"], ["file", "co/source.py", "print('checkout source')\n"],
    ["file", "co/.git/config", "[remote]\n"], ["link", "co/svc/venv", "<root>/elsewhere"],
    ["link", "elsewhere/bin/python", "/usr/bin/python-none"],
    # A uv-managed interpreter under the home, and a binary directly in the home's bin.
    ["file", UV + "/bin/python3.12", ""], ["link", UV + "/bin/python3", "python3.12"], ["dir", UV + "/lib"],
    ["file", "fakehome/bin/python3", ""], ["dir", "fakehome/lib"],
    ["file", NODE + "/bin/node", ""], ["dir", NODE + "/lib"],
    ["link", "managed/.venv/bin/python", "<root>/" + UV + "/bin/python3"], ["dir", "managed/node_modules"],
    # Depth: one chain, so the walk order is the same on every filesystem.
    ["dir", "deep/a/b/c/node_modules"], ["dir", "deep/a/b/c/d/node_modules"], ["dir", "deep/a/.hidden/node_modules"],
    ["dir", "deep/a/node_modules/inner/node_modules"], ["dir", "deep/a/venv/lib"],
    ["file", "deep/a/b/.venv/bin/python", ""],
    # Environments that are symlinks out of the checkout.
    ["link", "linked/node_modules", "<root>/elsewhere"], ["link", "linked/.venv", "<root>/elsewhere"],
    ["link", "linked/pkg", "<root>/co/web"],
    # Interpreter layouts.
    ["file", "tools/notbin/python", ""], ["file", "tools/nolib/bin/python", ""], ["file", "tools/ok/bin/python", ""],
    ["dir", "tools/ok/lib"], ["link", "tools/chain/bin/python", "<root>/tools/ok/bin/python"],
    # A virtualenv that is the checkout itself, whose interpreter's directory is the checkout.
    ["link", "self/.venv", "<root>/self"], ["file", "self/bin/python", ""], ["dir", "self/lib"],
    ["link", "self/node_modules", "<root>/self"],
    # Test session toolchains: python with bin, node with bin/node, and one without bin.
    ["dir", "tc/python/3.12/bin"], ["file", "tc/node/20/bin/node", ""], ["dir", "tc/missing/3"],
    ["dir", "envdir/lib"], ["dir", "envbin/bin"],
]
build(ROOT, TREE)
out["tree"] = TREE
sub = lambda value: json.loads(json.dumps(value).replace(ROOT, "<root>"))  # noqa: E731
R = lambda path: os.path.join(ROOT, path)  # noqa: E731

out["interpreter_home"] = [
    {"binary": b, "home": h, "prefix": sub(t.interpreter_home(R(b) if not b.startswith("/") else b, R(h) if h else None))}
    for b, h in [["managed/.venv/bin/python", "fakehome"], ["/usr/bin/python-none", "fakehome"], ["fakehome/bin/python3", "fakehome"],
                 ["/no/such/python", "fakehome"], ["tools/notbin/python", "fakehome"], ["tools/nolib/bin/python", "fakehome"],
                 ["tools/ok/bin/python", "fakehome"], ["tools/chain/bin/python", "fakehome"], ["tools/ok/bin/python", "tools/ok/x"],
                 ["tools/ok/bin/python", "tools/ok"], ["tools/ok/bin/python", None], [UV + "/bin/python3", None]]
]


def discover(checkout, directory, home):
    found = t.discover_environment(R(checkout) if not checkout.startswith("/") else checkout, directory,
                                   R(home) if home else None)
    return sub({"binds": [list(item) for item in found["binds"]], "python": found["python"],
                "interpreters": found["interpreters"]})


out["discover"] = [{"checkout": c, "directory": d, "home": h, "found": discover(c, d, h)} for c, d, h in [
    ["co", "", None], ["co", "web/src", None], ["co", "svc", None], ["co", "/web/src/", None], ["co", "web", None],
    ["co", "nothing/here/x", None], ["/no/such/checkout", "", None], ["managed", "", "fakehome"], ["managed", "", None],
    ["linked", "", None], ["linked", "pkg/src", None], ["deep", "a/b/c/d", None], ["co/web", "src", None],
    ["self", "", None],
]]
out["list"] = [{"checkout": c, "depth": d, "found": sub(t.list_environments(R(c) if not c.startswith("/") else c, d))}
               for c, d in [["co", 3], ["managed", 3], ["deep", 3], ["deep", 2], ["deep", 4], ["deep", 0],
                            ["linked", 3], ["/no/such", 3], ["elsewhere", 3], ["self", 1]]]

# An interpreter whose install directory would be a top-level directory's child (`/tmp/<dir>`): never bound.
top = os.path.realpath(tempfile.mkdtemp(prefix="review-top-"))
os.makedirs(os.path.join(top, "bin"))
os.makedirs(os.path.join(top, "lib"))
open(os.path.join(top, "bin", "python"), "w").close()
out["top_level"] = {"slashes": top.count("/"), "prefix": t.interpreter_home(os.path.join(top, "bin", "python"), HOME)}
shutil.rmtree(top)

# The command line, as a separate process.
script = os.path.join(RLM, "autoreview_tests.py")


def cli(args):
    done = subprocess.run([sys.executable, "-I", script, *args], capture_output=True, text=True,
                          env={"PATH": FIXED_PATH, "HOME": HOME}, cwd=ROOT)
    return {"args": sub(args), "code": done.returncode, "stdout": sub(done.stdout), "stderr": done.stderr}


out["cli"] = [cli(args) for args in [[], ["x"], ["environments"], ["environments", R("co")],
                                     ["environments", R("deep")], ["environments", "a", "b"]]]

# --- Runners --------------------------------------------------------------------------------------------------

DETECT = [
    {"pytest.ini": "", "tests/test_a.py": ""},
    {"pyproject.toml": "[tool.pytest.ini_options]", "a.py": ""},
    {"pyproject.toml": "[project]", "a.py": ""},
    {"setup.cfg": "[tool:pytest]"},
    {"tox.ini": "[testenv]\ncommands = pytest\n"},
    {"tox.ini": "[testenv]\n"},
    {"conftest.py": ""},
    {"src/x.py": "", "tests/test_x.py": ""},
    {"src/x_test.py": ""},
    {"package.json": json.dumps({"devDependencies": {"vitest": "1"}})},
    {"package.json": json.dumps({"dependencies": {"jest": "1"}})},
    {"package.json": json.dumps({"scripts": {"test": "jest --ci"}})},
    {"package.json": json.dumps({"scripts": {"test": "vitest run"}, "devDependencies": {"jest": "1"}})},
    {"package.json": json.dumps({"scripts": {"test": "node test.js"}}), "pnpm-lock.yaml": ""},
    {"package.json": json.dumps({"scripts": {"test": "node test.js"}}), "yarn.lock": ""},
    {"package.json": json.dumps({"scripts": {"test": "node test.js"}})},
    {"package.json": json.dumps({"scripts": {"test": "echo Error: no test specified && exit 1"}})},
    {"package.json": json.dumps({"scripts": {"test": 5}})},
    {"package.json": json.dumps({"scripts": {"test": True}})},
    {"package.json": json.dumps({"scripts": {"test": ["vitest"]}})},
    {"package.json": json.dumps({"scripts": {"test": ""}}), "go.mod": "module x"},
    {"package.json": "not json", "Cargo.toml": ""},
    {"package.json": "", "Makefile": "test:\n\t./run\n"},
    {"go.mod": "module x"},
    {"Cargo.toml": ""},
    {"Makefile": "build:\n\tcc x.c\ntest:\n\t./run\n"},
    {"Makefile": "build:\n\tcc x.c\n", "README.md": ""},
    {"Makefile": "  test:\n"},
    {"README.md": "", "test.py": "", "a_tests.py": ""},
    {},
]
COMMANDS = [[["tests/test_a.py"], "name"], [[], None], [["a/x_test.go", "a/y_test.go", "b_test.go"], None],
            [["x.ts", "y.ts"], ""]]


def runner_json(runner):
    return None if runner is None else [runner.name, runner.because]


out["detect_runner"] = []
for files in DETECT:
    runner = t.detect_runner(list(files), lambda path, files=files: files.get(path))
    out["detect_runner"].append({"files": files, "runner": runner_json(runner),
                                 "commands": [[p, s, runner.command(p, s)] for p, s in COMMANDS] if runner else []})
out["commands"] = [{"runner": name, "paths": p, "select": s, "argv": t.TestRunner(name, "x").command(p, s)}
                   for name in ["pytest", "vitest", "jest", "npm", "pnpm", "yarn", "go", "cargo", "make", "other"]
                   for p, s in COMMANDS]

MONO = {
    "pyproject.toml": "[tool.pytest.ini_options]",
    "api/tests/test_users.py": "",
    "api/conftest.py": "",
    "web/package.json": json.dumps({"devDependencies": {"vitest": "1"}}),
    "web/src/Resources.test.tsx": "",
    "web/src/Resources.tsx": "",
    "packages/ui/package.json": json.dumps({"scripts": {"test": "jest"}}),
    "packages/ui/src/Button.test.jsx": "",
    "packages/ui/src/deep/package.json": json.dumps({"name": "no runner here"}),
    "packages/ui/src/deep/x.spec.ts": "",
    "packages/cli/package.json": json.dumps({"scripts": {"test": "node --test"}}),
    "packages/cli/src/a.test.mjs": "",
    "tools/loose.test.ts": "",
    "svc/go.mod": "module svc",
    "svc/pkg/a_test.go": "",
    "crate/Cargo.toml": "",
    "crate/src/lib.rs": "",
    "lone/x.rs": "",
    "lone/y.go": "",
    "notes/readme.txt": "",
    "yarn.lock": "",
    "x.cts": "",
}
MONO_NAMES = ["api/tests/test_users.py", "web/src/Resources.test.tsx", "packages/ui/src/Button.test.jsx",
              "packages/ui/src/deep/x.spec.ts", "tools/loose.test.ts", "svc/pkg/a_test.go", "crate/src/lib.rs",
              "packages/cli/src/a.test.mjs", "lone/x.rs", "lone/y.go", "notes/readme.txt", "test_root.py", "x.cts"]
mono_tracked = set(MONO)
out["runner_for"] = {"files": MONO, "cases": [
    [name, (lambda found: None if found is None else [found[0].name, found[0].because, found[1]])(
        t.runner_for(name, mono_tracked, MONO.get))] for name in MONO_NAMES]}
out["nearest"] = [[path, names, t._nearest(mono_tracked, path, tuple(names))] for path, names in [
    ["api/tests/test_users.py", ["conftest.py"]], ["api/tests/test_users.py", ["pyproject.toml"]],
    ["svc/pkg/deeper/a_test.go", ["go.mod"]], ["x.go", ["go.mod"]], ["a/b/c.py", ["nothing"]],
    ["packages/ui/src/deep/x.spec.ts", ["package.json"]]]]

# --- Outcomes -------------------------------------------------------------------------------------------------

OUTCOMES = [
    ["pytest", 0, "PASSED tests/a.py::t"],
    ["pytest", 1, "FAILED tests/a.py::t - assert 1 == 2"],
    ["pytest", 1, "ImportError while importing test module 'tests/a.py'\nModuleNotFoundError: No module named 'requests'"],
    ["pytest", 1, "/usr/bin/python3: No module named pytest"],
    ["pytest", 5, "no tests ran"],
    ["pytest", 4, "ERROR: file or directory not found: x"],
    ["pytest", 4, "FAILED a.py::t\n"],
    ["pytest", 127, "npx: not found"],
    ["pytest", 124, ""],
    ["pytest", 124, "PASSED a.py::x\n"],
    ["pytest", 1, "FAILED tests/a.py::t - ModuleNotFoundError: No module named 'x'"],
    ["pytest", 1, "PASSED a.py::one\nERROR a.py::two - fixture 'db' not found\nFAILED a.py::three\n"],
    ["pytest", 2, "ERROR a.py::two\nerrors during collection\n"],
    ["pytest", 1, "FAILED a.py::t - " + "d" * 300 + "\n"],
    ["pytest", 1, "PASSED a.py::t\r\nFAILED a.py::u - boom\r\n"],
    ["pytest", 1, "PASSED\na.py::split\n  FAILED a.py::indented\nfailed a.py::lower\n"],
    ["pytest", 1, "FAILED a.py::t -\n"],
    ["pytest", 1, "Something went wrong\n"],
    ["vitest", 1, "Error: Cannot find module 'vitest'"],
    ["vitest", 1, "FAIL src/a.test.ts > adds\n"],
    ["jest", 1, "sh: 1: jest: not found"],
    ["npm", 1, "npm ERR! missing script: test"],
    ["go", 1, "--- FAIL: TestX (0.00s)"],
    ["go", 0, "=== RUN TestA\n--- PASS: TestA (0.00s)\n    --- FAIL: TestA/sub (0.00s)\nok  x 0.1s\n"],
    ["go", 1, "go: no required module provides package x"],
    ["cargo", 101, "test a::b ... ok\ntest a::c ... FAILED\ntest result: FAILED.\n"],
    ["cargo", 101, "error: no matching package named `serde` found\n"],
    ["cargo", 0, "test x ... ok\n"],
    ["make", 2, "make: *** [Makefile:2: test] Error 1\n"],
    ["make", 2, "Temporary failure in name resolution\n"],
    ["make", 2, "GETADDRINFO failed\n"],
    ["make", -9, ""],
]
out["parse_outcome"] = [{"runner": r, "code": c, "output": o, "status": t.parse_outcome(r, c, o)[0],
                         "tests": t.parse_outcome(r, c, o)[1]} for r, c, o in OUTCOMES]

# --- Test sessions --------------------------------------------------------------------------------------------

APP_BASE = 'KINDS = ["a", "b"]\n\n\ndef show(kind):\n    return kind.upper()\n'
APP_HEAD = 'KINDS = ["a", "b", "c"]\n\n\ndef show(kind):\n    return kind.upper()\n'
TEST_APP = ('from app import KINDS, show\n\n\ndef test_kinds():\n    assert KINDS == ["a", "b"]\n\n\n'
            'def test_show():\n    assert show("a")\n')
FIXTURE_BASE = {"pytest.ini": "[pytest]\n", "src/app.py": APP_BASE, "tests/test_app.py": TEST_APP}
FIXTURE_HEAD = dict(FIXTURE_BASE, **{"src/app.py": APP_HEAD, "src/crlf.py": "a = 1\r\nb = 2\r\n",
                                     "src/last.py": "x = 1\ny = 2", "src/cr.py": "a = 1\rb = 2\n",
                                     "src/nul.py": "a\0b\n", "src/ff.py": "a\fb\n", "src/big.py": "é = 1\n" * 3})


def executor_for(spec, calls, labels):
    def norm(text):
        for path, label in labels.items():
            text = text.replace(path, label)
        return text.replace(ROOT, "<root>")

    def run(argv, cwd, env, timeout):
        watched = {}
        for name in spec.get("watch", []):
            path = os.path.join(cwd, name)
            watched[name] = open(path, encoding="utf-8", newline="").read() if os.path.exists(path) else None
        calls.append({"argv": [norm(part) for part in argv], "cwd": norm(cwd), "env": env, "timeout": timeout,
                      "watched": watched})
        kind = spec["kind"]
        if kind == "explode":
            raise OSError(spec["message"])
        if kind == "fixture":
            source = open(os.path.join(cwd, "src/app.py")).read()
            kinds = ("FAILED tests/test_app.py::test_kinds - AssertionError: assert ['a', 'b', 'c'] == ['a', 'b']"
                     if '"c"' in source else "PASSED tests/test_app.py::test_kinds")
            show = ("PASSED tests/test_app.py::test_show" if "upper()" in source
                    else "FAILED tests/test_app.py::test_show - AssertionError")
            return (1 if "FAILED" in kinds + show else 0), kinds + "\n" + show + "\n"
        if kind == "make":
            value = open(os.path.join(cwd, "value.txt")).read()
            return (0, "home=/tmp/home\n") if value == "1\n" else (2, "make: *** [Makefile:2: test] Error 1\n")
        items = spec["items"]
        code, output = items[min(len(calls) - 1, len(items) - 1)]
        return code, output.replace("{cwd}", cwd)

    return run


def session_case(name, trees, steps, executor, tracked=None, mechanism="bwrap", head="HEAD1", base="BASE1",
                 options=None, node=None, export_fail=()):
    options = dict(options or {})
    for key in ("env_dir", "checkout"):
        if options.get(key):
            options[key] = R(options[key])
    if options.get("toolchain"):
        options["toolchain"] = [R(path) for path in options["toolchain"]]
    calls, labels, exported = [], {}, []
    files = trees[head] if head in trees else {}
    tracked = sorted(files) if tracked is None else tracked

    def export(root, rev):
        if rev in export_fail:
            raise RuntimeError(f"the commit {rev[:12]} could not be exported")
        path = tempfile.mkdtemp(prefix="ultron-autoreview-run-")
        for file, text in trees.get(rev, {}).items():
            os.makedirs(os.path.dirname(os.path.join(path, file)), exist_ok=True)
            with open(os.path.join(path, file), "w", encoding="utf-8", newline="") as handle:
                handle.write(text)
        labels[path] = "<head>" if rev == head else "<base>"
        exported.append(path)
        return path

    ticks = [0]

    def clock():
        ticks[0] += 1
        return 100.0 + 0.375 * ticks[0]

    real_which = shutil.which
    shutil.which = lambda name, *args, **kwargs: (R(node) if node else None) if name == "node" else real_which(name)
    session = t.TestSession("/repo", head, base, tracked, lambda path: files.get(path),
                            t.Sandbox(mechanism, "img" if mechanism == "docker" else None), executor=executor_for(
                                executor, calls, labels), export=export, clock=clock, **options)
    shutil.which = real_which

    def norm(value):
        text = json.dumps(value)
        for path, label in labels.items():
            text = text.replace(path, label)
        return json.loads(text.replace(ROOT, "<root>"))

    results = []
    for step in steps:
        op = step["op"]
        shutil.which = lambda name, *args, **kwargs: (R(node) if node else None) if name == "node" else real_which(name)
        try:
            if op == "paths":
                value = session.paths(step.get("raw"))
            elif op == "plan":
                value = [[runner.name, runner.because, directory, paths] for runner, directory, paths in
                         session.plan(step["paths"])]
            elif op == "run":
                value = session.run(step["paths"], step.get("select"), which=step.get("which", "head"),
                                    kind=step.get("kind", "run"))
            elif op == "serve":
                args = step["args"]
                if step["kind"] == "run_tests":
                    record = session.run(session.paths(args.get("paths")), args.get("select"))
                else:
                    record = session.mutation(args.get("path"), args.get("line"), args.get("replacement"),
                                              args.get("tests"))
                value = [f"test run {record['n']}", t.summarize(record)]
            elif op == "mutation":
                value = session.mutation(step.get("path"), step.get("line"), step.get("replacement"), step.get("tests"))
            elif op == "compare":
                value = session.compare(step["paths"])
            elif op == "executor":
                session._executor = executor_for(step["spec"], calls, labels)
                value = None
            elif op == "file":
                path = os.path.join(session._dirs[step.get("which", "head")], step["path"])
                value = open(path, encoding="utf-8", newline="").read()
            elif op == "close":
                session.close()
                value = [os.path.exists(path) for path in exported]
            results.append({"ok": norm(value)})
        except (t.TestsRejected, RuntimeError, OSError) as error:
            kind = ("TestsRejected" if isinstance(error, t.TestsRejected) else
                    "RuntimeError" if isinstance(error, RuntimeError) else "OSError")
            results.append({"error": [kind, norm(str(error))]})
        finally:
            shutil.which = real_which
    session.close()
    return {"name": name, "trees": trees, "tracked": tracked, "mechanism": mechanism, "head": head, "base": base,
            "options": {key: (sub(value) if key in ("env_dir", "checkout", "toolchain") else value)
                        for key, value in options.items()},
            "node": node, "exportFail": list(export_fail), "executor": executor, "steps": steps,
            "results": results, "calls": calls, "records": norm(session.records),
            "summaries": [norm(t.summarize(record)) for record in session.records],
            "runner": runner_json(session.runner), "toolchain": sub(session.toolchain), "envKind": session.env_kind,
            "limit": session.limit, "exports": [labels[path] for path in exported]}


FIX = {"HEAD1": FIXTURE_HEAD, "BASE1": FIXTURE_BASE}
FIXTURE = {"kind": "fixture", "watch": ["src/app.py"]}
sessions = []
sessions.append(session_case("limit", FIX, [
    {"op": "run", "paths": ["tests/test_app.py"]},
    {"op": "run", "paths": ["tests/test_app.py"], "select": "test_show"},
    {"op": "run", "paths": ["tests/test_app.py"]},
    {"op": "close"},
], FIXTURE, options={"runs": 2, "timeout_s": 45, "env_dir": "envdir"}))
sessions.append(session_case("env-bin", FIX, [
    {"op": "run", "paths": ["tests"], "select": "kinds or show"},
], FIXTURE, options={"env_dir": "envbin", "toolchain": ["tc/python/3.12", "tc/missing/3", "tc/nope"]}))
LONG = "x" * 120
sessions.append(session_case("refusals", FIX, [
    {"op": "paths", "raw": ["../outside"]}, {"op": "paths", "raw": ["/etc/passwd"]},
    {"op": "paths", "raw": ["tests/missing.py"]}, {"op": "paths", "raw": ["-rf"]}, {"op": "paths", "raw": []},
    {"op": "paths", "raw": ["tests"] * 20}, {"op": "paths", "raw": "tests"}, {"op": "paths", "raw": None},
    {"op": "paths", "raw": [5]}, {"op": "paths", "raw": [None]}, {"op": "paths", "raw": ["a\\b"]},
    {"op": "paths", "raw": [":x"]}, {"op": "paths", "raw": ["~x"]}, {"op": "paths", "raw": ["it's"]},
    {"op": "paths", "raw": ["a\0b"]}, {"op": "paths", "raw": [""]}, {"op": "paths", "raw": [{"a": [1, 2.5, True]}]},
    {"op": "paths", "raw": ["tests", "tests/test_app.py", "./tests/", "tests//test_app.py", "src"]},
    {"op": "paths", "raw": ["sr"]}, {"op": "paths", "raw": ["tests/./x/.."]},
    {"op": "paths", "raw": ["tests"] * 12},
    {"op": "run", "paths": ["tests/test_app.py"], "select": "x; rm -rf /"},
    {"op": "run", "paths": ["tests/test_app.py"], "select": "$(id)"},
    {"op": "run", "paths": ["tests/test_app.py"], "select": LONG + "x"},
    {"op": "run", "paths": ["tests/test_app.py"], "select": ""},
    {"op": "serve", "kind": "run_tests", "args": {"paths": ["tests/test_app.py"], "select": 5}},
    {"op": "serve", "kind": "run_tests", "args": {"paths": "tests/test_app.py"}},
    {"op": "run", "paths": ["README.md"]},
    {"op": "mutation", "path": "src/app.py", "line": 1, "replacement": "a\nb", "tests": ["tests/test_app.py"]},
    {"op": "mutation", "path": "src/app.py", "line": 1, "replacement": "a\0b", "tests": ["tests/test_app.py"]},
    {"op": "mutation", "path": "src/app.py", "line": 1, "replacement": "y" * 301, "tests": ["tests/test_app.py"]},
    {"op": "mutation", "path": "src/app.py", "line": 1, "replacement": 7, "tests": ["tests/test_app.py"]},
    {"op": "mutation", "path": "src/app.py", "line": 99, "replacement": "x", "tests": ["tests/test_app.py"]},
    {"op": "mutation", "path": "src/nope.py", "line": 1, "replacement": "x", "tests": ["tests/test_app.py"]},
    {"op": "mutation", "path": "src/app.py", "line": 1, "replacement": "x", "tests": ["../tests"]},
    {"op": "mutation", "path": "src/app.py", "line": 1, "replacement": "x", "tests": []},
    {"op": "mutation", "path": "src", "line": 1, "replacement": "x", "tests": ["tests"]},
    {"op": "mutation", "path": 5, "line": 1, "replacement": "x", "tests": ["tests"]},
    {"op": "mutation", "line": 1, "replacement": "x", "tests": ["tests"]},
    {"op": "mutation", "path": "/etc/passwd", "line": 1, "replacement": "x", "tests": ["tests"]},
    {"op": "mutation", "path": "src/app.py", "line": "abc", "replacement": "x", "tests": ["tests"]},
    {"op": "mutation", "path": "src/app.py", "replacement": "x", "tests": ["tests"]},
    {"op": "mutation", "path": "src/app.py", "line": 0, "replacement": "x", "tests": ["tests"]},
    {"op": "mutation", "path": "src/app.py", "line": -1, "replacement": "x", "tests": ["tests"]},
    {"op": "mutation", "path": "src/app.py", "line": 6, "replacement": "x", "tests": ["tests"]},
    {"op": "mutation", "path": "src/app.py", "line": "99", "replacement": "x", "tests": ["tests"]},
    {"op": "mutation", "path": "src/app.py", "line": 9.5, "replacement": "x", "tests": ["tests"]},
    {"op": "mutation", "path": "src/app.py", "line": "1.5", "replacement": "x", "tests": ["tests"]},
    {"op": "mutation", "path": "src/app.py", "line": [1], "replacement": "x", "tests": ["tests"]},
    {"op": "mutation", "path": "src/nul.py", "line": 1, "replacement": "x", "tests": ["tests"]},
    {"op": "close"},
], FIXTURE, options={"runs": 10}))
sessions.append(session_case("serve-base-unknown", FIX, [
    {"op": "run", "paths": ["tests/test_app.py"], "which": "base"},
    {"op": "compare", "paths": ["tests/test_app.py"]},
    {"op": "serve", "kind": "run_tests", "args": {"paths": ["tests/test_app.py"], "select": "test_kinds"}},
    {"op": "serve", "kind": "mutation_check",
     "args": {"path": "src/app.py", "line": "5", "replacement": "    return kind", "tests": ["tests/test_app.py"]}},
], FIXTURE, base=None))
sessions.append(session_case("compare-regression", FIX, [
    {"op": "compare", "paths": ["tests/test_app.py"]}, {"op": "close"}], FIXTURE))
sessions.append(session_case("compare-old-failure", FIX, [{"op": "compare", "paths": ["tests/test_app.py"]}],
                             {"kind": "items", "items": [[1, "FAILED tests/test_app.py::test_kinds - AssertionError\n"
                                                             "PASSED tests/test_app.py::test_show"]]}))
sessions.append(session_case("compare-clean", FIX, [{"op": "compare", "paths": ["tests/test_app.py"]}],
                             {"kind": "items", "items": [[0, "PASSED tests/test_app.py::test_kinds"]]}))
sessions.append(session_case("compare-base-export-fails", FIX, [
    {"op": "compare", "paths": ["tests/test_app.py"]}], FIXTURE, export_fail=("BASE1",)))
sessions.append(session_case("head-export-fails", FIX, [
    {"op": "compare", "paths": ["tests/test_app.py"]}, {"op": "run", "paths": ["tests"]}], FIXTURE,
    export_fail=("HEAD1",), head="HEAD1"))
sessions.append(session_case("compare-base-explodes", FIX, [
    {"op": "run", "paths": ["tests/test_app.py"]},
    {"op": "executor", "spec": {"kind": "explode", "message": "bwrap vanished"}},
    {"op": "compare", "paths": ["tests/test_app.py"]}], FIXTURE))
sessions.append(session_case("compare-limit", FIX, [
    {"op": "compare", "paths": ["tests/test_app.py"]}, {"op": "compare", "paths": ["tests/test_app.py"]}],
    FIXTURE, options={"runs": 1}))
sessions.append(session_case("compare-zero", FIX, [
    {"op": "compare", "paths": ["tests/test_app.py"]}, {"op": "run", "paths": ["tests"]}], FIXTURE,
    options={"runs": -3}))
sessions.append(session_case("mutation", FIX, [
    {"op": "mutation", "path": "src/app.py", "line": 5, "replacement": "    return kind", "tests": ["tests/test_app.py"]},
    {"op": "file", "path": "src/app.py"},
    {"op": "mutation", "path": "src/app.py", "line": 2, "replacement": "# nothing", "tests": ["tests/test_app.py"]},
    {"op": "mutation", "path": "src/crlf.py", "line": 1, "replacement": "a = 3", "tests": ["tests"]},
    {"op": "file", "path": "src/crlf.py"},
    {"op": "mutation", "path": "src/last.py", "line": True, "replacement": "z = 0", "tests": ["tests"]},
    {"op": "mutation", "path": "src/cr.py", "line": 1, "replacement": "c = 3", "tests": ["tests"]},
    {"op": "mutation", "path": "src/ff.py", "line": " 1 ", "replacement": "q", "tests": ["tests"]},
    {"op": "mutation", "path": "src/big.py", "line": 2.9, "replacement": "ü = 2", "tests": ["tests"]},
    {"op": "executor", "spec": {"kind": "explode", "message": "the sandbox could not start"}},
    {"op": "mutation", "path": "src/app.py", "line": 5, "replacement": "    return None", "tests": ["tests/test_app.py"]},
    {"op": "file", "path": "src/app.py"},
    {"op": "close"},
], {"kind": "fixture", "watch": ["src/app.py", "src/crlf.py", "src/last.py", "src/cr.py", "src/ff.py", "src/big.py"]},
    options={"runs": 10}))
sessions.append(session_case("mutation-select-free", FIX, [
    {"op": "mutation", "path": "src/app.py", "line": "+1_0", "replacement": "x", "tests": ["tests"]},
    {"op": "mutation", "path": "src/app.py", "line": 1, "replacement": "x", "tests": ["tests"]},
    {"op": "mutation", "path": "src/app.py", "line": 1, "replacement": "x", "tests": ["tests"]},
], {"kind": "items", "items": [[124, "PASSED tests/test_app.py::test_kinds\n"], [127, "python3: not found"]]},
    options={"runs": 1}))

MONO_TREE = {"HEAD": MONO}
sessions.append(session_case("monorepo", MONO_TREE, [
    {"op": "plan", "paths": MONO_NAMES},
    {"op": "plan", "paths": ["api", "web", "notes/readme.txt", "README.md"]},
    {"op": "compare", "paths": ["web/src/Resources.test.tsx", "api/tests/test_users.py", "packages/ui/src/Button.test.jsx",
                                "packages/ui/src/deep/x.spec.ts"]},
    {"op": "run", "paths": ["web/src/Resources.test.tsx", "api/tests/test_users.py"]},
    {"op": "run", "paths": ["tools/loose.test.ts", "a", "b", "c"]},
    {"op": "run", "paths": ["svc/pkg/a_test.go"], "select": "TestA"},
    {"op": "run", "paths": ["crate/src/lib.rs"], "select": "parse::works"},
    {"op": "run", "paths": ["packages/cli/src/a.test.mjs"]},
], {"kind": "items", "items": [[0, "PASSED tests/test_users.py::test_ok"]]}, head="HEAD", base=None,
    options={"runs": 10}))
sessions.append(session_case("monorepo-go-cargo", MONO_TREE, [
    {"op": "run", "paths": ["svc/pkg/a_test.go"]},
    {"op": "run", "paths": ["crate/src/lib.rs"]},
], {"kind": "items", "items": [[1, "--- FAIL: TestA (0.00s)\n--- PASS: TestB (0.00s)\n"],
                               [101, "test parse::works ... ok\ntest parse::fails ... FAILED\n"]]},
    head="HEAD", base=None))
MAKE = {"HEAD2": {"Makefile": "test:\n\t@test \"$$(cat value.txt)\" = \"1\"\n", "value.txt": "2\n"},
        "BASE2": {"Makefile": "test:\n\t@test \"$$(cat value.txt)\" = \"1\"\n", "value.txt": "1\n"}}
sessions.append(session_case("make-regression", MAKE, [{"op": "compare", "paths": ["value.txt", "Makefile"]}],
                             {"kind": "make"}, head="HEAD2", base="BASE2"))
many = "\n".join(f"PASSED tests/test_app.py::t{i}" for i in range(70))
sessions.append(session_case("output", FIX, [
    {"op": "run", "paths": ["tests/test_app.py"]},
    {"op": "run", "paths": ["tests/test_app.py"]},
    {"op": "run", "paths": ["tests/test_app.py"]},
    {"op": "run", "paths": ["tests/test_app.py"]},
    {"op": "run", "paths": ["tests/test_app.py"]},
], {"kind": "items", "items": [
    [1, many + "\nFAILED tests/test_app.py::bad - in {cwd}/src/app.py\nERROR tests/test_app.py::err\n" +
     "\n".join(f"line {i} of the run" for i in range(400)) + "\n"],
    [1, "FAILED tests/test_app.py::bad - wrote {cwd}/out.txt\nsee {cwd}\n"],
    [124, "PASSED tests/test_app.py::a\nslow\n"],
    [1, "ModuleNotFoundError: No module named 'requests'\n"],
    [0, ""]]}, options={"timeout_s": 7.5, "env_kind": "prepared"}))

# Environments from a local checkout.
CHECKOUT_OPTS = {"checkout": "co"}
sessions.append(session_case("checkout-pytest", FIX, [
    {"op": "run", "paths": ["tests/test_app.py"]}], FIXTURE, options=CHECKOUT_OPTS))
JS = {"HEAD": {"web/package.json": json.dumps({"devDependencies": {"vitest": "1"}}), "web/src/a.test.ts": "",
               "package.json": json.dumps({"scripts": {"test": "jest"}}), "b.test.js": "",
               "pkg/package.json": json.dumps({"scripts": {"test": "node t.js"}}), "pkg/c.test.js": ""}}
sessions.append(session_case("checkout-vitest", JS, [
    {"op": "run", "paths": ["web/src/a.test.ts"]}, {"op": "run", "paths": ["b.test.js"], "select": "adds"},
    {"op": "run", "paths": ["pkg/c.test.js"]}],
    {"kind": "items", "items": [[0, ""]]}, head="HEAD", base=None, options=CHECKOUT_OPTS))
sessions.append(session_case("checkout-node", JS, [
    {"op": "run", "paths": ["web/src/a.test.ts"]}, {"op": "run", "paths": ["pkg/c.test.js"]}],
    {"kind": "items", "items": [[0, ""]]}, head="HEAD", base=None, options=CHECKOUT_OPTS, node=NODE + "/bin/node"))
sessions.append(session_case("checkout-node-toolchain", JS, [{"op": "run", "paths": ["web/src/a.test.ts"]}],
                             {"kind": "items", "items": [[0, ""]]}, head="HEAD", base=None,
                             options=dict(CHECKOUT_OPTS, toolchain=["tc/node/20", "tc/python/3.12"]),
                             node=NODE + "/bin/node"))
sessions.append(session_case("checkout-system-node", JS, [{"op": "run", "paths": ["web/src/a.test.ts"]}],
                             {"kind": "items", "items": [[0, ""]]}, head="HEAD", base=None, options=CHECKOUT_OPTS,
                             node="fakehome/bin/python3"))
sessions.append(session_case("checkout-managed", FIX, [{"op": "run", "paths": ["tests/test_app.py"]}], FIXTURE,
                             options={"checkout": "managed"}))
sessions.append(session_case("checkout-override", FIX, [{"op": "run", "paths": ["tests/test_app.py"]}], FIXTURE,
                             options={"checkout": "co", "env_dir": "co"}))
sessions.append(session_case("checkout-unshare", FIX, [{"op": "run", "paths": ["tests/test_app.py"]}], FIXTURE,
                             mechanism="unshare", options=CHECKOUT_OPTS))
sessions.append(session_case("checkout-docker", JS, [{"op": "run", "paths": ["web/src/a.test.ts"]}],
                             {"kind": "items", "items": [[0, ""]]}, mechanism="docker", head="HEAD", base=None,
                             options=CHECKOUT_OPTS))
sessions.append(session_case("checkout-missing", FIX, [{"op": "run", "paths": ["tests/test_app.py"]}], FIXTURE,
                             options={"checkout": "nope", "env_dir": "nope2"}))
out["sessions"] = sessions

CRAFTED = [
    {"n": 3, "kind": "mutation", "rev": "head", "runner": "pytest", "cwd": ".", "command": "python3 -m pytest x",
     "paths": ["x"], "status": "timeout", "exit": 124, "tests": [], "passed": 0, "failed": 0, "ms": 5,
     "environment": [], "toolchain": [], "output": "a\nb\n", "mutation": {"path": "a.py", "line": 2, "was": "x",
                                                                          "replacement": "y"}, "caught": False},
    {"n": 4, "kind": "run", "rev": "base", "runner": "go", "cwd": "svc", "command": "go test ./pkg", "paths": ["p"],
     "status": "failed", "exit": 1, "tests": [{"id": f"svc/T{i}", "status": "failed" if i % 2 else "error",
                                               **({"detail": f"d{i}"} if i % 3 == 0 else {})} for i in range(12)]
     + [{"id": "svc/ok", "status": "passed"}], "passed": 1, "failed": 12, "ms": 0, "environment": [],
     "toolchain": [], "output": "\n".join(f"o{i}" for i in range(40)), "mutation": None, "caught": None},
    {"n": 5, "kind": "automatic", "rev": "head", "runner": "make", "cwd": ".", "command": "make test", "paths": [],
     "status": "passed", "exit": 0, "tests": [], "passed": 0, "failed": 0, "ms": 1, "environment": [],
     "toolchain": [], "output": "fine\n"},
    {"n": 6, "kind": "mutation", "rev": "head", "runner": "pytest", "cwd": ".", "command": "c", "paths": [],
     "status": "unavailable", "exit": 1, "tests": [{"id": "x", "status": "passed", "detail": ""}], "passed": 1,
     "failed": 0, "ms": 1, "environment": [], "toolchain": [], "output": "x\r\ny\u2028z",
     "mutation": {"path": "a.py", "line": 1, "was": "", "replacement": ""}, "caught": False},
]
out["summaries"] = [{"record": {k: v for k, v in record.items() if v is not None}, "text": t.summarize(record)}
                    for record in CRAFTED]

# --- Real processes -------------------------------------------------------------------------------------------

ENVP = {"PATH": FIXED_PATH}
PROCESSES = [
    [["sh", "-c", "echo out; echo err >&2; exit 3"], ROOT, ENVP],
    [["definitely-not-a-command-xyz"], ROOT, ENVP],
    [["sh", "-c", "printf 'caf\\303\\251 \\377 end'"], ROOT, ENVP],
    [["sh", "-c", "head -c 450000 /dev/zero | tr '\\0' a; echo END"], ROOT, ENVP],
    [["sh", "-c", "echo $FOO; echo ${HOME:-unset}; pwd"], ROOT, {"FOO": "bar", "PATH": FIXED_PATH}],
    [["sh", "-c", "echo x"], os.path.join(ROOT, "no-such-dir"), ENVP],
    [["sh", "-c", "read line; echo got=$line"], ROOT, ENVP],
]
out["processes"] = []
for argv, cwd, env in PROCESSES:
    code, text = t.run_process(argv, cwd, env, 60)
    out["processes"].append({"argv": argv, "cwd": sub(cwd), "env": env, "code": code, "length": len(text.replace(ROOT, "<root>")),
                             "output": sub(text) if len(text) < 1000 else sub(text[-200:])})

# --- A real export --------------------------------------------------------------------------------------------

GIT_ENV = {"PATH": FIXED_PATH, "HOME": HOME, "GIT_CONFIG_GLOBAL": "/dev/null", "GIT_CONFIG_NOSYSTEM": "1",
           "GIT_AUTHOR_NAME": "Review Test", "GIT_AUTHOR_EMAIL": "review@test", "GIT_COMMITTER_NAME": "Review Test",
           "GIT_COMMITTER_EMAIL": "review@test", "GIT_AUTHOR_DATE": "2026-01-01T00:00:00Z",
           "GIT_COMMITTER_DATE": "2026-01-01T00:00:00Z"}
REPO_STEPS = [
    {"files": {"a.txt": "one\n", "sub/b.py": "print(1)\n", ".gitattributes": "skip.txt export-ignore\n",
               "skip.txt": "ignored\n", "run.sh": "#!/bin/sh\n"}, "exec": ["run.sh"], "message": "base"},
    {"files": {"a.txt": "two\n", "sub/c.py": "x\n"}, "remove": ["sub/b.py"], "message": "head"},
]
repo = os.path.join(ROOT, "repo")
os.makedirs(repo)
git = lambda *args: subprocess.run(["git", *args], cwd=repo, env=GIT_ENV, check=True, capture_output=True,  # noqa: E731
                                   text=True).stdout.strip()
git("init", "-q", "-b", "main")
for step in REPO_STEPS:
    for path, text in step["files"].items():
        os.makedirs(os.path.dirname(os.path.join(repo, path)), exist_ok=True)
        with open(os.path.join(repo, path), "w", encoding="utf-8") as handle:
            handle.write(text)
    for path in step.get("exec", []):
        os.chmod(os.path.join(repo, path), 0o755)
    for path in step.get("remove", []):
        os.unlink(os.path.join(repo, path))
    git("add", "-A")
    git("commit", "-qm", step["message"])


def tree_of(path):
    files = {}
    for current, dirs, names in os.walk(path):
        dirs.sort()
        for name in sorted(names):
            full = os.path.join(current, name)
            files[os.path.relpath(full, path)] = [open(full, encoding="utf-8").read(), os.access(full, os.X_OK)]
    return files


exports = []
os.environ.update({"GIT_CONFIG_GLOBAL": "/dev/null", "GIT_CONFIG_NOSYSTEM": "1"})
for rev in ["HEAD", "HEAD~1", "deadbeefdeadbeefdeadbeef"]:
    try:
        target = t.export_commit(repo, rev)
        exports.append({"rev": rev, "tree": tree_of(target)})
        shutil.rmtree(target)
    except RuntimeError as error:
        exports.append({"rev": rev, "error": str(error)})
out["export"] = {"steps": REPO_STEPS, "exports": exports}

shutil.rmtree(ROOT)
# Every temp path left (a home inside an argv, say) is recorded as <root>.
print(json.dumps(out, indent=1).replace(ROOT, "<root>"))
