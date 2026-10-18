// Records loki.py's `hook`, `protect` rule packs, `scan`, `slop` and `init` behavior
// for the Rust differential tests (tests/hook_differential.rs).
// Run: node crates/loki/fixtures/generate-hook.mjs  (writes hook-corpus.json next to this file)
// Python: $ULTRON_PYTHON, else /usr/bin/python3. Deterministic: run twice and compare sha256.
// Every case runs in a fresh temp dir with HOME there and PATH limited to a bin dir of
// git/sh/python3 (node only when asked) plus the case's fake tools, so results do
// not depend on what the machine has installed.
import { spawn, spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { chmodSync, lstatSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, readlinkSync, realpathSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const engine = join(here, "../../../packages/coding-agent/src/ultron/loki-engine/loki.py");
const python = process.env.ULTRON_PYTHON || "/usr/bin/python3";

const tokens = {
	AWS: ["AKIA", "Q7XRJ2M4PL3ZWNVB"],
	GH: ["ghp_", "a8Kd93LmQz7Xw2Pv5Rt1Yb6Nc4Hj0Ge9Fs3D"],
};
const expand = (text) => text.replace(/@@(\w+)@@/g, (_, name) => tokens[name].join(""));

// --- fake tools (identical scripts are written by the Rust replay) ---------
const RUFF = `#!/usr/bin/env python3
import json, sys
args = sys.argv[1:]
if "--output-format" in args:
    text = sys.stdin.read()
    out = []
    for i, line in enumerate(text.splitlines(), 1):
        if "BADX" in line:
            out.append({"code": "X100", "message": "bad thing here", "location": {"row": i, "column": 1}})
        if "SYNTAXERR" in line:
            out.append({"code": None, "message": "SyntaxError: nope", "location": {"row": i, "column": 1}})
    print(json.dumps(out))
    sys.exit(1 if out else 0)
files = [a for a in args if a.endswith(".py")]
bad = []
for name in files:
    text = open(name).read()
    if args[0] == "check" and "BADX" in text:
        bad.append(name + ":1:1: X100 bad thing here")
    if args[0] == "format" and "UNFORMATTED" in text:
        bad.append("Would reformat: " + name)
print("\\n".join(bad))
sys.exit(1 if bad else 0)
`;
const MYPY = `#!/usr/bin/env python3
import sys
args = sys.argv[1:]
shadows = {}
i = 0
while i < len(args):
    if args[i] == "--shadow-file":
        shadows[args[i + 1]] = args[i + 2]
        i += 3
        continue
    i += 1
found = []
for name in [a for a in args if a.endswith(".py") and not a.startswith("-")]:
    if name in shadows.values():
        continue
    text = open(shadows.get(name, name)).read()
    for n, line in enumerate(text.splitlines(), 1):
        if "MYPYBAD" in line:
            found.append(f"{name}:{n}: error: Incompatible types in assignment  [assignment]")
print("\\n".join(found))
sys.exit(1 if found else 0)
`;
const OXLINT = `#!/usr/bin/env python3
import json, os, sys
args = sys.argv[1:]
if "json" in args:
    files = args[args.index("json") + 1:]
    diags = []
    for name in files:
        data = open(name, encoding="utf-8").read()
        offset = 0
        for n, line in enumerate(data.splitlines(keepends=True), 1):
            col = line.find("BADJS")
            if col >= 0:
                start = offset + len(line[:col].encode())
                diags.append({"filename": os.path.relpath(name), "code": "eslint(no-debugger)", "message": "debugger-ish statement", "labels": [{"span": {"offset": start, "length": 5, "line": n, "column": col + 1}}]})
            offset += len(line.encode())
    print(json.dumps({"number_of_files": len(files), "number_of_rules": 11, "diagnostics": diags}))
    sys.exit(1 if diags else 0)
name = args[-1]
errors = 0
for n, line in enumerate(open(name).read().splitlines(), 1):
    if "BADJS" in line:
        print(f"{name}:{n}:1: Unexpected BADJS [Error/eslint(no-debugger)]")
        errors += 1
    if "console.log" in line:
        print(f"{name}:{n}:1: Unexpected console [Warning/eslint(no-console)]")
print(f"Found {errors} errors")
sys.exit(1 if errors else 0)
`;
const RUSTFMT = `#!/usr/bin/env python3
import sys
if "BADFMT" in open(sys.argv[1]).read():
    print("error: unformatted " + sys.argv[1], file=sys.stderr)
    sys.exit(1)
`;
const CARGO = `#!/usr/bin/env python3
import glob, json, sys
args = sys.argv[1:]
json_mode = "--message-format=json" in args
found = 0
for name in sorted(glob.glob("src/**/*.rs", recursive=True)):
    for n, line in enumerate(open(name).read().splitlines(), 1):
        if "CLIPPYBAD" in line:
            found += 1
            if json_mode:
                print(json.dumps({"reason": "compiler-message", "message": {"level": "warning", "message": "used unwrap() on a Result value", "code": {"code": "clippy::unwrap_used"}, "spans": [{"is_primary": True, "file_name": name, "line_start": n}]}}))
            else:
                print(f"warning: used unwrap() at {name}:{n}")
if json_mode:
    print(json.dumps({"reason": "build-finished", "success": True}))
    sys.exit(0)
sys.exit(1 if found else 0)
`;

// A stand-in Mix: format fails on UNFORMATTED, Credo reports CREDOBAD lines, compile fails on WARNME, Sobelow
// reports SOBELOW_HIGH / SOBELOW_LOW lines (NOTJSON: not a report), Dialyzer passes.
const MIX = `#!/usr/bin/env python3
import glob, json, sys
args = sys.argv[1:]
command, rest = args[0], args[1:]
files = [a for a in rest if a.endswith((".ex", ".exs"))] or sorted(glob.glob("lib/**/*.ex", recursive=True))
def lines(name):
    return open(name).read().splitlines()
if command == "format":
    bad = [f for f in files if "UNFORMATTED" in open(f).read()]
    if bad:
        print("** (Mix) mix format failed due to --check-formatted.", file=sys.stderr)
        for f in bad:
            print("  * " + f, file=sys.stderr)
        sys.exit(1)
    sys.exit(0)
if command == "credo":
    issues = [{"check": "Credo.Check.Readability.Bad", "filename": f, "line_no": n, "message": "bad code"} for f in files for n, line in enumerate(lines(f), 1) if "CREDOBAD" in line]
    print(json.dumps({"issues": issues}))
    sys.exit(2 if issues else 0)
if command == "compile":
    if any("WARNME" in open(f).read() for f in files):
        print("warning: variable x is unused", file=sys.stderr)
        sys.exit(1)
    print("Compiling 1 file (.ex)")
    sys.exit(0)
if command == "sobelow":
    if any("NOTJSON" in open(f).read() for f in files):
        print("Sobelow crashed")
        sys.exit(0)
    high = [{"type": "XSS.Raw: XSS", "file": f, "line": n, "variable": "x"} for f in files for n, line in enumerate(lines(f), 1) if "SOBELOW_HIGH" in line]
    print(json.dumps({"findings": {"high_confidence": high, "medium_confidence": [], "low_confidence": []}}))
    sys.exit(0)
print("done: " + command)
`;
// A stand-in \`elixir -pa ... -e <runner> -- <args>\` for the compiled Credo and Sobelow the hook runs.
const ELIXIR = `#!/usr/bin/env python3
import glob, json, sys
args = sys.argv[1:]
runner = args[args.index("-e") + 1]
files = sorted(glob.glob("lib/**/*.ex", recursive=True))
def lines(name):
    return open(name).read().splitlines()
if "Credo" in runner:
    issues = [{"check": "Credo.Check.Readability.Bad", "filename": f, "line_no": n, "message": "bad code"} for f in files for n, line in enumerate(lines(f), 1) if "CREDOBAD" in line]
    print(json.dumps({"issues": issues}))
    sys.exit(2 if issues else 0)
groups = {"high_confidence": [], "medium_confidence": [], "low_confidence": []}
for f in files:
    for n, line in enumerate(lines(f), 1):
        for level in ("high", "medium", "low"):
            if "SOBELOW_" + level.upper() in line:
                groups[level + "_confidence"].append({"type": "XSS.Raw: XSS", "file": f, "line": n, "variable": "x"})
if any("SOBELOW_PARTIAL" in open(f).read() for f in files):
    print("some files were skipped", file=sys.stderr)
print(json.dumps({"findings": groups}))
`;

// --- helpers ----------------------------------------------------------------
const cfg = (value) => ({ ".loki/loki.json": JSON.stringify(value) });
const hook = (...files) => ["hook", ...files.flatMap((f) => ["--file", f])];
const hookJson = (...files) => [...hook(...files), "--format", "json"];
const write = (content, name = "write") => ({ tool_name: name, tool_input: { path: "x", content } });
const ultron = (file, content) => ({ args: ["protect", "--file", file, "--preview", "ultron"], stdin: write(content) });
const ruffConfig = { ".ruff.toml": "line-length = 100\n" };
const tsconfig = JSON.stringify({ compilerOptions: { strict: true, noEmit: true, target: "es2022", module: "commonjs", lib: ["es2022"] }, include: ["src"] });
const tsProject = { "tsconfig.json": tsconfig, ".gitignore": "node_modules/\n", "src/ok.ts": "export const ok: number = 1;\n" };

const complex = (name, extra = 0) => {
	const branches = Array.from({ length: 11 + extra }, (_, i) => `    if x == ${i}:\n        return ${i}\n`).join("");
	return `def ${name}(x):\n${branches}    return -1\n`;
};
const complexTs = (name) => {
	const branches = Array.from({ length: 11 }, (_, i) => `  if (x === ${i}) return ${i};\n`).join("");
	return `export function ${name}(x: number): number {\n${branches}  return -1;\n}\n`;
};
const block = (seed) =>
	Array.from({ length: 12 }, (_, i) => `    total = total + values[${i}] * ${seed} - offset / scale + bias\n`).join("");
const duplicated = (name) => `def ${name}(values, offset, scale, bias):\n    total = 0\n${block(3)}    return total\n`;

const js = (name, content) => ({ name: `js-${name}`, ...ultron(`src/${name}.js`, content) });
const ts = (name, content, file = `src/${name}.ts`) => ({ name: `ts-${name}`, ...ultron(file, content) });
const py = (name, content, file = `app/${name}.py`) => ({ name: `py-${name}`, ...ultron(file, content) });
const rs = (name, content) => ({ name: `rs-${name}`, ...ultron(`src/${name}.rs`, content) });
const go = (name, content) => ({ name: `go-${name}`, ...ultron(`${name}.go`, content) });
const ex = (name, content) => ({ name: `ex-${name}`, ...ultron(`lib/${name}.ex`, content) });

const cases = [
	// --- JavaScript/TypeScript rule pack (fallback rules: no oxlint) ---
	js("inner-html", "el.innerHTML = userInput;\nel.innerHTML = '';\nel.outerHTML += `<b>${x}</b>`;\n"),
	js("html-calls", "el.insertAdjacentHTML('beforeend', html);\ndocument.write(markup);\ndocument.writeln('ok');\n"),
	js("dangerous-html", "const A = () => <div dangerouslySetInnerHTML={{ __html: body }} />;\nconst B = () => <div dangerouslySetInnerHTML={{ __html: 'x' }} />;\n"),
	js("location", "window.location = next;\nlocation.href = '/home';\nconst location = 1;\nlocation.assign(target);\nlocation.replace(`/a/${id}`);\nlocation.replace(safeUrl(x));\n"),
	js("tls", "const agent = new Agent({ rejectUnauthorized: false });\nprocess.env.NODE_TLS_REJECT_UNAUTHORIZED = '0';\n// rejectUnauthorized: false\n"),
	js("redirect", "app.get('/', (req, res) => { res.redirect(req.query.next); res.redirect('/ok'); });\n"),
	js("exec", "const { exec } = require('child_process');\nconst run = util.promisify(exec);\nexec(`ls ${dir}`);\nexec('ls');\nrun(cmd);\nchild_process.execSync(cmd);\n"),
	js("sql", "db.query(`SELECT * FROM t WHERE id = ${id}`);\ndb.query('SELECT 1');\ndb.execute('DELETE FROM t WHERE id = ' + id);\n"),
	js("timing", "if (signature === expected) {}\nif (hmac.digest('hex') == given) {}\nif (signature.length === 3) {}\nif (`${digest}` === other) {}\n"),
	js("postmessage", "window.addEventListener('message', (event) => { run(event.data); });\nwindow.addEventListener('message', (e) => { if (e.origin !== o) return; });\nwindow.addEventListener('click', go);\n"),
	js("ssrf", "const target = req.query.url;\nfetch(target);\nconst { host } = req.body;\naxios.get(`http://${host}/x`);\nconst clean = validateUrl(req.query.u);\nfetch(clean);\nhttps.get(req.params.u);\n"),
	js("prototype", "function merge(a, b) {\n  for (const key in b) {\n    if (typeof b[key] === 'object') merge(a[key], b[key]);\n    else a[key] = b[key];\n  }\n}\nconst safe = (a, b) => {\n  for (const k of Object.keys(b)) { if (k === '__proto__') continue; safe(a, b); a[k] = b[k]; }\n};\n"),
	js("redos", "const a = /(a+)+$/;\nconst b = /^(?:-[a-z]+)*$/;\nconst c = new RegExp('(\\\\d+)*x');\nconst d = /(x*){2,}/g;\n"),
	js("timers", "setInterval(tick, 10);\nlist.forEach((item, i) => { list.splice(i, 1); });\nthis.items.forEach(x => this.items.splice(0, 1));\n"),
	js("timers-cleared", "const t = setInterval(tick, 10);\nclearInterval(t);\n"),
	js("eval", "eval(code);\neval('1 + 1');\nconst f = new   Function('a', body);\n"),
	js("unicode-offsets", "const s = 'héllo wörld';\n/* ✓ */ el.innerHTML = s + 'ü';\n"),
	js("fallback", "try { go(); } catch (e) {}\ntry { go(); } catch { /* ok */ }\nnew Promise(async (resolve) => resolve());\nfunction f() { try {} finally { return 1; } }\ndebugger;\n"),
	{ name: "js-fallback-test-file", ...ultron("src/a.test.js", "describe.only('x', () => {});\nit.skip('y', () => {});\ntest['only']('z', () => {});\n") },
	{ name: "js-fallback-imported-names", ...ultron("src/spec.js", "import { it as check, describe } from 'vitest';\ncheck.only('a', () => {});\ndescribe.skip('b');\n") },
	{ name: "js-fallback-strict", env: { LOKI_STRICT: "1" }, ...ultron("src/s.js", "debugger;\n") },
	{ name: "js-fallback-json", args: ["protect", "--file", "src/j.js", "--preview", "ultron", "--format", "json"], stdin: write("debugger;\nel.innerHTML = x;\n") },
	{ name: "js-existing-not-new", files: { "src/e.js": "eval(a);\n" }, ...ultron("src/e.js", "eval(a);\nconst x = 1;\n") },
	ts("tsx", "export const A = () => <div dangerouslySetInnerHTML={{ __html: props.html }} />;\n", "src/a.tsx"),
	// Oxlint preview with a fake binary.
	{ name: "ts-oxlint-preview", files: { "node_modules/.bin/oxlint": { exec: OXLINT } }, ...ultron("src/o.ts", "const a = 1;\nBADJS;\n") },
	{ name: "ts-oxlint-preview-existing", files: { "node_modules/.bin/oxlint": { exec: OXLINT }, "src/o.ts": "BADJS;\n" }, ...ultron("src/o.ts", "BADJS;\nconst b = 2;\n") },
	{ name: "ts-oxlint-preview-broken", files: { "node_modules/.bin/oxlint": { exec: "#!/bin/sh\necho oops >&2\nexit 3\n" } }, ...ultron("src/o.ts", "debugger;\n") },
	// --- Python rule pack (CPython ast) ---
	py("request-auth", "def view(request):\n    if request.GET.get('is_admin'):\n        return 1\n    if request.user.role == 'x':\n        return 2\n    return request.data['role'] == 'admin'\n"),
	py("command-injection", "import subprocess\nsubprocess.run(['bash', '-c', cmd])\nsubprocess.run(['bash', '-c', 'ls'])\nsubprocess.check_output(['sh', '-c', None])\n"),
	py("timing", "import hmac\nsig = hmac.new(k, m, 'sha256').hexdigest()\nif sig == provided:\n    pass\nif token_signature != 'x':\n    pass\n"),
	py("redos", "import re\nre.compile(r'(a+)+$')\nre.match('(?:-[a-z]+)*', s)\n"),
	py("path-traversal", "from flask import request\ndef f():\n    name = request.args.get('n')\n    return open('/data/' + name)\ndef g():\n    name = request.args['n']\n    return open(os.path.basename(name))\n"),
	py("tautology", "def test_add():\n    result = add(1, 2)\n    expected = add(1, 2)\n    assert result == expected\n    self.assertEqual(add(1, 2), add(1, 2))\n", "tests/test_math.py"),
	py("ssrf", "import requests\ndef f(request):\n    url = request.GET['u']\n    requests.get(url)\n    requests.post(url='x')\n    requests.get('https://x', timeout=1)\n"),
	py("syntax-error", "def broken(:\n    pass\n"),
	py("unicode-offsets", "s = 'héllo'; ok = request.GET.get('is_admin') and 1\n"),
	py("clean", "def add(a, b):\n    return a + b\n"),
	// --- Rust, Go, Elixir rule packs ---
	rs("placeholders", "fn a() { todo!() }\nfn b() -> u8 { unimplemented!(\"x\") }\n// todo!()\nconst S: &str = \"todo!()\";\n"),
	rs("writer", "fn save(p: &Path) -> io::Result<()> {\n    let mut w = BufWriter::new(File::create(p)?);\n    w.write_all(b\"x\")?;\n    Ok(())\n}\nfn ok(p: &Path) -> io::Result<()> {\n    let mut w = BufWriter::new(File::create(p)?);\n    w.flush()?;\n    Ok(())\n}\n"),
	rs("unchecked", "fn a(v: &[u8], i: usize) -> u8 { unsafe { *v.get_unchecked(i) } }\nfn b(v: &[u8], i: usize) -> u8 { assert!(i < v.len()); unsafe { *v.get_unchecked(i) } }\n"),
	rs("command", "fn run(x: &str) { Command::new(\"sh\").arg(\"-c\").arg(format!(\"ls {x}\")).status(); }\nfn ok() { Command::new(\"sh\").args([\"-c\", \"ls\"]).status(); }\n"),
	go("zip", "package main\n\nimport \"archive/zip\"\n\nfunc extract(f *zip.File, dest string) string {\n\treturn filepath.Join(dest, f.Name)\n}\n\nfunc safe(f *zip.File, dest string) string {\n\tp := filepath.Join(dest, f.Name)\n\tif !filepath.IsLocal(f.Name) {\n\t\treturn \"\"\n\t}\n\treturn p\n}\n"),
	ex("params", "defmodule W do\n  def show(conn, params) do\n    if params[\"is_admin\"] == \"true\", do: :ok\n    redirect(conn, external: params[\"next\"])\n    Enum.sort_by(posts, & &1.inserted_at)\n    {:ok, data} = File.read(path)\n    {:ok, file} = File.open(path)\n    System.cmd(\"sh\", [\"-c\", \"ls #{dir}\"])\n    :os.cmd(~c\"ls\")\n    :os.cmd(command)\n    HTTPoison.get(url_params)\n  end\nend\n"),
	ex("head-flag", "defmodule W do\n  def update(conn, %{\"admin\" => true} = attrs), do: attrs\nend\n"),
	// --- hook: git snapshots, policy, test integrity ---
	{ name: "hook-no-git", files: { "a.txt": "x\n" }, args: hook("a.txt") },
	{ name: "hook-clean", files: { "a.txt": "x\n" }, git: true, after: { "a.txt": "y\n" }, args: hook("a.txt") },
	{ name: "hook-clean-json", files: { "a.txt": "x\n" }, git: true, after: { "a.txt": "y\n" }, args: hookJson("a.txt") },
	{ name: "hook-secret", files: { "a.txt": "x\n" }, git: true, after: { "a.txt": "x\nkey = '@@AWS@@'\n" }, args: hook("a.txt") },
	{ name: "hook-secret-json", files: { "a.txt": "x\n" }, git: true, after: { "a.txt": "key = '@@GH@@'\n" }, args: hookJson("a.txt") },
	{ name: "hook-secret-committed", files: { "a.txt": "key = '@@AWS@@'\n" }, git: true, after: { "a.txt": "key = '@@AWS@@'\nmore\n" }, args: hook("a.txt") },
	{ name: "hook-no-commits", files: { "a.txt": "key = '@@AWS@@'\n" }, gitInit: true, args: hook("a.txt") },
	{ name: "hook-root-not-toplevel", files: { "sub/a.txt": "x\n" }, git: true, rootSub: "sub", args: hook("a.txt") },
	{ name: "hook-protected-target", git: true, files: { "a.txt": "" }, args: hook(".ruff.toml") },
	{ name: "hook-outside", git: true, files: { "a.txt": "" }, args: hook("../x.txt") },
	{ name: "hook-protected-content", files: { "a.txt": "x\n" }, git: true, after: { ".ruff.toml": "x = 1\n" }, args: hook("a.txt") },
	{ name: "hook-loki-uncommitted-json", files: { "a.txt": "x\n" }, git: true, after: cfg({ rule_packs: ["core"] }), args: hookJson("a.txt") },
	{ name: "hook-protected-extra-base", files: { "a.txt": "x\n", ...cfg({ protected_extra: ["docs/**"] }), "docs/r.md": "a\n" }, git: true, after: { "docs/r.md": "b\n" }, args: hook("a.txt") },
	{ name: "hook-invalid-base-policy", files: { "a.txt": "x\n", ".loki/loki.json": "{\"rule_packs\": [\"core\",]}" }, git: true, env: { LOKI_ALLOW_PROTECTED: "1" }, after: { ".loki/loki.json": "{}" }, args: hook("a.txt") },
	{ name: "hook-test-deleted", files: { "tests/a_test.txt": "def test_one():\n    assert x\n\ndef test_two():\n    assert y\n" }, git: true, after: { "tests/a_test.txt": "def test_one():\n    assert x\n" }, args: hook("tests/a_test.txt") },
	{ name: "hook-test-skip-added", files: { "tests/b_test.txt": "it('a', () => { expect(1) })\n" }, git: true, after: { "tests/b_test.txt": "it.skip('a', () => { expect(1) })\n" }, args: hook("tests/b_test.txt") },
	{ name: "hook-test-file-deleted", files: { "spec/c.txt": "test(\"x\")\n", "a.txt": "" }, git: true, after: { "spec/c.txt": null }, args: hook("spec/c.txt") },
	{ name: "hook-test-file-moved", files: { "tests/old_test.txt": "assert 1\n" }, git: true, after: { "tests/old_test.txt": null, "tests/new_test.txt": "assert 1\n" }, args: hook("tests/old_test.txt", "tests/new_test.txt") },
	{ name: "hook-test-untouched", files: { "tests/a_test.txt": "assert x\n", "a.txt": "" }, git: true, after: { "tests/a_test.txt": "", "a.txt": "y" }, args: hook("a.txt") },
	{ name: "hook-quality-threshold", files: { "pyproject.toml": "[tool.coverage.report]\nfail_under = 90\n" }, git: true, after: { "pyproject.toml": "[tool.coverage.report]\nfail_under = 10\n" }, args: hook("pyproject.toml") },
	{ name: "hook-test-non-utf8", files: { "tests/x_test.txt": "a\n" }, git: true, after: { "tests/x_test.txt": { bytes: [0xff, 0xfe, 0x0a] } }, args: hook("tests/x_test.txt") },
	{ name: "hook-lockfile-deleted", files: { "package.json": "{}\n", "package-lock.json": "{}\n", "a.txt": "" }, git: true, after: { "package-lock.json": null, "a.txt": "x" }, args: hook("a.txt") },
	{ name: "hook-lockfile-and-manifest-deleted", files: { "Cargo.toml": "[package]\n", "Cargo.lock": "x\n", "a.txt": "" }, git: true, after: { "Cargo.lock": null, "Cargo.toml": null }, args: hook("a.txt") },
	{ name: "hook-lockfile-symlink", files: { "go.mod": "module m\n", "go.sum": "x\n" }, git: true, after: { "go.sum": null }, links: { "go.sum": "go.mod" }, args: hook("go.mod") },
	{
		name: "hook-dependencies-unapproved",
		files: {
			...cfg({ approved_dependencies: ["npm:left-pad", "pypi:requests"] }),
			"package.json": JSON.stringify({ dependencies: { "left-pad": "1" }, devDependencies: { evil: "1" } }),
			"pyproject.toml": "[project]\ndependencies = [\"requests>=2\", \"Flask_Login\"]\n",
			"requirements-dev.txt": "# c\n-r base.txt\npytest==8\n",
			"go.mod": "module m\n\nrequire (\n\tgithub.com/x/y v1.0.0\n)\nrequire golang.org/z v0.1.0\n",
			"Cargo.toml": "[dependencies]\nserde = \"1\"\n",
			"mix.exs": "defp deps do\n  [{:phoenix, \"~> 1.7\"}]\nend\n",
		},
		git: true,
		after: { "package.json": JSON.stringify({ dependencies: { "left-pad": "1" }, devDependencies: { evil: "2" } }) },
		args: hook("package.json"),
	},
	{ name: "hook-dependencies-untouched", files: { ...cfg({ approved_dependencies: [] }), "package.json": "{\"dependencies\": {\"x\": \"1\"}}", "a.txt": "" }, git: true, after: { "a.txt": "b" }, args: hook("a.txt") },
	{ name: "hook-many-files", files: { "a.txt": "", "b.md": "" }, git: true, after: { "a.txt": "1", "b.md": "@@GH@@\n" }, args: hook("a.txt", "b.md", "a.txt") },
	{ name: "hook-missing-target", files: { "a.txt": "" }, git: true, args: hook("gone.txt") },
	{ name: "hook-claude-harness", files: { "a.txt": "x\n" }, git: true, after: { "a.txt": "y\n" }, args: ["hook", "--harness", "claude"], stdin: { tool_name: "Write", cwd: "<ROOT>", tool_input: { file_path: "a.txt" } } },
	{ name: "hook-claude-harness-ts-warning", files: { "a.ts": "export const a = 1;\n" }, git: true, after: { "a.ts": "export const a = 2;\n" }, args: ["hook", "--harness", "claude"], stdin: { tool_name: "Edit", cwd: "<ROOT>", tool_input: { file_path: "<ROOT>/a.ts" } } },
	{ name: "hook-harness-json", args: ["hook", "--harness", "claude", "--format", "json"], stdin: {} },
	{ name: "hook-harness-bad-tool", args: ["hook", "--harness", "codex"], stdin: { tool_name: "Write", cwd: "/", tool_input: {} } },
	{ name: "hook-harness-relative-cwd", args: ["hook", "--harness", "claude"], stdin: { tool_name: "Write", cwd: "x", tool_input: {} } },
	{ name: "hook-harness-not-object", args: ["hook", "--harness", "factory"], stdin: [] },
	{ name: "hook-harness-factory-patch", args: ["hook", "--harness", "factory"], stdin: { tool_name: "ApplyPatch", cwd: "/", tool_input: {} } },
	{ name: "hook-harness-bad-json", args: ["hook", "--harness", "claude"], rawStdin: "{\"a\": " },
	{ name: "hook-codex-patch", files: { "a.txt": "x\n" }, git: true, after: { "a.txt": "@@AWS@@\n" }, args: ["hook", "--harness", "codex"], stdin: { tool_name: "apply_patch", cwd: "<ROOT>", tool_input: { command: "*** Begin Patch\n*** Update File: a.txt\n@@\n-x\n+y\n*** End Patch\n" } } },
	{ name: "hook-codex-patch-invalid", args: ["hook", "--harness", "codex"], stdin: { tool_name: "apply_patch", cwd: "/", tool_input: { command: "*** Begin Patch\n*** Add File: a\n*** End Patch" } } },
	{ name: "protect-codex-move", args: ["protect", "--harness", "codex"], stdin: { tool_name: "apply_patch", cwd: "<ROOT>", tool_input: { command: "*** Begin Patch\n*** Update File: a.txt\n*** Move to: .ruff.toml\n*** End Patch" } } },
	{ name: "protect-claude-multiedit", files: { "a.txt": "one\ntwo\n" }, args: ["protect", "--harness", "claude"], stdin: { tool_name: "MultiEdit", cwd: "<ROOT>", tool_input: { file_path: "a.txt", edits: [{ old_string: "one", new_string: "@@AWS@@" }, { old_string: "two", new_string: "2" }] } } },
	{ name: "protect-factory-edit-ts-warning", files: { "a.ts": "let a = 1;\n" }, args: ["protect", "--harness", "factory"], stdin: { tool_name: "Edit", cwd: "<ROOT>", tool_input: { file_path: "a.ts", old_str: "1", new_str: "2" } } },
	{ name: "protect-preview-non-utf8", files: { "a.txt": { bytes: [0x61, 0xff] } }, args: ["protect", "--file", "a.js", "--preview", "ultron"], stdin: write("x") },
	{ name: "protect-invalid-config-json", files: { ".loki/loki.json": "{\"a\": 1,}" }, args: ["protect", "--file", "a.txt"] },
	{ name: "protect-config-not-utf8", files: { ".loki/loki.json": { bytes: [0xff] } }, args: ["protect", "--file", "a.txt", "--format", "json"] },
	// --- hook language analyzers ---
	{ name: "hook-py-missing-ruff", files: { "app/a.py": "x = 1\n" }, git: true, after: { "app/a.py": "def stub():\n    pass\n" }, args: hook("app/a.py") },
	{ name: "hook-py-stub-existing", files: { "app/a.py": "def stub():\n    pass\n" }, git: true, after: { "app/a.py": "def stub():\n    pass\n\nx = 1\n" }, args: hook("app/a.py") },
	{ name: "hook-py-interface-stubs", files: { "app/a.py": "" }, git: true, after: { "app/a.py": "from abc import ABC, abstractmethod\nclass A(ABC):\n    def f(self):\n        ...\nclass B:\n    @abstractmethod\n    def g(self):\n        pass\n    def h(self):\n        raise NotImplementedError\ndef top():\n    \"\"\"doc\"\"\"\n    raise NotImplementedError()\n" }, args: hook("app/a.py") },
	{ name: "hook-py-syntax-error", files: { "app/a.py": "" }, git: true, after: { "app/a.py": "def (\n" }, args: hookJson("app/a.py") },
	{ name: "hook-py-base-unparsable", files: { "app/a.py": "print 'x'\n" }, git: true, after: { "app/a.py": "x = 1\n" }, args: hook("app/a.py") },
	{ name: "hook-py-imports-no-interpreter", files: { "app/a.py": "" }, git: true, after: { "app/a.py": "import os\nimport nonexistent_pkg_q\nfrom app import a\n" }, args: hook("app/a.py") },
	{ name: "hook-py-imports-resolved", files: { "app/a.py": "", "requirements.txt": "Declared-Pkg>=1\n", "src/localmod.py": "" }, git: true, after: { "app/a.py": "import json\nimport nonexistent_pkg_q.sub\nimport declared_pkg\nimport localmod\ntry:\n    import optional_thing\nexcept ImportError:\n    pass\nfrom . import sibling\n" }, env: { LOKI_PYTHON: "<PYTHON>" }, args: hook("app/a.py") },
	{ name: "hook-py-imports-strict", files: { "app/a.py": "" }, git: true, after: { "app/a.py": "import nonexistent_pkg_q\n" }, env: { LOKI_STRICT: "1" }, args: hook("app/a.py") },
	{ name: "hook-py-rules", files: { "app/a.py": "" }, git: true, after: { "app/a.py": "import subprocess\nsubprocess.run(['sh', '-c', cmd])\n" }, args: hook("app/a.py") },
	{ name: "hook-py-ruff-no-config", bin: { ruff: RUFF }, files: { "app/a.py": "" }, git: true, after: { "app/a.py": "x = 1  # BADX\n" }, args: hook("app/a.py") },
	{ name: "hook-py-ruff-new", bin: { ruff: RUFF }, files: { ...ruffConfig, "app/a.py": "y = 1  # BADX\n" }, git: true, after: { "app/a.py": "y = 1  # BADX\nz = 2  # BADX\n" }, args: hook("app/a.py") },
	{ name: "hook-py-ruff-syntax", bin: { ruff: RUFF }, files: { ...ruffConfig, "app/a.py": "" }, git: true, after: { "app/a.py": "x = 1  # SYNTAXERR\n" }, args: hookJson("app/a.py") },
	{ name: "hook-py-ruff-extend", bin: { ruff: RUFF }, files: { ".ruff.toml": "extend = \"other.toml\"\n", "app/a.py": "" }, git: true, after: { "app/a.py": "x = 1\n" }, args: hook("app/a.py") },
	{ name: "hook-py-ruff-no-commit", bin: { ruff: RUFF }, files: { "app/a.py": "x = 1\n" }, gitInit: true, args: hook("app/a.py") },
	{ name: "hook-py-mypy", bin: { ruff: RUFF, mypy: MYPY }, files: { ...ruffConfig, "app/a.py": "a: int = 1  # MYPYBAD\n" }, git: true, after: { "app/a.py": "a: int = 1  # MYPYBAD\nb: int = 2  # MYPYBAD\n" }, args: hook("app/a.py") },
	{ name: "hook-py-mypy-unannotated", bin: { ruff: RUFF, mypy: MYPY }, files: { ...ruffConfig, "app/a.py": "" }, git: true, after: { "app/a.py": "a = 1  # MYPYBAD\n" }, args: hook("app/a.py") },
	{ name: "hook-py-mypy-broken", bin: { ruff: RUFF, mypy: "#!/bin/sh\necho crashed >&2\nexit 2\n" }, files: { ...ruffConfig, "app/a.py": "" }, git: true, after: { "app/a.py": "a: int = 1\n" }, args: hook("app/a.py") },
	{ name: "hook-py-language-disabled", files: { ...cfg({ languages: { python: false } }), "app/a.py": "" }, git: true, after: { "app/a.py": "def stub():\n    pass\n" }, args: hook("app/a.py") },
	{ name: "hook-ts-missing-oxlint", files: { "src/a.ts": "" }, git: true, after: { "src/a.ts": "debugger;\neval(x);\n" }, args: hook("src/a.ts") },
	{ name: "hook-ts-oxlint", files: { "src/a.ts": "", "node_modules/.bin/oxlint": { exec: OXLINT }, ".gitignore": "node_modules/\n" }, git: true, after: { "src/a.ts": "BADJS;\nconsole.log(1);\n" }, args: hook("src/a.ts") },
	{ name: "hook-ts-oxlint-warning-only", files: { "src/a.ts": "", "node_modules/.bin/oxlint": { exec: OXLINT }, ".gitignore": "node_modules/\n" }, git: true, after: { "src/a.ts": "console.log(1);\n" }, args: hookJson("src/a.ts") },
	{ name: "hook-rs-missing-rustfmt", files: { "src/lib.rs": "" }, git: true, after: { "src/lib.rs": "fn a() { todo!() }\n" }, args: hook("src/lib.rs") },
	{ name: "hook-rs-clippy", bin: { rustfmt: RUSTFMT, cargo: CARGO }, files: { "Cargo.toml": "[package]\nname = \"c\"\n", "src/lib.rs": "fn a() { x.unwrap() } // CLIPPYBAD\n" }, git: true, after: { "src/lib.rs": "fn a() { x.unwrap() } // CLIPPYBAD\nfn b() { y.unwrap() } // CLIPPYBAD\n" }, args: hook("src/lib.rs") },
	{ name: "hook-rs-rustfmt-fails", bin: { rustfmt: RUSTFMT, cargo: CARGO }, files: { "Cargo.toml": "[package]\n", "src/lib.rs": "" }, git: true, after: { "src/lib.rs": "fn a() {} // BADFMT\n" }, args: hook("src/lib.rs") },
	{ name: "hook-rs-no-crate", bin: { rustfmt: RUSTFMT, cargo: CARGO }, files: { "x.rs": "" }, git: true, after: { "x.rs": "fn a() {}\n" }, args: hook("x.rs") },
	{ name: "hook-go-missing", files: { "a.go": "" }, git: true, after: { "a.go": "package a\n" }, args: hook("a.go") },
	{ name: "hook-ex-missing-mix", files: { "lib/a.ex": "" }, git: true, after: { "lib/a.ex": "defmodule A do\nend\n" }, args: hook("lib/a.ex") },
	{ name: "hook-strict-missing-tool", env: { LOKI_STRICT: "1" }, files: { "src/a.ts": "" }, git: true, after: { "src/a.ts": "export const a = 1;\n" }, args: hook("src/a.ts") },
	// --- TypeScript project checks with the real compiler ---
	{ name: "hook-ts-types", node: true, typescript: true, files: { ...tsProject }, git: true, after: { "src/bad.ts": "export const n: number = 'x';\n" }, args: hook("src/bad.ts") },
	{ name: "hook-ts-types-existing", node: true, typescript: true, files: { ...tsProject, "src/old.ts": "export const n: number = 'x';\n" }, git: true, after: { "src/old.ts": "export const n: number = 'x';\nexport const m = 1;\n" }, args: hook("src/old.ts") },
	{ name: "hook-ts-semantic", node: true, typescript: true, files: { ...tsProject }, git: true, after: { "src/p.ts": "async function f(): Promise<void> {}\nexport function g(): void {\n  f();\n  [3, 10].sort();\n}\n" }, args: hook("src/p.ts") },
	{ name: "hook-ts-config-change", node: true, typescript: true, files: { ...tsProject }, git: true, after: { "tsconfig.json": tsconfig.replace("es2022\"]", "es2022\", \"dom\"]"), "src/a.ts": "export const a = 1;\n" }, args: hook("src/a.ts") },
	{ name: "hook-ts-check-explicit", node: true, typescript: true, files: { ...tsProject, ...cfg({ typescript_check: true }) }, gitInit: true, args: hook("src/ok.ts") },
	{ name: "hook-ts-check-off", node: true, typescript: true, files: { ...tsProject, ...cfg({ typescript_check: false }) }, git: true, after: { "src/bad.ts": "export const n: number = 'x';\n" }, args: hook("src/bad.ts") },
	{ name: "protect-ts-types-preview", node: true, typescript: true, files: { ...tsProject }, git: true, args: ["protect", "--file", "src/ok.ts", "--preview", "ultron"], stdin: write("export const ok: number = 'no';\n") },
	{ name: "protect-ts-types-preview-clean", node: true, typescript: true, files: { ...tsProject }, git: true, args: ["protect", "--file", "src/new.ts", "--preview", "ultron"], stdin: write("export const fresh: number = 2;\n") },
	// --- written slop (advisory) ---
	{ name: "hook-slop-complexity", files: { "app/m.py": "x = 1\n" }, git: true, after: { "app/m.py": complex("busy") }, args: hook("app/m.py") },
	{ name: "hook-slop-complexity-grew", files: { "app/m.py": complex("busy") }, git: true, after: { "app/m.py": complex("busy", 3) }, args: hookJson("app/m.py") },
	{ name: "hook-slop-block", files: { ...cfg({ slop: { block: true } }), "app/m.py": "" }, git: true, after: { "app/m.py": complex("busy") }, args: hook("app/m.py") },
	{ name: "hook-slop-ts-structural", files: { "src/m.ts": "" }, git: true, after: { "src/m.ts": complexTs("busy") }, args: hook("src/m.ts") },
	{ name: "hook-slop-ts-exact", node: true, typescript: true, files: { "src/m.ts": "", ".gitignore": "node_modules/\n" }, git: true, after: { "src/m.ts": complexTs("busy") }, args: hook("src/m.ts") },
	{ name: "hook-slop-rust", files: { "src/m.rs": "" }, git: true, after: { "src/m.rs": `fn busy(x: u8) -> u8 {\n${Array.from({ length: 11 }, (_, i) => `    if x == ${i} { return ${i}; }\n`).join("")}    0\n}\n` }, args: hook("src/m.rs") },
	{ name: "hook-slop-duplication", files: { "app/a.py": duplicated("first"), "app/b.py": "" }, git: true, after: { "app/b.py": duplicated("second") }, args: hook("app/b.py") },
	{ name: "hook-slop-cycle", files: { "pkg/__init__.py": "", "pkg/a.py": "from pkg import b\n", "pkg/b.py": "x = 1\n" }, git: true, after: { "pkg/b.py": "from . import a\n" }, args: hook("pkg/b.py") },
	{ name: "hook-slop-ts-cycle", files: { "src/a.ts": "import { b } from './b';\nexport const a = 1;\n", "src/b.ts": "export const b = 1;\n" }, git: true, after: { "src/b.ts": "import { a } from './a.js';\nexport const b = a;\n" }, args: hook("src/b.ts") },
	{ name: "hook-slop-no-git-note", files: { "app/m.py": "x = 1\n" }, args: hook("app/m.py") },
	// --- slop command ---
	{ name: "slop-empty", args: ["slop"] },
	{
		name: "slop-report",
		files: {
			"app/a.py": duplicated("first") + complex("busy"),
			"app/b.py": duplicated("second") + "from app import a\n",
			"app/c.py": "from app import b\nimport app.a\n",
			"app/__init__.py": "",
			"src/x.ts": complexTs("tbusy") + "import { y } from './y';\n",
			"src/y.ts": "import { x } from './x';\nexport const y = () => { if (a && b) { return c ?? d; } };\nif (v === true) {}\n// const old = 1;\n",
			"src/l.rs": "fn alpha(x: u8) -> u8 { match x { 1 => 2, _ => 3 } }\n",
			"lib/a.ex": "defmodule A do\n  import B\n  def f(x) do\n    if x, do: 1, else: 2\n  end\nend\n",
			"lib/b.ex": "defmodule B do\n  require A\n  def g(x), do: x\nend\n",
			"Main.java": "class Main {\n  public int run(int x) {\n    if (x > 1 && x < 3) { return 1; }\n    return x > 2 ? 1 : 0;\n  }\n}\n",
			"tests/test_skip.py": complex("ignored"),
			"vendor/v.py": complex("vendored"),
			"app/z.min.js": "function a(){}\n",
		},
		git: true,
		args: ["slop"],
	},
	{ name: "slop-report-json", files: { "app/a.py": duplicated("first") + complex("busy"), "app/b.py": duplicated("second"), "app/v.py": "def check(x):\n    if x:\n        return True\n    else:\n        return False\n# x = compute()\ndef y():\n    r = 1\n    return r\n" }, git: true, args: ["slop", "--json"] },
	{ name: "slop-top-zero", files: { "app/a.py": complex("busy") }, args: ["slop", "--top", "0"] },
	{ name: "slop-policy-fails", files: { ...cfg({ slop: { max_index: 0 } }), "app/a.py": complex("busy") }, git: true, args: ["slop"] },
	{ name: "slop-base", files: { ...cfg({ slop: { max_index_increase: 0 } }), "app/a.py": "x = 1\n" }, git: true, after: { "app/a.py": complex("busy") }, args: ["slop", "--base", "HEAD"] },
	{ name: "slop-bad-base", files: { "a.py": "" }, git: true, args: ["slop", "--base", "nope"] },
	{ name: "slop-ts-exact", node: true, typescript: true, files: { "src/x.ts": complexTs("tbusy") + "const f = (a?: {b: number}) => a?.b ?? 0;\n", ".gitignore": "node_modules/\n" }, git: true, args: ["slop", "--json"] },
	{ name: "slop-invalid-config", files: cfg({ slop: { block: 1 } }), args: ["slop"] },
	// --- scan ---
	{ name: "scan-no-git", files: { "a.txt": "" }, args: ["scan"] },
	{ name: "scan-no-git-strict", files: { "a.txt": "" }, args: ["scan", "--strict"] },
	{ name: "scan-clean", files: { "a.txt": "x\n" }, git: true, args: ["scan"] },
	{ name: "scan-secret-change", files: { "a.txt": "x\n" }, git: true, after: { "a.txt": "@@AWS@@\n" }, args: ["scan"] },
	{ name: "scan-protected", files: { "a.txt": "" }, git: true, after: { ".ruff.toml": "" }, args: ["scan"] },
	{ name: "scan-test-integrity", files: { "tests/t_test.txt": "assert a\n" }, git: true, after: { "tests/t_test.txt": "" }, args: ["scan"] },
	{ name: "scan-python-missing-ruff", files: { "app/a.py": "def f():\n    pass\n" }, git: true, args: ["scan"] },
	{ name: "scan-python-strict", files: { "app/a.py": "import nonexistent_q\n" }, git: true, args: ["scan", "--strict"] },
	{ name: "scan-python-ruff", bin: { ruff: RUFF }, files: { "app/a.py": "x = 1  # BADX\n", "app/b.py": "UNFORMATTED = 1\n" }, git: true, args: ["scan"] },
	{ name: "scan-ruff-new", bin: { ruff: RUFF }, files: { ...ruffConfig, "app/a.py": "x = 1\n" }, git: true, after: { "app/a.py": "x = 1  # BADX\n" }, args: ["scan", "--ruff-new"] },
	{ name: "scan-ruff-new-no-config", bin: { ruff: RUFF }, files: { "app/a.py": "x = 1\n" }, git: true, args: ["scan", "--ruff-new"] },
	{ name: "scan-ts-missing-oxlint", files: { "src/a.ts": "export const a = 1;\n" }, git: true, args: ["scan"] },
	{ name: "scan-ts-oxlint", files: { "src/a.ts": "BADJS;\n", "node_modules/.bin/oxlint": { exec: "#!/bin/sh\necho \"lint: $*\"\nexit 1\n" }, ".gitignore": "node_modules/\n" }, git: true, args: ["scan"] },
	{ name: "scan-rust", bin: { cargo: CARGO }, files: { "src/lib.rs": "fn a() {} // CLIPPYBAD\n" }, git: true, args: ["scan"] },
	{ name: "scan-elixir-missing-mix", files: { "mix.exs": "", "lib/a.ex": "", "other/b.ex": "" }, git: true, args: ["scan"] },
	{ name: "scan-go-missing", files: { "a.go": "package a\n" }, git: true, args: ["scan", "--strict"] },
	{ name: "scan-commands", files: { ...cfg({ commands: { contract: [["sh", "-c", "echo contract failed; exit 3"]], property: [["sh", "-c", "exit 0", "{files}"]], differential: [["sh", "-c", "echo {base}"]] } }), "a.txt": "x\n" }, git: true, args: ["scan"] },
	{ name: "scan-commands-files", files: { ...cfg({ commands: { property: [["sh", "-c", "echo got \"$@\"; exit 1", "sh", "{files}"]] } }), "a.txt": "x\n" }, git: true, after: { "a.txt": "y\n" }, args: ["scan", "--base", "HEAD"] },
	{ name: "scan-lock-sync-notice", files: { "package.json": "{}" }, git: true, after: { "package.json": "{\"a\": 1}" }, args: ["scan"] },
	{ name: "scan-bad-base", files: { "a.txt": "" }, git: true, args: ["scan", "--base", "nope"] },
	{ name: "scan-language-disabled", files: { ...cfg({ languages: { python: false } }), "app/a.py": "def f():\n    pass\n" }, git: true, args: ["scan"] },
	{ name: "scan-invalid-config", files: cfg({ zeta: 1 }), args: ["scan"] },
	// --- init ---
	{ name: "init-minimal", tree: true, args: ["init", "--minimal", "--dir", "<ROOT>/proj"] },
	{ name: "init-minimal-keeps-config", tree: true, files: { "proj/.loki/loki.json": "{}\n", "proj/.loki/loki.py": "old\n" }, args: ["init", "--minimal", "--dir", "proj"] },
	{ name: "init-minimal-force", tree: true, files: { "proj/.loki/loki.json": "{}\n", "proj/.loki/loki.py": "old\n" }, args: ["init", "--minimal", "--force", "--dir", "proj"] },
	{ name: "init-minimal-shell-guard", tree: true, args: ["init", "--minimal", "--shell-guard"] },
	{ name: "init-minimal-file-target", tree: true, files: { "f": "x" }, args: ["init", "--minimal", "--dir", "f"] },
	// --- more rule-pack edges ---
	js("location-literals", "location.href = 'https://example.com/path';\nwindow.location.href = `/u/${id}`;\ndocument.location = `${base}/x`;\nlocation = sanitizeTarget(next);\nlet location = elsewhere;\ntop.location = '//evil';\nlocation.assign('#top');\n"),
	js("html-expression", "el.innerHTML = cond ? a : b, other = 1;\nel.innerHTML = (\n  `<p>`\n);\nel.innerHTML = '' + x;\nnode.insertAdjacentHTML('afterend');\n"),
	js("regex-division", "const r = a / b / c;\nconst q = x.split(/(\\s+)*/);\nreturn /[a-z]+(?:,[a-z]+)*/;\nconst k = /(?:a|b)+c+/;\n"),
	js("sql-more", "pool.raw(\"UPDATE t SET a = '\" + v + \"'\");\nknex.prepare('INSERT INTO t VALUES (?)');\nq.unsafe(`delete from t where id=${id}`);\n"),
	js("sql-identifier", "db.prepare(`SELECT * FROM ${name} ORDER BY 1`);\ndb.query(`SELECT * FROM \"${table}\" ORDER BY ${column}`);\ndb.query(`INSERT INTO s.${table} (a) VALUES (?)`, [a]);\n"),
	js("sql-identifier-mixed", "db.query(`SELECT * FROM ${table} WHERE id = ${id}`);\ndb.query(`SELECT * FROM t WHERE name = '${name}'`);\ndb.query('SELECT * FROM ' + table);\n"),
	js("comments-and-strings", "// el.innerHTML = x;\nconst s = \"eval(code)\";\n/* setInterval(f) */\nconst t = `${eval(code)}`;\n"),
	js("crlf", "el.innerHTML = a;\r\nel.innerHTML = b;\r\n"),
	ts("generics", "export function merge<T>(a: T, b: Partial<T>): T {\n  for (const key in b) {\n    merge(a as any, b as any);\n    (a as any)[key] = (b as any)[key];\n  }\n  return a;\n}\n"),
	py("nested-tests", "def test_outer():\n    def test_inner():\n        assert f(1) == f(1)\n    x = g()\n    y = g()\n    assert x == y\n", "tests/test_nested.py"),
	py("request-more", "def f(req, request):\n    a = req.form['permissions']\n    b = request.session.get('role')\n    c = not req.cookies.get('superuser')\n    while request.query_params['scope']:\n        pass\n    return params.get('admin_flag') if x else 1\n"),
	py("ssrf-taint-chain", "import httpx\ndef f(request):\n    base = request.GET['host']\n    url = 'http://' + base\n    httpx.get(url)\n    other = url\n    urllib.request.urlopen(other)\n"),
	py("redos-calls", "import re\nre.findall(r'(\\w+\\s?)+$', s)\nre.sub('(x+x+)+y', '', s)\nre.split('(?:,\\s*)*', s)\nrx.compile('(a+)+')\n"),
	py("decorated-and-async", "import abc\nclass P(abc.ABC):\n    def m(self):\n        pass\nasync def handler(request):\n    if request.json['is_staff']:\n        return 1\n"),
	py("non-ascii-col", "x = 'ééé'; y = subprocess.call(['sh', '-c', cmd])\n"),
	rs("nested-fns", "fn outer() {\n    fn inner(v: &[u8], j: usize) -> u8 { unsafe { *v.get_unchecked_mut(j) } }\n    let w = BufWriter::with_capacity(8, f);\n}\nfn into(w: BufWriter<File>) -> File { w.into_inner().unwrap() }\n"),
	rs("raw-strings", "fn a() { let s = r#\"todo!()\"#; let c = '\\''; let l: &'static str = \"x\"; unimplemented!{} }\n"),
	ex("charlists", "defmodule C do\n  def run(x) do\n    :os.cmd('ls #{x}')\n    System.cmd(\"bash\", [\"-c\", cmd])\n    Req.get!(params[\"url\"])\n    Enum.max_by(list, & &1.updated_at, DateTime)\n    Enum.min_by(list, & &1.date, :desc)\n  end\nend\n"),
	{ name: "py-preview-ruff", bin: { ruff: RUFF }, files: { ...ruffConfig, "app/a.py": "x = 1  # BADX\n" }, git: true, ...ultron("app/a.py", "x = 1  # BADX\ny = 2  # BADX\n") },
	{ name: "py-preview-ruff-no-config", bin: { ruff: RUFF }, files: { "app/a.py": "" }, git: true, ...ultron("app/a.py", "x = 1  # BADX\n") },
	{ name: "protect-claude-edit-replace-all", files: { "a.txt": "t t\n" }, args: ["protect", "--harness", "claude"], stdin: { tool_name: "Edit", cwd: "<ROOT>", tool_input: { file_path: "a.txt", old_string: "t", new_string: "@@GH@@", replace_all: true } } },
	{ name: "protect-claude-write-no-content", args: ["protect", "--harness", "claude"], stdin: { tool_name: "Write", cwd: "<ROOT>", tool_input: { file_path: "a.js" } } },
	{ name: "protect-claude-multiedit-too-many", files: { "a.js": "x\n" }, args: ["protect", "--harness", "claude"], stdin: { tool_name: "MultiEdit", cwd: "<ROOT>", tool_input: { file_path: "a.js", edits: [] } } },
	{ name: "protect-claude-multiedit-bad-edit", files: { "a.js": "x\n" }, args: ["protect", "--harness", "claude"], stdin: { tool_name: "MultiEdit", cwd: "<ROOT>", tool_input: { file_path: "a.js", edits: [1] } } },
	{ name: "protect-python-flag", files: { "a.txt": "" }, args: ["protect", "--file", "a.txt", "--python", "/nonexistent/python"] },
	{ name: "protect-ts-new-uncovered", node: true, typescript: true, files: { ...tsProject }, git: true, args: ["protect", "--file", "other/n.ts", "--preview", "ultron"], stdin: write("export const z: number = 'q';\n") },
	// --- more hook edges ---
	{ name: "hook-non-utf8-written", files: { "a.txt": "x\n" }, git: true, after: { "a.txt": { bytes: [0x61, 0xff, 0x0a] } }, args: hook("a.txt") },
	{ name: "hook-fifo-tracked", files: { "d/a.txt": "x\n", "b.txt": "" }, git: true, after: { "d/a.txt": null }, fifo: ["d/a.txt"], args: hook("b.txt") },
	{ name: "hook-unmerged", files: { "a.txt": "x\n" }, git: true, conflict: true, args: hook("a.txt") },
	{ name: "hook-renamed-test-to-other-content", files: { "tests/a_test.txt": "assert 1\n" }, git: true, after: { "tests/a_test.txt": null, "tests/b_test.txt": "assert 2\n" }, args: hook("tests/a_test.txt", "tests/b_test.txt") },
	{ name: "hook-executable-mode", files: { "tests/x_test.txt": "assert 1\n" }, git: true, chmod: { "tests/x_test.txt": 0o755 }, args: hook("tests/x_test.txt") },
	{ name: "hook-go-tools", bin: { gofmt: "#!/bin/sh\necho \"gofmt: $*\"\nexit 1\n", go: "#!/bin/sh\nexit 0\n", "golangci-lint": "#!/bin/sh\necho 'a.go:1:1: unused (unused)'\nexit 1\n" }, files: { "a.go": "" }, git: true, after: { "a.go": "package a\n" }, args: hook("a.go") },
	{ name: "hook-go-lint", bin: { gofmt: "#!/bin/sh\nexit 0\n", go: "#!/bin/sh\nexit 0\n", "golangci-lint": "#!/bin/sh\necho 'sub/a.go:1:1: unused (unused)'\nexit 1\n" }, files: { "sub/a.go": "" }, git: true, after: { "sub/a.go": "package a\n" }, args: hook("sub/a.go") },
	{ name: "hook-go-lint-broken", bin: { gofmt: "#!/bin/sh\nexit 0\n", go: "#!/bin/sh\nexit 0\n", "golangci-lint": "#!/bin/sh\necho 'config error' >&2\nexit 3\n" }, files: { "a.go": "" }, git: true, after: { "a.go": "package a\n" }, args: hook("a.go") },
	{ name: "hook-go-vet", bin: { gofmt: "#!/bin/sh\nexit 0\n", go: "#!/bin/sh\necho \"vet: $*\" >&2\nexit 1\n" }, files: { "a.go": "" }, git: true, after: { "a.go": "package a\n" }, args: hook("a.go") },
	{ name: "hook-rs-clippy-broken", bin: { rustfmt: RUSTFMT, cargo: "#!/bin/sh\necho 'error: could not compile' >&2\nexit 101\n" }, files: { "Cargo.toml": "[package]\n", "src/lib.rs": "" }, git: true, after: { "src/lib.rs": "fn a() {}\n" }, args: hook("src/lib.rs") },
	{ name: "hook-rs-clippy-nested-crate", bin: { rustfmt: RUSTFMT, cargo: CARGO }, files: { "crates/c/Cargo.toml": "[package]\n", "crates/c/src/lib.rs": "" }, git: true, after: { "crates/c/src/lib.rs": "fn a() { x.unwrap() } // CLIPPYBAD\n" }, args: hookJson("crates/c/src/lib.rs") },
	{ name: "hook-py-many-files", bin: { ruff: RUFF }, files: { ...ruffConfig, "app/a.py": "", "app/b.py": "" }, git: true, after: { "app/a.py": "x = 1  # BADX\n", "app/b.py": "y = 2  # BADX\n" }, args: hook("app/a.py", "app/b.py") },
	{ name: "hook-py-match-complexity", files: { "app/m.py": "" }, git: true, after: { "app/m.py": "def route(x, items):\n    match x:\n        case 1: return 1\n        case 2 if x: return 2\n        case [a, b]: return a and b or x\n        case {'k': v}: return v\n        case str(): return 0\n        case _: pass\n    try:\n        y = [i for i in items if i if i > 1]\n    except ValueError:\n        y = None\n    except (KeyError, TypeError):\n        y = 0\n    with open(x) as f:\n        while f:\n            for line in f:\n                if line: break\n    return lambda q: q if q else None\n" }, args: hook("app/m.py") },
	{ name: "hook-slop-elixir-structural", files: { "lib/m.ex": "" }, git: true, after: { "lib/m.ex": `defmodule M do\n  def busy(x) do\n${Array.from({ length: 11 }, (_, i) => `    if x == ${i}, do: ${i}\n`).join("")}    x && x || x\n  end\n  def one(x), do: x\nend\n` }, args: hook("lib/m.ex") },
	{ name: "hook-slop-java", files: { "src/M.java": "" }, git: true, after: { "src/M.java": `class M {\n  int busy(int x) {\n${Array.from({ length: 11 }, (_, i) => `    if (x == ${i}) { return ${i}; }\n`).join("")}    return 0;\n  }\n}\n` }, args: hook("src/M.java") },
	{ name: "hook-slop-ruby", files: { "lib/m.rb": "" }, git: true, after: { "lib/m.rb": `def busy(x)\n${Array.from({ length: 11 }, (_, i) => `  return ${i} if x == ${i}\n`).join("")}  x\nend\n` }, args: hook("lib/m.rb") },
	{ name: "hook-slop-namesakes", files: { "app/m.py": `class A:\n${complex("run").replace(/^/gm, "    ")}class B:\n${complex("run").replace(/^/gm, "    ")}` }, git: true, after: { "app/m.py": `class A:\n${complex("run").replace(/^/gm, "    ")}class B:\n${complex("run", 2).replace(/^/gm, "    ")}` }, args: hook("app/m.py") },
	// --- more slop ---
	{ name: "slop-structural-languages", files: { "a.go": "package a\nfunc (s *S) Busy(x int) int {\n\tif x > 1 && x < 3 {\n\t\treturn 1\n\t}\n\tfor i := range x { _ = i }\n\tswitch x {\n\tcase 1:\n\t}\n\treturn 0\n}\nfunc decl() int\n", "b.rb": "class B\n  def self.go(x)\n    if x then 1 elsif x then 2 end\n    x.each { |y| y }\n  end\nend\n", "c.c": "static int f(int x) {\n  return x > 1 ? x : 0;\n}\nint main(void) { while (1) { if (f(1)) break; } }\n", "d.kt": "fun busy(x: Int): Int {\n  when (x) { 1 -> return 1 }\n  return x ?: 0\n}\n", "e.php": "<?php\nfunction f($x) { foreach ($x as $y) { if ($y) {} } }\n" }, args: ["slop", "--json"] },
	{ name: "slop-python-relative-imports", files: { "pkg/__init__.py": "from .a import x\n", "pkg/a.py": "from .b import y\nx = 1\n", "pkg/b.py": "from ..pkg import a\nfrom pkg.sub import c\ny = 2\n", "pkg/sub/__init__.py": "", "pkg/sub/c.py": "from ... import pkg\nfrom .. import b\n", "top.py": "import pkg.a, top\n" }, args: ["slop", "--json"] },
	{ name: "slop-ts-imports", files: { "src/index.ts": "export * from './a';\nimport type { T } from './b';\nimport './side.mjs';\n", "src/a.ts": "import { x } from '../src/index';\nexport const a = 1;\n", "src/b.ts": "import { a } from './a';\nexport type T = number;\n", "src/side.mts": "import { a } from './dir';\n", "src/dir/index.ts": "import '../side.mjs';\n" }, args: ["slop", "--json"] },
	{ name: "slop-clones-multi", files: { "a.py": duplicated("one"), "b.py": duplicated("two"), "c.py": duplicated("three") + duplicated("four"), "short.py": "x = 1\n" }, args: ["slop"] },
	{ name: "slop-non-git-skip-dirs", files: { "node_modules/x.js": complexTs("hidden"), "deps/y.py": complex("hidden"), "app/__pycache__/z.py": complex("hidden"), "app/real.py": complex("real"), "app/data.json": "{}" }, args: ["slop"] },
	{ name: "slop-protected-and-generated", files: { ".loki/x.py": complex("loki"), "gen/a.pb.go": "package a\n", "api_pb2.py": complex("gen"), "build/out.js": complexTs("built"), "x.py": "" }, git: true, args: ["slop", "--json"] },
	{ name: "slop-unparsable-python", files: { "bad.py": "def (:\n", "ok.py": complex("ok") }, args: ["slop", "--json"] },
	{ name: "slop-ts-exact-jsx", node: true, typescript: true, files: { "src/v.tsx": "export function V({ a }: { a?: { b?: () => void } }) {\n  a?.b?.();\n  return <div>{a ? 1 : 2}</div>;\n}\nconst w = function* () { yield 1; };\nclass K { m() { do { } while (false); } }\n", ".gitignore": "node_modules/\n" }, git: true, args: ["slop", "--json"] },
	// --- more scan ---
	{ name: "scan-zero-base", files: { "a.txt": "@@AWS@@\n" }, git: true, args: ["scan", "--base", "0000000000000000000000000000000000000000"] },
	{ name: "scan-empty-base", files: { "a.txt": "" }, git: true, args: ["scan", "--base", ""] },
	{ name: "scan-unborn-head", files: { "a.txt": "x\n" }, gitInit: true, args: ["scan"] },
	{ name: "scan-mutation-no-systemd", files: { ...cfg({ commands: { mutation: [["mutmut", "run", "--max-children", "4"]] } }), "a.txt": "" }, git: true, args: ["scan"] },
	{ name: "scan-commands-base-missing", files: { ...cfg({ commands: { differential: [["echo", "{base}"]] } }), "a.txt": "" }, gitInit: true, args: ["scan"] },
	{ name: "scan-policy-from-base", files: { ...cfg({ approved_dependencies: [] }), "package.json": "{\"dependencies\": {\"q\": \"1\"}}" }, git: true, args: ["scan"] },
	{ name: "scan-python-dedupe", files: { "app/a.py": "import nonexistent_q\n", "app/b.py": "import nonexistent_q\n" }, git: true, args: ["scan"] },
	{ name: "scan-shim-paths-skipped", files: { ".pi/extensions/loki.ts": "x", ".omp/extensions/loki.ts": "y" }, git: true, args: ["scan"] },
	{ name: "scan-ts-types", node: true, typescript: true, files: { ...tsProject, "src/bad.ts": "export const n: number = 'x';\n" }, git: true, args: ["scan"] },
	// --- CPython SyntaxWarnings print on every parse loki.py performs ---
	{ name: "warn-hook", files: { "app/w.py": "a = 1\n" }, git: true, after: { "app/w.py": "a = '\\d'\nb = 2\n" }, args: hook("app/w.py") },
	{ name: "warn-hook-two-files", files: { "app/w.py": "a = '\\d'\n", "app/v.py": "" }, git: true, after: { "app/v.py": "import app.w\nc = '\\q'\n" }, args: hook("app/v.py") },
	{ name: "warn-slop", files: { "w.py": "a = '\\d'\n# b = '\\w'\n" }, args: ["slop"] },
	{ name: "warn-scan", files: { "w.py": "a = '\\d'\n" }, git: true, args: ["scan"] },
	{ name: "warn-preview", ...ultron("w.py", "a = '\\d'\n") },
	{ name: "slop-two-line-clone", files: { "a.py": `x = ${Array.from({ length: 30 }, (_, i) => `a${i} + `).join("")}0\ny = ${Array.from({ length: 30 }, (_, i) => `b${i} * `).join("")}1\n`, "b.py": `p = ${Array.from({ length: 30 }, (_, i) => `c${i} + `).join("")}0\nq = ${Array.from({ length: 30 }, (_, i) => `d${i} * `).join("")}1\n` }, args: ["slop", "--json"] },
	// --- argument errors ---
	{ name: "args-none", args: [] },
	{ name: "args-bogus", args: ["bogus"] },
	{ name: "args-hook-no-target", args: ["hook"] },
	{ name: "args-hook-file-and-harness", args: ["hook", "--file", "a", "--harness", "claude"] },
	{ name: "args-protect-harness-and-file", args: ["protect", "--harness", "codex", "--file", "a"] },
	{ name: "args-hook-bad-format", args: ["hook", "--file", "a", "--format", "xml"] },
	{ name: "args-hook-file-missing-value", args: ["hook", "--file", "-x"] },
	{ name: "args-hook-ambiguous", args: ["hook", "--f", "a"] },
	{ name: "args-hook-abbreviated", files: { "a.txt": "" }, git: true, args: ["hook", "--fi", "a.txt", "--form", "json"] },
	{ name: "args-hook-preview", args: ["hook", "--file", "a", "--preview", "pi"] },
	{ name: "args-scan-root-after", args: ["scan", "--root", "/nonexistent"] },
	{ name: "args-slop-top-int", args: ["slop", "--top", "x"] },
	{ name: "args-slop-top-abbrev", args: ["slop", "--to", "x"] },
	{ name: "args-slop-extra", args: ["slop", "extra"] },
	{ name: "args-init-dir-missing", args: ["init", "--dir"] },
	{ name: "args-context-root-missing", args: ["context", "--root"] },
	{ name: "args-top-root-missing", noRoot: true, args: ["--root"] },
	// --- --record: each run's event (time and duration zeroed) in ~/.local/state/loki/<root>/events.jsonl ---
	{ name: "record-hook-clean", events: true, files: { "a.txt": "x\n" }, git: true, after: { "a.txt": "y\n" }, args: [...hook("a.txt"), "--record"] },
	{ name: "record-hook-secret", events: true, files: { "a.txt": "x\n" }, git: true, after: { "a.txt": "x\nkey = '@@AWS@@'\n" }, args: ["--record", ...hook("a.txt")] },
	{ name: "record-hook-secret-json-policy", events: true, files: { ...cfg({ rule_packs: ["core"] }), "a.txt": "x\n" }, git: true, after: { "a.txt": "key = '@@GH@@'\n" }, args: [...hookJson("a.txt"), "--record"] },
	{ name: "record-hook-test-deleted", events: true, files: { "tests/a_test.txt": "def test_one():\n    assert x\n\ndef test_two():\n    assert y\n" }, git: true, after: { "tests/a_test.txt": "def test_one():\n    assert x\n" }, args: [...hook("tests/a_test.txt"), "--record"] },
	{ name: "record-hook-test-file-deleted", events: true, files: { "spec/c.txt": "test(\"x\")\n", "a.txt": "" }, git: true, after: { "spec/c.txt": null }, args: [...hook("spec/c.txt"), "--record"] },
	{ name: "record-hook-quality", events: true, files: { "pyproject.toml": "[tool.coverage.report]\nfail_under = 90\n" }, git: true, after: { "pyproject.toml": "[tool.coverage.report]\nfail_under = 10\n" }, args: [...hook("pyproject.toml"), "--record"] },
	{ name: "record-hook-protected-content", events: true, files: { "a.txt": "x\n" }, git: true, after: { ".ruff.toml": "x = 1\n" }, args: [...hook("a.txt"), "--record"] },
	{ name: "record-hook-protected-target", events: true, git: true, files: { "a.txt": "" }, args: [...hook(".ruff.toml"), "--record"] },
	{ name: "record-hook-outside", events: true, git: true, files: { "a.txt": "" }, args: [...hook("../x.txt"), "--record"] },
	{ name: "record-hook-directory", events: true, git: true, files: { "d/a.txt": "" }, args: [...hook("d"), "--record"] },
	{ name: "record-hook-ruff-new", events: true, bin: { ruff: RUFF }, files: { ...ruffConfig, "app/a.py": "y = 1  # BADX\n" }, git: true, after: { "app/a.py": "y = 1  # BADX\nz = 2  # BADX\n" }, args: [...hook("app/a.py"), "--record"] },
	{ name: "record-hook-python-syntax", events: true, files: { "app/a.py": "x = 1\n" }, git: true, after: { "app/a.py": "def (:\n" }, args: [...hook("app/a.py"), "--record"] },
	{ name: "record-hook-stub", events: true, files: { "app/a.py": "x = 1\n" }, git: true, after: { "app/a.py": "def stub():\n    pass\n" }, args: [...hook("app/a.py"), "--record"] },
	{ name: "record-hook-warning", events: true, files: { "a.ts": "export const a = 1;\n" }, git: true, after: { "a.ts": "export const a = 2;\n" }, args: [...hook("a.ts"), "--record"] },
	{ name: "record-hook-secret-after", events: true, files: { "a.txt": "x\n" }, git: true, after: { "a.txt": "x\nkey = '@@AWS@@'\n" }, args: [...hook("a.txt"), "--record"] },
	{ name: "record-hook-invalid-policy", events: true, files: { "a.txt": "x\n", ".loki/loki.json": "{\"rule_packs\": 3}" }, git: true, args: [...hook("a.txt"), "--record"] },
	{ name: "record-hook-no-git", events: true, files: { "a.txt": "x\n" }, args: [...hook("a.txt"), "--record"] },
	{ name: "record-protect-preview-secret", events: true, files: { "a.txt": "" }, git: true, args: ["protect", "--file", "app/s.py", "--preview", "ultron", "--record"], stdin: write("key = '@@AWS@@'\n") },
	{ name: "record-protect-loki-policy", events: true, git: true, files: { "a.txt": "" }, args: ["protect", "--file", ".loki/loki.json", "--record"] },
	{ name: "record-hook-twice", events: true, files: { "a.txt": "x\n" }, git: true, after: { "a.txt": "y\n" }, args: [...hook("a.txt"), "--record"], runs: [[...hook("a.txt"), "--record"]] },
	{ name: "record-hook-missing-target", events: true, args: ["hook", "--record"] },
	{ name: "record-scan-invalid", events: true, files: { "a.txt": "" }, git: true, args: ["scan", "--record"] },
	{ name: "record-hook-help", events: true, args: ["hook", "--help", "--record"] },
	{ name: "record-shell-destructive", events: true, files: { "a.txt": "" }, git: true, args: ["shell", "--harness", "claude", "--record"], stdin: { tool_name: "Bash", cwd: "<ROOT>", tool_input: { command: "git reset --hard HEAD" } } },
	{ name: "record-shell-redirect-source", events: true, files: { "a.txt": "" }, git: true, args: ["shell", "--harness", "claude", "--record"], stdin: { tool_name: "Bash", cwd: "<ROOT>", tool_input: { command: "echo x > src/app.py" } } },
	{ name: "record-shell-redirect-other", events: true, files: { "a.txt": "" }, git: true, args: ["shell", "--harness", "claude", "--record"], stdin: { tool_name: "Bash", cwd: "<ROOT>", tool_input: { command: "cat a.txt > b.txt" } } },
	{ name: "record-shell-review", events: true, files: { "a.txt": "" }, git: true, args: ["shell", "--harness", "claude", "--record"], stdin: { tool_name: "Bash", cwd: "<ROOT>", tool_input: { command: "git commit -m x" } } },
	{ name: "record-shell-git-option", events: true, files: { "a.txt": "" }, git: true, args: ["shell", "--harness", "claude", "--record"], stdin: { tool_name: "Bash", cwd: "<ROOT>", tool_input: { command: "git diff --output=x" } } },
	{ name: "record-shell-syntax", events: true, files: { "a.txt": "" }, git: true, args: ["shell", "--harness", "claude", "--record"], stdin: { tool_name: "Bash", cwd: "<ROOT>", tool_input: { command: "echo 'x" } } },
	{ name: "record-shell-ok", events: true, files: { "a.txt": "" }, git: true, args: ["shell", "--harness", "claude", "--record"], stdin: { tool_name: "Bash", cwd: "<ROOT>", tool_input: { command: "git status" } } },
	{ name: "record-shell-bad-envelope", events: true, args: ["shell", "--harness", "claude", "--record"], rawStdin: "[]" },
	// --- verify-installation (ownership and modes; /usr/bin is root's, the temp tree the test user's) ---
	{ name: "verify-pass", args: ["verify-installation", "--engine", "/usr/bin/env", "--registration", "/usr/bin/git", "--agent-uid", "99999"] },
	{ name: "verify-symlink", args: ["verify-installation", "--engine", "/usr/bin/env", "--registration", "/usr/bin/sh", "--agent-uid", "99999"] },
	{ name: "verify-agent-owned", files: { "eng.py": "x" }, args: ["verify-installation", "--engine", "<ROOT>/eng.py", "--registration", "/usr/bin/git", "--registration", "<ROOT>/missing", "--agent-uid", "<UID>"] },
	{ name: "verify-world-writable", files: { "eng.py": "x" }, chmod: { "eng.py": 0o666 }, args: ["verify-installation", "--engine", "<ROOT>/eng.py", "--registration", "/usr/bin/git", "--agent-uid", "99999"] },
	{ name: "verify-root-agent", args: ["verify-installation", "--engine", "/usr/bin/env", "--registration", "/usr/bin/git", "--agent-uid", "0"] },
	{ name: "verify-directory", args: ["verify-installation", "--engine", "/usr/bin", "--registration", "/usr/bin/git", "--agent-uid", "99999"] },
	{ name: "verify-missing-args", args: ["verify-installation", "--engine", "x"] },
	{ name: "verify-bad-uid", args: ["verify-installation", "--engine", "x", "--registration", "y", "--agent-uid", "me"] },
	{ name: "verify-help", args: ["verify-installation", "-h"] },
	// --- apply: a checked multi-file transaction (the manifest is an untracked file of the work tree) ---
	...applyCases(),
	// --- Elixir: the elixir command's tiers and the hook's Credo/Sobelow comparison, with a stand-in Mix ---
	...elixirCases(),
	// --- scan --online: registry lookups against a stand-in registry (missing → 404, broken → 503, redirect → 302) ---
	...onlineCases(),
	// --- daemon management without a daemon (a serving daemon is exercised by tests/daemon.rs) ---
	{ name: "daemon-status", env: { HOME: "/nonexistent-loki-home" }, args: ["daemon", "status"] },
	{ name: "daemon-stop", env: { HOME: "/nonexistent-loki-home" }, args: ["daemon", "stop"] },
	{ name: "daemon-path-too-long", env: { HOME: `/${"h".repeat(100)}` }, args: ["daemon", "status"] },
	{ name: "daemon-serve-path-too-long", env: { HOME: `/${"h".repeat(100)}` }, args: ["daemon", "serve"] },
	{ name: "daemon-bad-action", args: ["daemon", "restart"] },
	{ name: "daemon-missing-action", args: ["daemon"] },
	{ name: "daemon-help", args: ["daemon", "--help"] },
];

function onlineCases() {
	// loki.py reaches the stand-in through this sitecustomize (urlopen rewritten); the Rust engine through
	// LOKI_REGISTRY_OVERRIDE itself. node_modules keeps it out of every scan.
	const site = {
		"node_modules/.site/sitecustomize.py": [
			"import os, urllib.request",
			"base = os.environ.get('LOKI_REGISTRY_OVERRIDE')",
			"if base:",
			"    original = urllib.request.urlopen",
			"    def urlopen(request, *args, **kwargs):",
			"        url = request.full_url if hasattr(request, 'full_url') else request",
			"        moved = base.rstrip('/') + '/' + url.split('://', 1)[1]",
			"        if hasattr(request, 'full_url'):",
			"            request.full_url = moved",
			"        else:",
			"            request = moved",
			"        return original(request, *args, **kwargs)",
			"    urllib.request.urlopen = urlopen",
			"",
		].join("\n"),
		".gitignore": "node_modules/\n",
	};
	const env = { LOKI_REGISTRY_OVERRIDE: "<REGISTRY>", PYTHONPATH: "<ROOT>/node_modules/.site" };
	const online = (name, files, extra = {}) => ({ name: `online-${name}`, files: { ...site, ...files }, git: true, env: { ...env, ...(extra.env ?? {}) }, args: ["scan", "--online", ...(extra.args ?? [])] });
	return [
		online("npm", { "package.json": JSON.stringify({ dependencies: { "left-pad": "1", "missing-pkg": "1", "@scope/missing-thing": "1" }, devDependencies: { "broken-tool": "1", "redirect-pkg": "1" } }) }),
		online("private", { "package.json": JSON.stringify({ dependencies: { "@acme/missing-internal": "1", "missing-public": "1" } }), ...cfg({ private_packages: ["@acme/"] }) }),
		online("pypi-crates-go-hex", {
			"requirements.txt": "requests==2.0\nmissing_lib>=1\n# comment\n-r other.txt\n",
			"Cargo.toml": "[package]\nname = 'x'\n[dependencies]\nserde = '1'\nmissing-crate = '1'\n",
			"go.mod": "module example.com/m\n\nrequire (\n\tgithub.com/Missing/Mod v1.0.0\n\tgolang.org/x/text v0.3.0\n)\nrequire example.com/broken v1.0.0\n",
			"mix.exs": "defp deps do\n  [{:phoenix, \"~> 1.7\"}, {:missing_hex, \"~> 0.1\"}]\nend\n",
		}),
		online("clean", { "package.json": JSON.stringify({ dependencies: { "left-pad": "1" } }) }),
		online("none", { "a.txt": "x\n" }),
		online("strict", { "package.json": JSON.stringify({ dependencies: { "missing-pkg": "1" } }) }, { args: ["--strict"] }),
	];
}

function elixirCases() {
	const mixProject = (lib, extra = {}) => ({
		"mix.exs": "defmodule App.MixProject do\nend\n",
		".credo.exs": "%{configs: []}\n",
		".gitignore": "_build/\ndeps/\n",
		"lib/app.ex": lib,
		"deps/credo/mix.exs": "x",
		"deps/sobelow/mix.exs": "x",
		"deps/dialyxir/mix.exs": "x",
		"_build/dev/lib/credo/ebin/.keep": "",
		"_build/dev/lib/jason/ebin/.keep": "",
		"_build/dev/lib/sobelow/ebin/.keep": "",
		...extra,
	});
	const tools = { mix: MIX, elixir: ELIXIR };
	const phoenix = cfg({ rule_packs: ["core", "phoenix"] });
	const clean = "defmodule App do\n  def ok, do: 1\nend\n";
	return [
		{ name: "elixir-project-pass", bin: tools, files: mixProject(clean), args: ["elixir", "--project", "<ROOT>"] },
		{ name: "elixir-fast-fail", bin: tools, files: mixProject("# UNFORMATTED\n# CREDOBAD\n"), args: ["elixir", "--project", "<ROOT>", "--tier", "fast"] },
		{ name: "elixir-deep-json", bin: tools, files: mixProject("# WARNME\n# SOBELOW_HIGH\n"), args: ["elixir", "--project", "<ROOT>", "--tier", "deep", "--json"] },
		{ name: "elixir-files", bin: tools, files: mixProject(clean, { "lib/b.ex": "# CREDOBAD\n" }), args: ["elixir", "--project", "<ROOT>", "--file", "lib/b.ex", "--file", "./lib/app.ex", "--tier", "fast"] },
		{ name: "elixir-file-missing", bin: tools, files: mixProject(clean), args: ["elixir", "--project", "<ROOT>", "--file", "lib/nope.ex"] },
		{ name: "elixir-file-outside", bin: tools, files: { ...mixProject(clean), "../x.ex": "" }, args: ["elixir", "--project", "<ROOT>", "--file", "../x.ex"] },
		{ name: "elixir-sobelow-not-json", bin: tools, files: mixProject("# NOTJSON\n"), args: ["elixir", "--project", "<ROOT>"] },
		{ name: "elixir-deps-missing", bin: tools, files: { "mix.exs": "x", "lib/app.ex": clean }, args: ["elixir", "--project", "<ROOT>", "--tier", "deep"] },
		{ name: "elixir-no-mix", files: mixProject(clean), args: ["elixir", "--project", "<ROOT>", "--tier", "fast"] },
		{ name: "elixir-no-project", args: ["elixir", "--project", "<ROOT>"] },
		{ name: "elixir-bad-tier", args: ["elixir", "--tier", "slow"] },
		{ name: "elixir-record-evidence", events: true, bin: tools, files: mixProject("# CREDOBAD\n# SOBELOW_HIGH\n"), args: ["elixir", "--project", "<ROOT>", "--record"] },
		{ name: "elixir-credo-new", bin: tools, files: mixProject("# CREDOBAD old\n"), git: true, after: { "lib/app.ex": "# CREDOBAD old\n# CREDOBAD new\n" }, args: ["elixir", "--project", "<ROOT>", "--credo-new"] },
		{ name: "elixir-credo-new-json-clean", bin: tools, files: mixProject(clean), git: true, after: { "lib/app.ex": clean.replace("1", "2") }, args: ["elixir", "--project", "<ROOT>", "--credo-new", "--json"] },
		{ name: "elixir-credo-new-config-change", bin: tools, files: mixProject(clean), git: true, after: { ".credo.exs": "%{}\n" }, args: ["elixir", "--project", "<ROOT>", "--credo-new"] },
		{ name: "elixir-credo-new-no-commits", bin: tools, files: mixProject(clean), gitInit: true, args: ["elixir", "--project", "<ROOT>", "--credo-new"] },
		{ name: "elixir-hook-credo", bin: tools, files: mixProject(clean), git: true, after: { "lib/app.ex": "defmodule App do\n  # CREDOBAD\nend\n" }, args: hook("lib/app.ex") },
		{ name: "elixir-hook-format", bin: tools, files: mixProject(clean), git: true, after: { "lib/app.ex": "# UNFORMATTED\n" }, args: hook("lib/app.ex") },
		{ name: "elixir-hook-phoenix-sobelow", bin: tools, files: { ...mixProject(clean), ...phoenix }, git: true, after: { "lib/app.ex": "# SOBELOW_HIGH\n# SOBELOW_LOW\n# SOBELOW_PARTIAL\n" }, args: hook("lib/app.ex") },
		{ name: "elixir-hook-sobelow-config-change", bin: tools, files: { ...mixProject(clean), ...phoenix, "config/config.exs": "x\n" }, git: true, after: { "lib/app.ex": "# x\n", "config/config.exs": "y\n" }, args: hook("lib/app.ex") },
		{ name: "elixir-hook-strict", bin: tools, files: { ...mixProject(clean), ...phoenix }, git: true, env: { LOKI_STRICT: "1" }, after: { "lib/app.ex": "# SOBELOW_MEDIUM\n# SOBELOW_PARTIAL\n" }, args: hook("lib/app.ex") },
		{ name: "elixir-hook-two-files", bin: tools, files: mixProject(clean, { "lib/b.ex": clean }), git: true, after: { "lib/app.ex": "# CREDOBAD 1\n", "lib/b.ex": "# UNFORMATTED\n" }, args: hook("lib/app.ex", "lib/b.ex") },
		{ name: "elixir-hook-no-credo-build", bin: tools, files: Object.fromEntries(Object.entries(mixProject(clean)).filter(([path]) => !path.startsWith("_build/dev/lib/credo"))), git: true, after: { "lib/app.ex": "# x\n" }, args: hook("lib/app.ex") },
		{ name: "elixir-hook-no-mix-project", bin: tools, files: { "lib/app.ex": clean }, git: true, after: { "lib/app.ex": "# x\n" }, args: hook("lib/app.ex") },
		{ name: "elixir-scan", bin: tools, files: mixProject("# CREDOBAD\n"), git: true, args: ["scan"] },
	];
}

function applyCases() {
	const manifest = (ops) => ({ "m.json": typeof ops === "string" ? ops : JSON.stringify(ops) });
	const base = { "a.txt": "x\n", "bin/run.sh": { exec: "#!/bin/sh\necho run\n" }, "app/m.py": "VALUE = 1\n" };
	const apply = (name, ops, extra = {}) => ({
		name: `apply-${name}`,
		files: { ...base, ...(extra.files ?? {}) },
		git: extra.git ?? true,
		gitInit: extra.gitInit,
		after: { ...manifest(ops), ...(extra.after ?? {}) },
		env: extra.env,
		links: extra.links,
		tree: true,
		args: ["apply", "--manifest", "m.json", ...(extra.args ?? [])],
	});
	return [
		apply("write", [{ op: "write", path: "a.txt", content: "y\n" }, { op: "write", path: "docs/new.md", content: "# New\n" }]),
		apply("check-only", [{ op: "write", path: "a.txt", content: "y\n" }], { args: ["--check-only"] }),
		apply("delete", [{ op: "delete", path: "a.txt" }]),
		apply("move-executable", [{ op: "move", path: "bin/run.sh", to: "bin/go.sh" }]),
		apply("secret", [{ op: "write", path: "a.txt", content: "key = '@@AWS@@'\n" }]),
		apply("python-stub", [{ op: "write", path: "app/m.py", content: "def stub():\n    pass\n" }]),
		apply("protected", [{ op: "write", path: ".ruff.toml", content: "x = 1\n" }], { env: { LOKI_ALLOW_PROTECTED: "1" } }),
		apply("existing-protected-change", [{ op: "write", path: "a.txt", content: "y\n" }], { after: { ".ruff.toml": "x = 1\n" } }),
		apply("not-a-list", { op: "write" }),
		apply("empty", []),
		apply("bad-json", "[{"),
		apply("bad-op", [{ op: "copy", path: "a.txt" }]),
		apply("not-object", ["a.txt"]),
		apply("missing-path", [{ op: "write", content: "x" }]),
		apply("noncanonical-dot", [{ op: "write", path: "./a.txt", content: "y" }]),
		apply("noncanonical-slashes", [{ op: "write", path: "docs//a.md", content: "y" }]),
		apply("noncanonical-trailing", [{ op: "write", path: "docs/", content: "y" }]),
		apply("absolute", [{ op: "write", path: "/tmp/x", content: "y" }]),
		apply("parent", [{ op: "write", path: "../x", content: "y" }]),
		apply("git-metadata", [{ op: "write", path: ".git/config", content: "y" }]),
		apply("node-modules", [{ op: "write", path: "node_modules/x.js", content: "y" }]),
		apply("duplicate", [{ op: "write", path: "a.txt", content: "y" }, { op: "delete", path: "a.txt" }]),
		apply("content-not-string", [{ op: "write", path: "a.txt", content: 3 }]),
		apply("delete-missing", [{ op: "delete", path: "nope.txt" }]),
		apply("move-same", [{ op: "move", path: "a.txt", to: "a.txt" }]),
		apply("move-onto-existing", [{ op: "move", path: "a.txt", to: "app/m.py" }]),
		apply("move-missing-to", [{ op: "move", path: "a.txt" }]),
		apply("symlink-target", [{ op: "write", path: "link.txt", content: "y" }], { links: { "link.txt": "a.txt" } }),
		apply("symlinked-parent", [{ op: "write", path: "lnk/x.txt", content: "y" }], { links: { lnk: "app" } }),
		apply("no-commits", [{ op: "write", path: "a.txt", content: "y\n" }], { git: false, gitInit: true }),
		{ name: "apply-missing-manifest", files: base, git: true, tree: true, args: ["apply", "--manifest", "absent.json"] },
		{ name: "apply-missing-args", args: ["apply"] },
	];
}

const gitRun = (cwd, ...args) => {
	const result = spawnSync("git", ["-c", "user.name=loki", "-c", "user.email=loki@example.invalid", "-c", "commit.gpgsign=false", "-c", "init.defaultBranch=main", ...args], {
		cwd,
		env: { PATH: process.env.PATH, HOME: cwd, GIT_AUTHOR_DATE: "2026-01-01T00:00:00Z", GIT_COMMITTER_DATE: "2026-01-01T00:00:00Z" },
		encoding: "utf8",
	});
	if (result.status !== 0) throw new Error(`git ${args.join(" ")}: ${result.stderr}`);
};

function place(root, files) {
	for (const [path, content] of Object.entries(files ?? {})) {
		const target = join(root, path);
		if (content === null) {
			rmSync(target);
			continue;
		}
		mkdirSync(dirname(target), { recursive: true });
		if (typeof content === "string") writeFileSync(target, expand(content));
		else if (content.bytes) writeFileSync(target, Buffer.from(content.bytes));
		else {
			writeFileSync(target, content.exec);
			chmodSync(target, 0o755);
		}
	}
}

function tree(root) {
	const found = {};
	const walk = (dir) => {
		for (const name of readdirSync(dir).sort()) {
			const path = join(dir, name);
			const relative = path.slice(root.length + 1);
			const info = lstatSync(path);
			if (name === ".git") continue;
			if (info.isSymbolicLink()) found[relative] = `link ${readlinkSync(path)}`;
			else if (info.isDirectory()) walk(path);
			else found[relative] = `${(info.mode & 0o777).toString(8)} ${createHash("sha256").update(readFileSync(path)).digest("hex")}`;
		}
	};
	walk(root);
	return found;
}

/** Every `--record` event under HOME, oldest first, with its wall-clock fields zeroed and its paths scrubbed. */
function events(home, scrub) {
	const state = join(home, ".local/state/loki");
	const found = [];
	let directories = [];
	try {
		directories = readdirSync(state).sort();
	} catch {
		return found;
	}
	for (const directory of directories)
		for (const line of readFileSync(join(state, directory, "events.jsonl"), "utf8").split("\n").filter(Boolean)) {
			const event = JSON.parse(scrub(line));
			if ("time" in event) event.time = 0;
			if ("duration_ms" in event) event.duration_ms = 0;
			found.push(event);
		}
	return found;
}

function run(testCase) {
	const top = realpathSync(mkdtempSync(join(tmpdir(), "loki-hook-")));
	try {
		const bin = join(top, "bin");
		mkdirSync(bin);
		for (const tool of ["git", "sh", "env"]) symlinkSync(`/usr/bin/${tool}`, join(bin, tool));
		symlinkSync(python, join(bin, "python3"));
		if (testCase.node) symlinkSync(process.execPath, join(bin, "node"));
		for (const [name, script] of Object.entries(testCase.bin ?? {})) {
			writeFileSync(join(bin, name), script);
			chmodSync(join(bin, name), 0o755);
		}
		const work = join(top, "w");
		mkdirSync(work);
		place(work, testCase.files);
		if (testCase.typescript) {
			mkdirSync(join(work, "node_modules"), { recursive: true });
			symlinkSync(realpathSync(join(here, "../../../node_modules/typescript")), join(work, "node_modules/typescript"));
		}
		if (testCase.git || testCase.gitInit) {
			gitRun(work, "init", "-q");
			if (testCase.git) {
				gitRun(work, "add", "-A");
				gitRun(work, "commit", "-q", "--allow-empty", "-m", "base");
			}
		}
		place(work, testCase.after);
		for (const [path, target] of Object.entries(testCase.links ?? {})) symlinkSync(target, join(work, path));
		for (const path of testCase.fifo ?? []) spawnSync("mkfifo", [join(work, path)]);
		for (const [path, mode] of Object.entries(testCase.chmod ?? {})) chmodSync(join(work, path), mode);
		if (testCase.conflict) {
			gitRun(work, "checkout", "-q", "-b", "other");
			writeFileSync(join(work, "a.txt"), "other\n");
			gitRun(work, "commit", "-q", "-am", "other");
			gitRun(work, "checkout", "-q", "main");
			writeFileSync(join(work, "a.txt"), "main\n");
			gitRun(work, "commit", "-q", "-am", "main");
			spawnSync("git", ["merge", "-q", "other"], { cwd: work, env: { PATH: process.env.PATH, HOME: work } });
		}
		const root = testCase.rootSub ? join(work, testCase.rootSub) : work;
		const fill = (text) =>
			text.split("<ROOT>").join(root).split("<PYTHON>").join(python).split("<UID>").join(String(process.getuid())).split("<REGISTRY>").join(registry);
		const stdin = testCase.rawStdin ?? (testCase.stdin === undefined ? "" : fill(expand(JSON.stringify(testCase.stdin))));
		const env = { PATH: bin, HOME: top, LOKI_DAEMON: "0" };
		for (const [key, value] of Object.entries(testCase.env ?? {})) env[key] = fill(value);
		const once = (args) =>
			spawnSync(python, [engine, ...(testCase.noRoot ? [] : ["--root", root]), ...args.map(fill)], { cwd: root, env, input: stdin, encoding: "utf8" });
		const result = once(testCase.args);
		const scrub = (text) =>
			text
				.split(work)
				.join("<ROOT>")
				.split(top)
				.join("<TOP>")
				.replace(/"duration_ms": [0-9.]+/g, '"duration_ms": 0')
				.replace(/\.cache\/loki\/d\/[0-9a-f]{16}/g, ".cache/loki/d/<ID>");
		const expected = { status: result.status, stdout: scrub(result.stdout), stderr: scrub(result.stderr) };
		if (testCase.tree) expected.tree = tree(work);
		if (testCase.runs)
			expected.runs = testCase.runs.map((args) => {
				const next = once(args);
				return { status: next.status, stdout: scrub(next.stdout), stderr: scrub(next.stderr) };
			});
		if (testCase.events) expected.events = events(top, scrub);
		return expected;
	} finally {
		rmSync(top, { recursive: true, force: true });
	}
}

// The stand-in registry for `scan --online`: the status follows the package name in the path.
const server = spawn(python, ["-c", [
	"import http.server, sys",
	"class Handler(http.server.BaseHTTPRequestHandler):",
	"    def do_GET(self):",
	"        path = self.path",
	"        status = 404 if 'missing' in path.lower() else 503 if 'broken' in path else 200",
	"        if 'redirect' in path and not path.endswith('/landed'):",
	"            self.send_response(302)",
	"            self.send_header('Location', path + '/landed')",
	"            self.end_headers()",
	"            return",
	"        self.send_response(status)",
	"        self.send_header('Content-Length', '2')",
	"        self.end_headers()",
	"        self.wfile.write(b'{}')",
	"    def log_message(self, *args):",
	"        pass",
	"server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)",
	"print(server.server_address[1], flush=True)",
	"server.serve_forever()",
].join("\n")], { stdio: ["ignore", "pipe", "inherit"] });
const registry = await new Promise((resolve) => server.stdout.once("data", (chunk) => resolve(`http://127.0.0.1:${String(chunk).trim()}`)));
let recorded;
try {
	recorded = cases.map((testCase) => ({ ...testCase, expected: run(testCase) }));
} finally {
	server.kill();
}
writeFileSync(
	join(here, "hook-corpus.json"),
	`${JSON.stringify({ engine: "packages/coding-agent/src/ultron/loki-engine/loki.py", tokens, cases: recorded }, null, "\t")}\n`,
);
console.log(`recorded ${recorded.length} cases`);
