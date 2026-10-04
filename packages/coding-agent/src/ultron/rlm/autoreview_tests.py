"""Running the reviewed project's tests for `ultron autoreview`, sandboxed.

The deep pass (autoreview_deep.py) may run tests: once automatically, for the test files the map tied to the
change, and on an investigator's request (`run_tests`, `mutation_check`). Models still have no shell: these are
two more closed request types the host serves.

Every execution happens
- in a disposable export of the commit (`git archive` into a temporary directory), never in the user's checkout;
- inside a sandbox: no network, no credentials (the environment is built from scratch), an empty HOME, the export
  as the only writable directory, and nothing of the user's real home, agent directory, other repositories or the
  Docker socket visible. Mechanisms, strongest first: bubblewrap, rootless `unshare` namespaces, Docker (only
  with a configured local image). With none available tests are not run at all;
- under a wall-clock limit, with capped output, at most `runs` times per review.

There is no network in the sandbox and nothing is installed: when the project's dependencies are missing the
outcome is "unavailable" (tests could not run), not a failure.

`python3 autoreview_tests.py doctor` prints the mechanism and the result of a self-check as JSON.
"""
from __future__ import annotations

import json
import os
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import uuid
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Callable

DEFAULT_RUNS = 6
DEFAULT_TIMEOUT_S = 300
MAX_OUTPUT_BYTES = 400_000
OUTPUT_TAIL_CHARS = 2_000
MAX_PATHS = 12
MAX_SELECT_CHARS = 120
MAX_REPLACEMENT_CHARS = 300
SANDBOX_HOME = "/tmp/home"
SANDBOX_PATH = "/usr/local/bin:/usr/bin:/bin"

#: (argv, cwd, env, timeout seconds) -> (exit code, combined output); 124 on timeout, 127 when not found.
Executor = Callable[[list[str], str, dict[str, str], float], "tuple[int, str]"]


class TestsRejected(Exception):
    """A test request the host will not run; the message goes back to the investigator."""


# --- Sandbox ---------------------------------------------------------------------------------------------------

_SYSTEM_DIRS = ("/usr", "/bin", "/sbin", "/lib", "/lib64", "/lib32")
_ETC = ("ld.so.cache", "ld.so.conf", "ld.so.conf.d", "passwd", "group", "nsswitch.conf", "localtime", "alternatives",
        "ssl", "ca-certificates", "mime.types", "os-release")
#: What `unshare` hides by mounting an empty tmpfs over it (bubblewrap starts from an empty root instead).
_HIDDEN = ("/home", "/root", "/run", "/media", "/srv", "/var/run", "/var/lib/docker")

_UNSHARE_SCRIPT = """set -e
work="$1"; shift
mount --bind "$work" /mnt
mount -t tmpfs tmpfs /tmp
while [ "$1" != "--" ]; do if [ -d "$1" ]; then mount -t tmpfs tmpfs "$1"; fi; shift; done
shift
mkdir -p "$work" /tmp/home
mount --move /mnt "$work" 2>/dev/null || mount --bind /mnt "$work"
cd "$work"
exec "$@"
"""


def sandbox_env(extra_path: str | None = None) -> dict[str, str]:
    """The whole environment of a sandboxed command, built from scratch: nothing of the caller's is inherited,
    so no token, key, cloud or SSH variable can be in it."""
    return {
        "PATH": (f"{extra_path}:" if extra_path else "") + SANDBOX_PATH,
        "HOME": SANDBOX_HOME,
        "TMPDIR": "/tmp",
        "LANG": "C.UTF-8",
        "TERM": "dumb",
        "NO_COLOR": "1",
        "CI": "1",
        "PYTHONDONTWRITEBYTECODE": "1",
        "PYTHONHASHSEED": "0",
    }


