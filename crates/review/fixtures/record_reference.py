"""Records autoreview_reference.py for crates/review tests (src/reference*.rs): the TypeScript scanner, the Python
indexer (the module's own `index_python`, which the host's embedded script must reproduce), the YAML reader, the
Terraform scanner, the module map, and whole references over small git repositories built here (index, lookups,
views, the block, facts, structural findings and served requests). Run by generate_reference.mjs; prints
reference.json.

Repositories are built in a temp dir from recorded steps (written and removed files, git commands under a fixed
identity and clock; `base` and `head` are tags), so the Rust test rebuilds the same commits and runs real git.
"""
import json
import os
import shutil
import sys
import tempfile

sys.path.insert(0, sys.argv[1])
import autoreview_deep as deep  # noqa: E402
import autoreview_reference as ref  # noqa: E402
import review_api as ra  # noqa: E402

GIT_ENV = {
    "GIT_AUTHOR_NAME": "Ultron Fixture", "GIT_AUTHOR_EMAIL": "fixture@example.invalid",
    "GIT_COMMITTER_NAME": "Ultron Fixture", "GIT_COMMITTER_EMAIL": "fixture@example.invalid",
    "GIT_AUTHOR_DATE": "2026-01-01T00:00:00+0000", "GIT_COMMITTER_DATE": "2026-01-01T00:00:00+0000",
    "GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": "/dev/null", "LC_ALL": "C", "TZ": "UTC",
}
os.environ.update(GIT_ENV)
os.environ.pop("GIT_DIR", None)
os.environ.pop("GIT_WORK_TREE", None)
GH = "$" + "{{"


def file_index(index):
    return {"path": index.path, "lang": index.lang, "symbols": [vars(s) for s in index.symbols],
            "imports": {k: list(v) for k, v in index.imports.items()}, "calls": [vars(c) for c in index.calls],
            "families": [dict(vars(f), items=list(f.items)) for f in index.families],
            "exports": sorted(index.exports), "parsed": index.parsed}


def yv(value):
    if isinstance(value, dict):
        return {"$map": [[k, yv(v)] for k, v in value.items()]}
    if isinstance(value, list):
        return [yv(v) for v in value]
    return value


def node_json(node):
    if node is None:
        return None
    return {"value": yv(node.value), "line": node.line, "lines": [[k, n] for k, n in node.lines.items()]}


def tf_json(block):
    return {"kind": block.kind, "type": block.type, "name": block.name, "path": block.path, "line": block.line,
            "end": block.end, "address": block.address, "attrs": [[k, v] for k, v in block.attrs.items()],
            "nested": [[k, [[a, b] for a, b in v.items()]] for k, v in block.nested.items()], "refs": sorted(block.refs)}


def attempt(fn):
    try:
        return {"ok": fn()}
    except Exception as error:  # noqa: BLE001
        return {"error": type(error).__name__}


# --- TypeScript ----------------------------------------------------------------------------------------------------