@dataclass
class Sandbox:
    """How commands are isolated: `mechanism` is "bwrap", "unshare" or "docker"."""
    mechanism: str
    image: str | None = None
    home: str = field(default_factory=lambda: os.path.expanduser("~"))

    def wrap(self, command: list[str], workdir: str, env: dict[str, str], ro_binds: list[str] | None = None) -> list[str]:
        """The full command line that runs `command` in `workdir` inside the sandbox."""
        binds = [path for path in (ro_binds or []) if path]
        if self.mechanism == "bwrap":
            argv = ["bwrap", "--unshare-all", "--die-with-parent", "--new-session", "--cap-drop", "ALL", "--clearenv"]
            for name, value in env.items():
                argv += ["--setenv", name, value]
            for path in _SYSTEM_DIRS:
                if os.path.islink(path):
                    argv += ["--symlink", os.readlink(path), path]
                elif os.path.isdir(path):
                    argv += ["--ro-bind", path, path]
            for name in _ETC:
                argv += ["--ro-bind-try", f"/etc/{name}", f"/etc/{name}"]
            argv += ["--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp", "--dir", SANDBOX_HOME]
            for path in binds:
                argv += ["--ro-bind", path, path]
            return [*argv, "--bind", workdir, workdir, "--chdir", workdir, "--", *command]
        if self.mechanism == "unshare":
            hidden = list(dict.fromkeys([*_HIDDEN, self.home]))
            return ["unshare", "--user", "--map-root-user", "--mount", "--net", "--pid", "--fork", "--ipc", "--uts",
                    "--kill-child", "env", "-i", *(f"{name}={value}" for name, value in env.items()),
                    "sh", "-c", _UNSHARE_SCRIPT, "sandbox", workdir, *hidden, "--", *command]
        if self.mechanism == "docker":
            argv = ["docker", "run", "--rm", "--network", "none", "--cap-drop", "ALL", "--security-opt",
                    "no-new-privileges", "--pids-limit", "1024", "--user", f"{os.getuid()}:{os.getgid()}",
                    "--tmpfs", "/tmp", "-v", f"{workdir}:{workdir}:rw", "-w", workdir]
            for path in binds:
                argv += ["-v", f"{path}:{path}:ro"]
            for name, value in env.items():
                argv += ["-e", f"{name}={value}"]
            return [*argv, str(self.image), *command]
        raise ValueError(f"unknown sandbox mechanism {self.mechanism}")

    def describe(self) -> str:
        return {
            "bwrap": "bubblewrap: new user, mount, pid, ipc, uts, cgroup and network namespaces; an empty root with "
                     "the system directories read-only, a private /tmp and HOME, and the exported commit as the only "
                     "writable directory; all capabilities dropped; environment cleared",
            "unshare": "unshare: new user, mount, pid, ipc, uts and network namespaces; /home, /root, /run and the "
                       "user's home hidden under empty tmpfs mounts, a private /tmp; environment cleared",
            "docker": f"docker ({self.image}): no network, all capabilities dropped, no new privileges, only the "
                      "exported commit mounted",
        }[self.mechanism]


def run_process(argv: list[str], cwd: str, env: dict[str, str], timeout: float) -> tuple[int, str]:
    """Run to completion in its own process group; kill the group on timeout; output capped."""
    try:
        process = subprocess.Popen(argv, cwd=cwd, env=env, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                                   stderr=subprocess.STDOUT, start_new_session=True)
    except FileNotFoundError:
        return 127, f"{argv[0]}: not found"
    try:
        output, _ = process.communicate(timeout=timeout)
        code = process.returncode
    except subprocess.TimeoutExpired:
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except OSError:
            process.kill()
        output, _ = process.communicate()
        code = 124
    text = output[-MAX_OUTPUT_BYTES:].decode("utf-8", "replace")
    return code, text


def _works(sandbox: Sandbox, executor: Executor) -> bool:
    probe = tempfile.mkdtemp(prefix="ultron-autoreview-probe-")
    try:
        code, _ = executor(sandbox.wrap(["true"], probe, sandbox_env()), probe, _launch_env(), 30)
        return code == 0
    finally:
        shutil.rmtree(probe, ignore_errors=True)


def _launch_env() -> dict[str, str]:
    """The environment of the sandbox launcher itself (bwrap, unshare, docker): a PATH and nothing else of ours."""
    return {"PATH": os.environ.get("PATH", SANDBOX_PATH), **({"DOCKER_HOST": os.environ["DOCKER_HOST"]}
                                                           if "DOCKER_HOST" in os.environ else {})}


def detect_sandbox(*, image: str | None = None, which: Callable[[str], str | None] = shutil.which,
                   executor: Executor = run_process, only: str | None = None) -> Sandbox | None:
    """The strongest sandbox that works here: bubblewrap, then unshare, then Docker (with a local image). None
    when there is none: tests are then not run."""
    for mechanism in ("bwrap", "unshare", "docker"):
        if only is not None and mechanism != only:
            continue
        if which(mechanism) is None or (mechanism == "docker" and not image):
            continue
        sandbox = Sandbox(mechanism, image if mechanism == "docker" else None)
        if _works(sandbox, executor):
            return sandbox
    return None