TS_TEXTS = [
    "export const NAMES = ['x', 'y', 'z'];\nexport function parse(input: string): number {\n  return Number(input);\n}\n"
    "export class Client {\n  fetch(url: string) {\n    return parse(url);\n  }\n}\n",
    "import { parse, Client } from './util';\nconst c = new Client();\nexport default function main() {\n"
    "  return parse(c.fetch('u'));\n}\n",
    # Imports of every shape, requires, aliases, a re-export at the start of a later line (not read: no multiline).
    "import Def, { a as b, type T, c } from \"../lib/mod\";\nimport * as ns from './ns';\nimport type { X } from './x';\n"
    "const { q, r: s } = require('./req');\nconst whole = require(\"./whole\");\nexport { a, b as bb };\n",
    "export { first, second as other };\nexport const x = 1;\n",
    # Classes with methods, keywords that look like methods, getters, arrows, types, interfaces, enums.
    "export abstract class Shape extends base.Thing {\n  private readonly size = 3;\n  constructor(a: number) {\n"
    "    super(a);\n  }\n  public async area<T>(x: T): Promise<number> {\n    if (x) {\n      return this.side(x) * 2;\n"
    "    }\n    for (const y of z) { go(y); }\n    return 0;\n  }\n  get side(v) {\n    return helper.calc(v);\n  }\n}\n"
    "export const handler = async (req: Request): Promise<void> => {\n  await process(req);\n};\n"
    "const single = v => v * 2;\nexport interface Props {\n  a: string;\n}\nexport type Alias = string;\n"
    "declare enum Color { Red, Green }\nexport let MAX_SIZE = 10;\nfunction* gen() {\n  yield 1;\n}\n"
    "// call(commented)\nfoo.bar.baz(1); new Thing(); typeof x(1); $el(2); a$b(3); _x.y(4); 1$f(5); x.2$g(6);\n",
    # Strings and comments around braces; a block that never closes; a function with no brace.
    "function tricky() {\n  const s = \"}\"; const t = '{'; const u = `}`; // }\n  return 1;\n}\n"
    "function noBrace()\n  ;\nfunction open() {\n  let x = 1;\n",
    # Literal arrays: named, multi-line, with trailing comma, too short, mixed.
    "const KINDS: string[] = [\n  'a',\n  \"b\",\n  `c`,\n];\nlet single = ['only'];\nfoo(['p', 'q']);\n"
    "const mixed = ['a', 1, 'b'];\nexport const ROUTES = {\n  list: ['x', 'y'],\n};\n",
    # Non-ASCII text before an array and in names.
    "const café = 'é';\nconst LIST = ['ü', 'ö', 'ä'];\nfunction naïve() {\n  return café();\n}\n",
    "",
]
ts_cases = [{"path": f"web/f{i}.ts", "text": text, "index": file_index(ref.index_ts(f"web/f{i}.ts", text))}
            for i, text in enumerate(TS_TEXTS)]
BLOCK_LINES = [
    ["function f() {", "  return 1;", "}"],
    ["function f()", "{", "  x();", "}", "after"],
    ["function f()", "", "{", "}"],
    ["const s = '{';", "next"],
    ["a { b {", "} }", "c"],
    ["x { // }", "}"],
    ["open {", "still"],
    ["t = `{` + \"}\" + '{' {", "}"],
    ["}", "{", "}"],
]
block_ends = [[lines, start, ref._ts_block_end(lines, start)] for lines in BLOCK_LINES for start in range(1, len(lines) + 1)]

# --- Python (the module's own indexer: the host's embedded script must match it) -----------------------------------

PY_TEXTS = [
    'def load(path, mode="r"):\n    """Read a file.\n\n    More."""\n    return open(path, mode).read()\n\n\n'
    "class Base:\n    def run(self):\n        return 1\n",
    "from pkg.helper import load, Base\nimport os.path, json as j\nfrom . import sibling\nfrom ..up import thing as other\n"
    "from .mod import *\n\nKINDS = [\"a\", \"b\", \"c\"]\n_private: tuple = (\"x\", \"y\")\nCONF = {\"k1\": 1, \"k2\": 2}\n\n\n"
    "class App(Base, metaclass=Meta):\n    \"\"\"An app.\"\"\"\n    NAMES = {\"p\", \"q\"}\n    level: int = 3\n\n"
    "    @property\n    @functools.cache\n    def run(self) -> int:\n        x = [\"m\", \"n\"]\n        return load(\"x\") + self.go()\n\n"
    "    async def go(self, *args, key=None, **kw):\n        def inner():\n            return helper.deep.call()\n"
    "        return inner()\n\n\n@decorator(\"arg\")\ndef show(path, strict=False):\n"
    "    for item in [\"u\", \"v\", \"w\"]:\n        print(item)\n    return load(path)\n\n\n"
    "@pytest.mark.parametrize(\"case\", [\"one\", \"two\"])\ndef test_cases(case):\n    assert run(case)\n"
    "a, b = 1, 2\n(c, d) = (\"s\", \"t\")\nfoo()()\nobj[0].method()\n",
    "def broken(:\n    pass\n",
    "x = 1\n" * 3 + "\0",
    "",
    "def é():\n    return \"ü\"\n",
]
py_cases = [{"path": f"src/m{i}.py", "text": text, "index": file_index(ref.index_python(f"src/m{i}.py", text))}
            for i, text in enumerate(PY_TEXTS)]

# --- YAML ----------------------------------------------------------------------------------------------------------

SCALARS = ["", "  ", "~", "null", "Null", "NULL", "nil", "'a''b'", '"a\\"b\\\\c\\n"', "'", '"x\'', "[]", "[ ]",
           "[a, 'b, c', [d, e], {f: g}]", "{a: 1, b, 'c': [x, y], : z}", "{}", "{[a]: b}", "plain text # here", "[a,", "{a: b"]
scalars = [[text, attempt(lambda text=text: yv(ref._yaml_scalar(text)))] for text in SCALARS]
FLOWS = ["a, b", "a, [b, c], {d: e}", "'x, y', \"z, w\"", "a,,b", " , ", "a, b,", "]a, b[", "'unclosed, x"]
flows = [[text, ref._split_flow(text)] for text in FLOWS]
COMMENTS = ["a: b # c", "# whole", "a: 'b # c'", "a: \"b # c\" # d", "a:#b", "a: b\t# c", "url: http://x/#frag", "  "]
comments = [[text, ref._strip_comment(text)] for text in COMMENTS]
YAML_TEXTS = [
    "# a workflow\nname: CI\non:\n  push:\n    branches: [main, 'release/*']\n  workflow_dispatch:\nenv:\n"
    "  FOO: \"bar # not a comment\"\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n"
    "      - uses: actions/checkout@v4\n      - name: Test\n        run: |\n          echo one\n          echo two\n"
    "      - run: echo three\n    strategy:\n      matrix: {node: [18, 20]}\n",
    "",
    "# only comments\n\n",
    "---\na: 1\n---\nb: 2\n",
    "---\nkey: value\n...\nignored: yes\n",
    "- a\n- b: 1\n  c: 2\n-\n  - nested\n  - list\n- [x, y]\n- {k: v}\n-\n- last\n",
    "list:\n- one\n- two\nnext: 3\n",
    "folded: >\n  one\n  two\n\n  three\nkept: |+\n  a\n  # not a comment\n  b\nafter: x\n",
    "a:\n\tb: tab\n",
    "dup: 1\nother: 2\ndup: 3\n",
    "'quoted key': 1\n\"dq\": 2\n~: null key\nnull: also\nkey with: spaces\nurl: http://x:80\n",
    "a: 1\n  b: deeper\nc: 3\n",
    "a:\n  b: 1\n c: 2\nd: 4\n",
    "[a]: b\n",
    "x: {[a]: b}\n",
    "- - a\n  - b\n- - c\n",
    "a: 'it''s'\nb: \"q\\\"q\"\nc: 'x' # comment\n",
    "on:\n  pull_request_target:\n    types: [opened]\n",
    "jobs:\n  x:\n    steps:\n      - uses: a\n        with:\n          ref: " + GH + " github.head_ref }}\n",
    "not: valid: mapping\n- dash after map\nlast: 1\n",
    "key:\n",
    "a:\n" + "".join("  " * (i + 1) + f"k{i}:\n" for i in range(30)) + "  " * 31 + "leaf: 1\n",
]
yaml_cases = [[text, attempt(lambda text=text: node_json(ref.parse_yaml(text)))] for text in YAML_TEXTS]
DOC = ref.parse_yaml(YAML_TEXTS[0]).value
GETS = [["jobs", "build", "steps", 1, "run"], ["jobs", "build", "steps", 9], ["on", "push", "branches", 0], ["name", "x"],
        ["missing"], [], ["jobs", 0]]
yaml_gets = [[keys, yv(ref._yaml_get(DOC, *keys))] for keys in GETS]

# --- Terraform -----------------------------------------------------------------------------------------------------