_SELF_CHECK = r"""
import os, socket, sys
out = {}
try:
    socket.create_connection(("1.1.1.1", 53), 2).close()
    out["network"] = "reachable"
except OSError:
    out["network"] = "unreachable"
out["canary"] = "readable" if os.path.exists(sys.argv[1]) else "unreadable"
out["token"] = "present" if any("CANARY_TOKEN" in name for name in os.environ) else "absent"
leaked = [name for name in os.environ if any(word in name.upper() for word in ("TOKEN", "SECRET", "KEY", "PASSWORD", "AWS_", "SSH_", "GH_", "GITHUB"))]
out["credentialVariables"] = leaked
out["home"] = os.environ.get("HOME")
try:
    open("probe.txt", "w").write("x")
    out["workdir"] = "writable"
except OSError:
    out["workdir"] = "read-only"
try:
    open("/usr/ultron-autoreview-probe", "w").write("x")
    out["system"] = "writable"
except OSError:
    out["system"] = "read-only"
out["dockerSocket"] = "visible" if os.path.exists("/var/run/docker.sock") or os.path.exists("/run/docker.sock") else "hidden"
import json; print("SELFCHECK " + json.dumps(out))
"""


def self_check(sandbox: Sandbox, executor: Executor = run_process) -> dict[str, Any]:
    """Run the isolation checks inside the sandbox: the network must be unreachable, a canary file in the real
    HOME unreadable, a token-like variable of the caller absent."""
    home = os.path.expanduser("~")
    canary = os.path.join(home, f".ultron-autoreview-canary-{uuid.uuid4().hex[:8]}")
    workdir = tempfile.mkdtemp(prefix="ultron-autoreview-check-")
    launch = {**_launch_env(), "ULTRON_AUTOREVIEW_CANARY_TOKEN": "not-a-real-token"}
    try:
        Path(canary).write_text("canary\n", encoding="utf-8")
        code, output = executor(sandbox.wrap(["python3", "-c", _SELF_CHECK, canary], workdir, sandbox_env()), workdir,
                                launch, 60)
        line = next((item for item in output.splitlines() if item.startswith("SELFCHECK ")), None)
        if line is None:
            return {"ok": False, "error": f"the self-check did not run (exit {code}): {output[-300:]}"}
        result = json.loads(line[len("SELFCHECK "):])
        result["ok"] = (result["network"] == "unreachable" and result["canary"] == "unreadable"
                        and result["token"] == "absent" and not result["credentialVariables"]
                        and result["workdir"] == "writable" and result["system"] == "read-only"
                        and result["dockerSocket"] == "hidden" and result["home"] == SANDBOX_HOME)
        return result
    finally:
        try:
            os.unlink(canary)
        except OSError:
            pass
        shutil.rmtree(workdir, ignore_errors=True)


def doctor(image: str | None = None) -> dict[str, Any]:
    sandbox = detect_sandbox(image=image)
    if sandbox is None:
        return {"mechanism": None, "ok": False,
                "message": "tests are not run: no sandbox is available (install bubblewrap, or allow unprivileged "
                           "user namespaces, or set autoreview.testImage for Docker)"}
    check = self_check(sandbox)
    return {"mechanism": sandbox.mechanism, "isolation": sandbox.describe(), "selfCheck": check,
            "ok": bool(check.get("ok"))}


# --- Test runners ----------------------------------------------------------------------------------------------


@dataclass
class TestRunner:
    name: str
    #: The files that made the choice, for the record.
    because: str

    def command(self, paths: list[str], select: str | None) -> list[str]:
        if self.name == "pytest":
            return ["python3", "-m", "pytest", "-q", "-rA", "--no-header", "-p", "no:cacheprovider", "--tb=short",
                    *(["-k", select] if select else []), *paths]
        if self.name in ("vitest", "jest"):
            run = ["run"] if self.name == "vitest" else []
            return ["npx", "--no-install", self.name, *run, *(["-t", select] if select else []), *paths]
        if self.name in ("npm", "pnpm", "yarn"):
            return [self.name, "test", *(["--", *paths] if paths and self.name != "yarn" else paths)]
        if self.name == "go":
            dirs = list(dict.fromkeys("./" + (os.path.dirname(path) or ".") for path in paths)) or ["./..."]
            return ["go", "test", *(["-run", select] if select else []), *dirs]
        if self.name == "cargo":
            return ["cargo", "test", "--offline", *([select] if select else [])]
        return ["make", "test"]