TF_TEXTS = [
    'resource "aws_iam_role" "r" {\n  name = "r"\n  assume_role_policy = <<EOF\n{ "ref": "aws_s3_bucket.other.arn" }\nEOF\n'
    "  lifecycle {\n    prevent_destroy = true\n  }\n  tags = { a = var.name }\n}\n"
    'module "m" {\n  source = "./m"\n  role   = aws_iam_role.r.arn\n}\n',
    'data "aws_caller_identity" "me" {}\nvariable "region" {\n  default = "us-east-1" # comment\n}\n'
    'locals {\n  bucket = "${var.region}-logs" // trailing\n  ids = [for b in aws_s3_bucket.all : b.id]\n}\n'
    'output "o" {\n  value = data.aws_caller_identity.me.account_id\n}\nprovider "aws" {\n  region = local.region\n}\n'
    "terraform {\n  required_version = \">= 1.0\"\n}\n",
    'resource "aws_s3_bucket" "b" {\n  bucket = "b"\n  versioning {\n    enabled = true\n  }\n  tags = {\n    Name = "x"\n'
    "  }\n  lifecycle {\n    prevent_destroy = false\n    ignore_changes = [tags]\n  }\n  policy = <<-POLICY\n    {}\n"
    "  POLICY\n  depends_on = [module.vpc, aws_iam_role.r-1]\n}\n",
    'resource "unclosed" "x" {\n  a = 1\n',
    "# nothing here\nfoo = 1\n",
    'resource "a" {\n  x = 1\n}\nresource {\n}\n',
]
tf_cases = [[text, [tf_json(block) for block in ref.parse_terraform("infra/main.tf", text)]] for text in TF_TEXTS]

# --- Module map ----------------------------------------------------------------------------------------------------

MODULE_LISTS = [
    ["src/pkg/__init__.py", "src/pkg/app.py", "tests/test_app.py", "web/util.ts", "setup.py", "README.md"],
    ["a/util.py", "b/util.py", "util.py", "util"],
    ["__init__.py", "x/__init__.py", "x/y/z.py"],
]
module_maps = [[paths, ref._module_map(paths)] for paths in MODULE_LISTS]

# --- Repositories --------------------------------------------------------------------------------------------------

ROOT = os.path.realpath(tempfile.mkdtemp(prefix="review-reference-"))


def run_steps(base, steps):
    for step in steps:
        if step[0] == "write":
            path = os.path.join(base, step[1])
            os.makedirs(os.path.dirname(path), exist_ok=True)
            with open(path, "w", encoding="utf-8") as handle:
                handle.write(step[2])
        elif step[0] == "rm":
            os.remove(os.path.join(base, step[1]))
        else:
            code, out, err = ra._run_process(["git", *step[1:]], base, 30)
            if code != 0:
                raise SystemExit(f"step {step} failed: {err}{out}")


def write(path, text):
    return ["write", path, text]


def git(*args):
    return ["git", *args]


def commit(tag):
    return [git("add", "-A"), git("commit", "-q", "-m", tag), git("tag", tag)]


MIXED = [
    git("init", "-q", "-b", "main"),
    write("src/pkg/__init__.py", ""),
    write("src/pkg/helper.py", 'def load(path, mode="r"):\n    """Read a file."""\n    return open(path, mode).read()\n\n\n'
          "class Base:\n    def run(self):\n        return 1\n"),
    write("src/pkg/app.py", 'from pkg.helper import load, Base\n\nKINDS = ["a", "b", "c"]\n\n\nclass App(Base):\n    def run(self):\n'
          '        return load("x")\n\n\ndef show(path):\n    return load(path)\n'),
    write("src/pkg/other.py", 'KINDS = ["a", "b", "c"]\n\n\ndef consume():\n    from pkg.app import show\n    return show("p")\n'),
    write("tests/test_app.py", 'from pkg.app import KINDS, show\n\n\ndef test_show():\n    assert show("f") is not None\n'),
    write("web/util.ts", "export const NAMES = ['x', 'y', 'z'];\nexport function parse(input: string): number {\n"
          "  return Number(input);\n}\nexport class Client {\n  fetch(url: string) {\n    return parse(url);\n  }\n}\n"),
    write("web/main.ts", "import { parse, Client } from './util';\nconst c = new Client();\nexport default function main() {\n"
          "  return parse(c.fetch('u'));\n}\n"),
    *[write(f".github/workflows/{name}.yml", f"name: {name}\non: push\npermissions:\n  contents: read\njobs:\n  x:\n"
            "    runs-on: ubuntu-latest\n    steps:\n      - run: echo hi\n") for name in ("a", "b")],
    write("infra/main.tf", 'resource "aws_s3_bucket" "logs" {\n  bucket = "logs"\n  lifecycle {\n    prevent_destroy = true\n  }\n}\n'
          'resource "aws_s3_bucket" "data" {\n  bucket = "data"\n  lifecycle {\n    prevent_destroy = true\n  }\n}\n'
          'output "o" {\n  value = aws_s3_bucket.gone.id\n}\n'),
    write("infra/gone.tf", 'resource "aws_s3_bucket" "gone" {\n  bucket = "gone"\n}\n'),
    *commit("base"),
    write("src/pkg/app.py", 'from pkg.helper import load, Base\n\nKINDS = ["a", "b", "c", "d"]\n\n\nclass App(Base):\n'
          '    def run(self):\n        return load("x")\n\n\ndef show(path, strict=False):\n    return load(path)\n'),
    write("web/util.ts", "export const NAMES = ['x', 'y', 'z'];\nexport function parse(input: string, radix = 10): number {\n"
          "  return Number(input);\n}\nexport class Client {\n  fetch(url: string) {\n    return parse(url);\n  }\n}\n"),
    write(".github/workflows/new.yml", "name: New\non:\n  pull_request_target:\n    types: [opened]\njobs:\n  x:\n"
          "    runs-on: ubuntu-latest\n    permissions:\n      id-token: write\n    steps:\n      - uses: actions/checkout@v4\n"
          "        with:\n          ref: " + GH + " github.event.pull_request.head.sha }}\n      - run: |\n          echo \""
          + GH + " github.event.pull_request.title }}\"\n"),
    ["rm", "infra/gone.tf"],
    write("infra/fresh.tf", 'resource "aws_s3_bucket" "fresh" {\n  bucket = "fresh"\n}\n'),
    *commit("head"),
]
EXTERNAL = [
    git("init", "-q", "-b", "main"),
    write(".github/workflows/old.yml", "name: Old\non: push\npermissions:\n  contents: read\njobs:\n  a:\n"
          "    runs-on: ubuntu-latest\n    steps:\n      - run: echo hi\n"),
    *commit("base"),
    write(".github/workflows/review.yml", "name: Review\non:\n  pull_request:\n    types: [opened]\npermissions:\n"
          "  contents: read\n  pull-requests: write\n  id-token: write\njobs:\n  review:\n"
          "    uses: other-org/.github/.github/workflows/review.yml@main\n    secrets: inherit\n"),
    *commit("head"),
]
EDGE = [
    git("init", "-q", "-b", "main"),
    write("lib/core/__init__.py", "from .engine import Engine\n"),
    write("lib/core/engine.py", "from . import util_mod\nfrom ..shared import consts\n\n\nclass Engine:\n"
          "    def start(self):\n        return self.step()\n\n    def step(self):\n        return util_mod.tick()\n\n\n"
          "class Turbo(Engine):\n    def step(self):\n        return 2\n"),
    write("lib/core/util_mod.py", "def tick():\n    return 1\n"),
    write("lib/shared/consts.py", "MODES = (\"fast\", \"slow\", \"safe\")\n"),
    write("lib/use_engine.py", "import lib.core.engine as eng\nfrom lib.core import engine\n\n\ndef go():\n"
          "    e = eng.Engine()\n    engine.Engine().start()\n    return e.start()\n"),
    write("lib/registry.py", "MODES = [\"fast\", \"slow\", \"safe\"]\n"),
    write("tests/test_engine.py", "from lib.core.engine import Engine\n\n\ndef test_start():\n    assert Engine().start()\n"),
    write("app/index.ts", "export * from './api';\nexport function boot() {\n  return 1;\n}\n"),
    write("app/api.ts", "const helpers = require('./helpers');\nconst { fmt } = require('./helpers');\n"
          "export const LEVELS = ['low', 'medium', 'high', 'critical'];\nexport function call(x) {\n  return helpers.wrap(fmt(x));\n}\n"),
    write("app/helpers.js", "function wrap(x) {\n  return x;\n}\nfunction fmt(x) {\n  return String(x);\n}\n"
          "module.exports = { wrap, fmt };\n"
          # A method defined in exactly three classes: an untyped `x.render()` reaches all three.
          + "".join(f"class {name} {{\n  render() {{\n    return 1;\n  }}\n}}\n" for name in ("A", "B", "C"))
          + "function paint(x) {\n  return x.render();\n}\n"
          # Ten members, five shared with LEVELS_TEN in api.ts: below the 0.6 overlap that makes a family match.
          + "const HALF = ['a0', 'a1', 'a2', 'a3', 'a4', 'b0', 'b1', 'b2', 'b3', 'b4'];\n"
          + "const MOST = ['a0', 'a1', 'a2', 'a3', 'a4', 'a5', 'b0', 'b1', 'b2', 'b3'];\n"),
    write("app/levels.ts", "export const ORDER = ['low', 'medium', 'high'];\n"),
    write("app/main.spec.ts", "import { call } from './api';\nimport * as boot from './index';\n"
          "describe('x', () => {\n  it('calls', () => {\n    call(1);\n    boot.boot();\n  });\n});\n"),
    write(".github/workflows/called.yml", "on: workflow_call\npermissions:\n  id-token: write\njobs:\n  a:\n"
          "    runs-on: ubuntu-latest\n    steps:\n      - run: echo $ACTIONS_ID_TOKEN_REQUEST_URL\n"),
    write(".github/workflows/one.yml", "on: push\npermissions: read-all\njobs:\n  a:\n    runs-on: x\n"),
    write(".github/workflows/two.yml", "on: push\njobs:\n  a:\n    permissions:\n      contents: read\n    runs-on: x\n"),
    write("infra/db.tf", 'data "aws_ami" "base" {\n  most_recent = true\n}\nresource "aws_db_instance" "main" {\n'
          "  ami = data.aws_ami.base.id\n  lifecycle {\n    prevent_destroy = true\n  }\n}\n"
          'resource "aws_db_instance" "replica" {\n  lifecycle {\n    prevent_destroy = true\n  }\n}\n'),
    write("infra/net.tf", 'resource "aws_vpc" "v" {\n  cidr = "10.0.0.0/16"\n}\n'),
    *commit("base"),
    write("lib/core/engine.py", "from . import util_mod\nfrom ..shared import consts\n\n\nclass Engine:\n"
          "    def start(self, fast=True):\n        return self.step()\n\n    def step(self):\n        return util_mod.tick()\n\n\n"
          "class Turbo(Engine):\n    def step(self):\n        return 3\n\n\ndef added():\n    return Engine().start()\n"),
    write("app/api.ts", "const helpers = require('./helpers');\nconst { fmt } = require('./helpers');\n"
          "export const LEVELS = ['low', 'medium', 'high', 'critical', 'fatal'];\nexport function call(x, y) {\n"
          "  return helpers.wrap(fmt(x));\n}\n"
          "export const TEN = ['a0', 'a1', 'a2', 'a3', 'a4', 'a5', 'a6', 'a7', 'a8', 'a9'];\n"),
    write(".github/workflows/deploy.yml", "on:\n  - issue_comment\n  - push\njobs:\n  guarded:\n"
          "    if: github.event.pull_request.head.repo.full_name == github.repository\n    steps:\n"
          "      - uses: actions/checkout@v4\n        with:\n          ref: " + GH + " github.head_ref }}\n"
          "  local:\n    uses: ./.github/workflows/called.yml\n  open:\n    steps:\n      - uses: actions/checkout@v3\n"
          "        with:\n          ref: refs/pull/head_ref\n      - run: echo \"" + GH + " github.event.comment.body }}\"\n"
          "      - run: echo \"" + GH + " inputs.name }}\"\n"),
    write("infra/db.tf", 'resource "aws_db_instance" "main" {\n  lifecycle {\n    prevent_destroy = true\n  }\n}\n'
          'resource "aws_db_instance" "replica" {\n  lifecycle {\n    prevent_destroy = true\n  }\n}\n'
          'resource "aws_db_instance" "extra" {\n  engine = data.aws_ami.base.id\n}\n'),
    *commit("head"),
]
CHECKOUT_FLOW = ("on: pull_request_target\njobs:\n  build:\n    steps:\n      - uses: actions/checkout@v4\n        with:\n"
                 "          ref: " + GH + " github.event.pull_request.head.sha }}\n      - run: echo one\n      - run: echo two\n"
                 "      - run: echo {}\n      - run: echo four\n")