def detect_runner(tracked: list[str] | set[str], read: Callable[[str], str | None]) -> TestRunner | None:
    """The project's test runner, from its tracked files; None when none is recognized."""
    files = set(tracked)

    def has(name: str, needle: str) -> bool:
        return name in files and needle in (read(name) or "")

    if "pytest.ini" in files or has("pyproject.toml", "[tool.pytest") or has("tox.ini", "pytest") or has(
            "setup.cfg", "[tool:pytest]") or "conftest.py" in files:
        return TestRunner("pytest", "pytest configuration")
    if "package.json" in files:
        try:
            package = json.loads(read("package.json") or "{}")
        except ValueError:
            package = {}
        deps = {**(package.get("devDependencies") or {}), **(package.get("dependencies") or {})}
        script = str((package.get("scripts") or {}).get("test") or "")
        for name in ("vitest", "jest"):
            if name in deps or name in script:
                return TestRunner(name, f"package.json ({name})")
        if script and "no test specified" not in script:
            manager = "pnpm" if "pnpm-lock.yaml" in files else "yarn" if "yarn.lock" in files else "npm"
            return TestRunner(manager, "package.json scripts.test")
    if "go.mod" in files:
        return TestRunner("go", "go.mod")
    if "Cargo.toml" in files:
        return TestRunner("cargo", "Cargo.toml")
    if re.search(r"^test:", read("Makefile") or "", re.M) if "Makefile" in files else False:
        return TestRunner("make", "Makefile test target")
    if any(re.search(r"(^|/)(test_[^/]+|[^/]+_test)\.py$", path) for path in files):
        return TestRunner("pytest", "python test files")
    return None


_MISSING = re.compile(r"ModuleNotFoundError|No module named|ImportError while (importing|loading)|Cannot find module"
                      r"|command not found|: not found|npm ERR! missing|ERR_MODULE_NOT_FOUND|could not determine executable"
                      r"|no required module provides|cannot find package|error: no matching package|failed to select a version"
                      r"|errors? during collection|Could not resolve dependencies|network is unreachable"
                      r"|Temporary failure in name resolution|getaddrinfo", re.I)
_PYTEST = re.compile(r"^(PASSED|FAILED|ERROR)\s+(\S+)(?:\s+-\s+(.*))?$", re.M)
_GO = re.compile(r"^\s*--- (PASS|FAIL): (\S+)", re.M)
_CARGO = re.compile(r"^test (\S+) \.\.\. (ok|FAILED)", re.M)


def parse_outcome(runner: str, code: int, output: str) -> tuple[str, list[dict[str, str]]]:
    """(status, per-test results) of one run: passed, failed, unavailable (it could not run: dependencies missing,
    nothing collected), or timeout."""
    tests: list[dict[str, str]] = []
    if runner == "pytest":
        tests = [{"id": name, "status": status.lower(), **({"detail": detail[:200]} if detail else {})}
                 for status, name, detail in _PYTEST.findall(output)]
    elif runner == "go":
        tests = [{"id": name, "status": "passed" if status == "PASS" else "failed"} for status, name in _GO.findall(output)]
    elif runner == "cargo":
        tests = [{"id": name, "status": "passed" if status == "ok" else "failed"} for name, status in _CARGO.findall(output)]
    if code == 124:
        return "timeout", tests
    if code == 0:
        return "passed", tests
    failed = [item for item in tests if item["status"] == "failed"]
    # Nothing ran, or it stopped before running: not a verdict on the code.
    if code == 127 or (not failed and _MISSING.search(output)) or (runner == "pytest" and code in (4, 5) and not failed):
        return "unavailable", tests
    return "failed", tests


# --- A review's test session -----------------------------------------------------------------------------------


def export_commit(root: str, rev: str) -> str:
    """A disposable copy of the commit's tree (`git archive` into a temporary directory)."""
    target = tempfile.mkdtemp(prefix="ultron-autoreview-run-")
    env = {"PATH": os.environ.get("PATH", SANDBOX_PATH), "GIT_OPTIONAL_LOCKS": "0", "GIT_TERMINAL_PROMPT": "0"}
    archive = subprocess.Popen(["git", "-C", root, "archive", "--format=tar", rev], stdout=subprocess.PIPE,
                               stderr=subprocess.DEVNULL, stdin=subprocess.DEVNULL, env=env)
    extract = subprocess.run(["tar", "-x", "-C", target], stdin=archive.stdout, capture_output=True, env=env)
    archive.stdout.close()  # type: ignore[union-attr]
    if archive.wait() != 0 or extract.returncode != 0:
        shutil.rmtree(target, ignore_errors=True)
        raise RuntimeError(f"the commit {rev[:12]} could not be exported")
    return target