EDITS = [
    git("init", "-q", "-b", "main"),
    write(".github/workflows/pr.yml", CHECKOUT_FLOW.format("three")),
    write("infra/gone.tf", 'resource "aws_sqs_queue" "q" {\n  name = "q"\n}\n'),
    write("infra/uses.tf", "".join(f'output "o{i}" {{\n  value = aws_sqs_queue.q.arn\n}}\n' for i in range(4))),
    *commit("base"),
    # The checkout's ref is line 7; the edit is line 10, just outside the three lines either side the fact looks at.
    write(".github/workflows/pr.yml", CHECKOUT_FLOW.format("changed")),
    ["rm", "infra/gone.tf"],
    write("infra/uses.tf", "".join(f'output "o{i}" {{\n  value = aws_sqs_queue.q.arn\n}}\n' for i in range(4)) + "# touched\n"),
    *commit("head"),
]
REPOS = {"mixed": MIXED, "external": EXTERNAL, "edge": EDGE, "edits": EDITS}
built = {}
for name, steps in REPOS.items():
    base_dir = os.path.join(ROOT, name)
    os.makedirs(base_dir)
    run_steps(base_dir, steps)
    built[name] = base_dir


def call_json(call):
    return [call.path, call.line, call.callee, call.text, call.enclosing, call.targets]


SERVE = [
    ["symbol", {"name": "show"}], ["symbol", {"symbol": "App.run"}], ["symbol", {"name": "", "symbol": "load"}],
    ["symbol", {"name": "  parse "}], ["symbol", {"name": "run"}], ["symbol", {"name": "nothing_here"}],
    ["symbol", {"name": 3}], ["symbol", {}], ["symbol", {"name": "   "}], ["symbol", {"name": "Engine.step"}],
    ["symbol", {"name": "start"}], ["symbol", {"name": "call"}], ["symbol", {"name": "fetch"}],
    ["callers", {"symbol": "load"}], ["callers", {"symbol": "show"}], ["callers", {"symbol": "parse"}],
    ["callers", {"symbol": "KINDS"}], ["callers", {"symbol": "fetch"}], ["callers", {"symbol": "start"}],
    ["callers", {"symbol": "tick"}], ["callers", {"symbol": "wrap"}], ["callers", {"name": "load"}],
    ["callers", {"symbol": "Engine"}], ["callers", {"symbol": "boot"}],
    ["callees", {"symbol": "App.run"}], ["callees", {"symbol": "show"}], ["callees", {"symbol": "main"}],
    ["callees", {"symbol": "nothing_here"}], ["callees", {"symbol": "KINDS"}], ["callees", {"symbol": "Engine.start"}],
    ["callees", {"symbol": "call"}], ["callees", {"symbol": "go"}],
    ["tests_of", {"symbol": "show"}], ["tests_of", {"symbol": "load"}], ["tests_of", {"symbol": "nothing_here"}],
    ["tests_of", {"symbol": "KINDS"}], ["tests_of", {"symbol": "start"}], ["tests_of", {"symbol": "call"}],
    ["tests_of", {"symbol": "boot"}], ["definition", {"symbol": "show"}],
]
NAMES = ["show", "load", "parse", "run", "App.run", "fetch", "Client.fetch", "KINDS", "main", "start", "step", "tick",
         "Engine", "call", "wrap", "fmt", "boot", "go", "added", "render", "paint", "nothing_here"]