class TestSession:
    """The test executions of one review: bounded in number and time, each in the sandbox, each in an export."""

    def __init__(self, root: str, head: str, base: str | None, tracked: list[str], read: Callable[[str], str | None],
                 sandbox: Sandbox, *, runs: int = DEFAULT_RUNS, timeout_s: float = DEFAULT_TIMEOUT_S,
                 env_dir: str | None = None, executor: Executor = run_process,
                 export: Callable[[str, str], str] = export_commit, clock: Callable[[], float] = time.monotonic) -> None:
        self.root = root
        self.revs = {"head": head, "base": base}
        self.tracked = set(tracked)
        self.sandbox = sandbox
        self.limit = max(0, runs)
        self.timeout_s = timeout_s
        self.env_dir = env_dir if env_dir and os.path.isdir(env_dir) else None
        self.runner = detect_runner(tracked, read)
        self._executor = executor
        self._export = export
        self._clock = clock
        self._dirs: dict[str, str] = {}
        #: Every execution, in order.
        self.records: list[dict[str, Any]] = []

    def close(self) -> None:
        for path in self._dirs.values():
            shutil.rmtree(path, ignore_errors=True)
        self._dirs.clear()

    def _dir(self, which: str) -> str:
        if which not in self._dirs:
            rev = self.revs[which]
            if not rev:
                raise TestsRejected("the base commit is not known")
            self._dirs[which] = self._export(self.root, rev)
        return self._dirs[which]

    def paths(self, raw: Any) -> list[str]:
        """Existing, tracked test files (or test directories), or TestsRejected."""
        items = raw if isinstance(raw, list) else []
        if not items or len(items) > MAX_PATHS:
            raise TestsRejected(f"give 1 to {MAX_PATHS} test paths")
        out = []
        for item in items:
            if not isinstance(item, str) or not item or item.startswith(("/", "-", "~", ":")) or "\0" in item \
                    or ".." in item.split("/") or "\\" in item:
                raise TestsRejected(f"{item!r} is not a path inside the repository")
            path = os.path.normpath(item)
            if path not in self.tracked and not any(name.startswith(path + "/") for name in self.tracked):
                raise TestsRejected(f"{path} is not a tracked file or directory at the reviewed commit")
            out.append(path)
        return out

    def run(self, paths: list[str], select: str | None = None, *, which: str = "head", kind: str = "run") -> dict[str, Any]:
        """One sandboxed execution of the project's tests on `paths`; counts against the review's limit."""
        if self.runner is None:
            raise TestsRejected("no test runner was recognized in this repository")
        if select is not None and (not isinstance(select, str) or len(select) > MAX_SELECT_CHARS
                                   or not re.fullmatch(r"[\w .:\[\]()/,=-]+", select)):
            raise TestsRejected("select must be a short test name expression")
        if len(self.records) >= self.limit:
            raise TestsRejected(f"the limit of {self.limit} test executions per review is reached")
        workdir = self._dir(which)
        command = self.runner.command(paths, select)
        extra = os.path.join(self.env_dir, "bin") if self.env_dir and os.path.isdir(os.path.join(self.env_dir, "bin")) else None
        env = sandbox_env(extra)
        if self.env_dir and extra is None:
            env["NODE_PATH"] = self.env_dir
        argv = self.sandbox.wrap(command, workdir, env, [self.env_dir] if self.env_dir else [])
        began = self._clock()
        code, output = self._executor(argv, workdir, _launch_env(), self.timeout_s)
        status, tests = parse_outcome(self.runner.name, code, output)
        record = {"n": len(self.records) + 1, "kind": kind, "rev": which, "command": " ".join(command),
                  "paths": paths, "status": status, "exit": code, "tests": tests[:60],
                  "passed": sum(1 for item in tests if item["status"] == "passed"),
                  "failed": sum(1 for item in tests if item["status"] in ("failed", "error")),
                  "ms": int((self._clock() - began) * 1000),
                  "output": output[-OUTPUT_TAIL_CHARS:].replace(workdir, ".")}
        self.records.append(record)
        return record

    def mutation(self, raw_path: Any, line: Any, replacement: Any, tests: Any) -> dict[str, Any]:
        """Replace one line of a tracked source file in the head export, run `tests`, and restore the file."""
        [path] = self.paths([raw_path]) if isinstance(raw_path, str) else [None]
        if path is None or path not in self.tracked:
            raise TestsRejected("mutation_check needs the path of a tracked file")
        if not isinstance(replacement, str) or "\n" in replacement or "\0" in replacement or len(replacement) > MAX_REPLACEMENT_CHARS:
            raise TestsRejected(f"replacement must be one line of at most {MAX_REPLACEMENT_CHARS} characters")
        test_paths = self.paths(tests)
        if len(self.records) >= self.limit:
            raise TestsRejected(f"the limit of {self.limit} test executions per review is reached")
        target = Path(self._dir("head"), path)
        original = target.read_bytes()
        lines = original.decode("utf-8", "replace").splitlines(keepends=True)
        try:
            number = int(line)
        except (TypeError, ValueError):
            raise TestsRejected("mutation_check needs a line number") from None
        if b"\0" in original[:8192] or not 1 <= number <= len(lines):
            raise TestsRejected(f"{path} has no line {line}")
        ending = "\r\n" if lines[number - 1].endswith("\r\n") else "\n" if lines[number - 1].endswith("\n") else ""
        was = lines[number - 1].rstrip("\r\n")
        lines[number - 1] = replacement + ending
        try:
            target.write_text("".join(lines), encoding="utf-8")
            record = self.run(test_paths, kind="mutation")
        finally:
            target.write_bytes(original)
        record["mutation"] = {"path": path, "line": number, "was": was[:200], "replacement": replacement}
        # A mutant the tests do not notice is the evidence: nothing pins that line.
        record["caught"] = record["status"] == "failed"
        return record

    def compare(self, paths: list[str]) -> dict[str, Any]:
        """Run `paths` at the head commit and, when anything fails, at the base commit too: a test that fails at
        head and passed at base is a regression; one that failed before is not this change's."""
        head = self.run(paths, kind="automatic")
        out: dict[str, Any] = {"head": head, "base": None, "regressions": []}
        failing = [item for item in head["tests"] if item["status"] in ("failed", "error")]
        if head["status"] != "failed" or not self.revs["base"] or len(self.records) >= self.limit:
            return out
        existing = [path for path in paths]
        try:
            base = self.run(existing, which="base", kind="base")
        except (TestsRejected, RuntimeError):
            return out
        out["base"] = base
        passed_before = {item["id"] for item in base["tests"] if item["status"] == "passed"}
        out["regressions"] = [item for item in failing if item["id"] in passed_before]
        if base["status"] == "passed" and not head["tests"]:
            # A runner without per-test results: the selection as a whole regressed.
            out["regressions"] = [{"id": " ".join(paths), "status": "failed"}]
        return out