def reference_case(name, with_base, **limits):
    root = built[name]
    repo = deep.Repo(root, "head")
    files = ra.parse_diff(ra.Git(root).out("diff", "-U3", "base", "head", "--"))
    base_repo = deep.Repo(root, "base") if with_base else None
    reference = ref.Reference(repo, files, base_repo=base_repo, clock=lambda: 0.0, **limits).build()
    out = {"repo": name, "base": with_base, "limits": limits, "stats": reference.stats,
           "index": [file_index(index) for index in reference.index.values()],
           "base_index": [file_index(index) for index in reference.base_index.values()],
           "workflows": [[path, node_json(node)] for path, node in reference.workflows.items()],
           "terraform": [[path, [tf_json(block) for block in blocks]] for path, blocks in reference.terraform.items()],
           "changed_lines": [[path, sorted(lines)] for path, lines in reference.changed_lines.items()],
           "removed_text": [[path, texts] for path, texts in reference.removed_text.items()]}
    out["symbols_named"] = {n: [s.key for s in reference.symbols_named(n)] for n in NAMES}
    out["callers"] = {n: [call_json(c) for c in reference.callers(n)] for n in NAMES}
    out["callers_all"] = {n: [call_json(c) for c in reference.callers(n, outside_changes=False)] for n in NAMES}
    out["callees"] = {n: [call_json(c) for c in reference.callees(n)] for n in NAMES}
    out["tests_of"] = {n: [list(t) for t in reference.tests_of(n)] for n in NAMES}
    out["changed_symbols"] = [s.key for s in reference.changed_symbols()]
    out["signature_before"] = [[s.key, reference.signature_before(s)] for index in reference.index.values() for s in index.symbols]
    out["overrides"] = [[s.key, [o.key for o in reference.overrides(s)]] for index in reference.index.values() for s in index.symbols]
    out["enclosing"] = [[path, line, (lambda s: s.key if s else None)(reference.enclosing(path, line))]
                        for path in reference.index for line in range(0, 16)]
    out["family_matches"] = [[f.path, f.line, [[o.path, o.line, how] for o, how in reference.family_matches(f)]]
                             for index in reference.index.values() for f in index.families]
    out["resolve_module"] = [[spec, path, reference._resolve_module(spec, path)] for spec, path in (
        ("pkg.helper:load", "src/pkg/app.py"), ("pkg.helper", "x.py"), ("pkg:helper", "x.py"), ("os", "x.py"),
        ("./util:parse", "web/main.ts"), ("./util", "web/main.ts"), ("../web/util", "web/sub/x.ts"),
        ("node:fs:readFile", "web/main.ts"), ("./missing:x", "web/main.ts"), (".:util_mod", "lib/core/engine.py"),
        ("..shared:consts", "lib/core/engine.py"), (".engine:Engine", "lib/core/__init__.py"),
        ("...:x", "lib/core/engine.py"), (".:*", "lib/core/engine.py"), ("./index", "app/api.ts"), ("./api:*", "app/index.ts"),
        ("lib.core.engine", "lib/use_engine.py"), ("lib.core:engine", "lib/use_engine.py"), ("/abs/x", "a/b.py"))]
    out["symbol_views"] = [[s.key, reference.symbol_view(s), reference.symbol_view(s, body=True)]
                           for index in reference.index.values() for s in index.symbols if s.kind in ("function", "method", "class")]
    out["block"] = reference.block()
    out["block_small"] = ref.Reference(repo, files, base_repo=base_repo, clock=lambda: 0.0, **limits).build().block(limit=700)
    out["facts"] = reference.facts()
    out["findings"] = reference.structural_findings()
    out["served"] = [[kind, args, reference_serve(reference, kind, args)] for kind, args in SERVE]
    return out


def reference_serve(reference, kind, args):
    answer = ref.serve(reference, kind, args)
    return list(answer) if answer else None


references = [
    reference_case("mixed", True), reference_case("mixed", False), reference_case("external", True),
    reference_case("edge", True), reference_case("edge", False),
    reference_case("mixed", True, max_files=4), reference_case("mixed", True, max_bytes=300),
    reference_case("edge", True, max_files=6), reference_case("edits", True),
]
shutil.rmtree(ROOT)

print(json.dumps({
    "git_env": GIT_ENV, "repos": REPOS, "ts": ts_cases, "block_ends": block_ends, "python": py_cases,
    "scalars": scalars, "flows": flows, "comments": comments, "yaml": yaml_cases, "yaml_gets": yaml_gets,
    "terraform": tf_cases, "module_maps": module_maps, "references": references,
}, indent=1, sort_keys=False, ensure_ascii=False))