def summarize(record: dict[str, Any]) -> str:
    """One run for an investigator or the brief: what ran, the outcome, the failing tests, the trimmed output."""
    lines = [f"run {record['n']} ({record['kind']}, at the {record['rev']} commit, sandboxed, no network): "
             f"`{record['command']}` -> {record['status']}"
             + (f" ({record['passed']} passed, {record['failed']} failed)" if record["tests"] else "")]
    if record.get("mutation"):
        change = record["mutation"]
        lines.append(f"  with {change['path']}:{change['line']} changed from `{change['was']}` to "
                     f"`{change['replacement']}`: " + ("the tests caught it" if record["caught"] else
                                                       "the tests still pass (nothing pins this line)"
                                                       if record["status"] == "passed" else "no verdict"))
    if record["status"] == "unavailable":
        lines.append("  the tests could not run (missing dependencies or nothing collected; the sandbox has no "
                     "network and nothing is installed): this says nothing about the code")
    for item in [item for item in record["tests"] if item["status"] != "passed"][:8]:
        lines.append(f"  {item['status']}: {item['id']}" + (f" - {item['detail']}" if item.get("detail") else ""))
    if record["status"] in ("failed", "timeout", "unavailable"):
        tail = "\n".join("    " + line for line in record["output"].splitlines()[-25:])
        lines.append("  output (end):\n" + tail)
    return "\n".join(lines)


if __name__ == "__main__":
    if sys.argv[1:2] == ["doctor"]:
        print(json.dumps(doctor(sys.argv[2] if len(sys.argv) > 2 else None)))
    else:
        print("usage: autoreview_tests.py doctor [docker-image]", file=sys.stderr)
        sys.exit(2)
