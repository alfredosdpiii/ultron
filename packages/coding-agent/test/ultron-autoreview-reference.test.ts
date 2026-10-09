/**
 * The structural reference of an automated review (`rlm/autoreview_reference.py`) and what the pipeline does with
 * it: definitions resolved through imports instead of grepped (Python by `ast`, TypeScript by a scanner), the
 * callers, tests and signature changes of what the diff touches, literal families listed elsewhere, GitHub
 * workflow and Terraform facts that become candidate findings the verifier judges, the `symbol`, `callers`,
 * `callees` and `tests_of` lookups an investigator may request, and the precision rule for tests findings. All
 * offline, on small repositories made here; no model.
 */
import { execFileSync } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { afterEach, describe, expect, test } from "vitest";

const PYTHON =
	process.env.ULTRON_PYTHON ??
	(process.platform === "linux" && existsSync("/usr/bin/python3") ? "/usr/bin/python3" : "python3");
const here = dirname(fileURLToPath(import.meta.url));
const RLM_DIR = resolve(here, "../src/ultron/rlm");
const dirs: string[] = [];

afterEach(() => {
	for (const dir of dirs.splice(0)) rmSync(dir, { recursive: true, force: true });
});

function git(cwd: string, ...args: string[]): string {
	return execFileSync(
		"git",
		["-c", "user.name=Review Test", "-c", "user.email=review@test", "-c", "commit.gpgsign=false", ...args],
		{ cwd, encoding: "utf8", env: { ...process.env, GIT_CONFIG_NOSYSTEM: "1" } },
	).trim();
}

function write(root: string, path: string, text: string): void {
	mkdirSync(dirname(join(root, path)), { recursive: true });
	writeFileSync(join(root, path), text);
}

const GH = "$" + "{{"; // a GitHub Actions expression opener, kept out of plain strings for the linter

const PRELUDE = `
import sys, json, asyncio, re
sys.path.insert(0, ${JSON.stringify(RLM_DIR)})
import autoreview_api as a
import review_api as r
import autoreview_deep as deep
import autoreview_reference as ref
import review_prompts as p
from infer_api import MapResults, Incomplete, FrameError
a.DEFAULT_MODE = "fast"
LENS = {p.deep_task(name): name for name in p.DEEP_LENSES}
def emit(value):
    print(json.dumps(value, default=str))
def opened(root, base, head):
    repo = deep.Repo(root, head)
    files = r.parse_diff(r.Git(root).out("diff", "-U3", base, head, "--"))
    reader = r._rev_reader(r.Git(root), head)
    return repo, files, reader
`;

function py<T = unknown>(code: string): T {
	const output = execFileSync(PYTHON, ["-c", `${PRELUDE}\n${code}`], {
		cwd: RLM_DIR,
		encoding: "utf8",
		env: { ...process.env, PYTHONDONTWRITEBYTECODE: "1" },
	});
	return JSON.parse(output.trim().split("\n").at(-1)!) as T;
}

/** A repository with Python (a package, an importer, a test), TypeScript, workflows and Terraform, and a change
 * that touches each: a signature change, a literal family extended, a new risky workflow, a removed resource. */
function mixedRepo(): { dir: string; base: string; head: string } {
	const dir = mkdtempSync(join(tmpdir(), "ultron-autoreview-reference-"));
	dirs.push(dir);
	git(dir, "init", "-q", "-b", "main");
	write(dir, "src/pkg/__init__.py", "");
	write(
		dir,
		"src/pkg/helper.py",
		'def load(path, mode="r"):\n    """Read a file."""\n    return open(path, mode).read()\n\n\nclass Base:\n    def run(self):\n        return 1\n',
	);
	write(
		dir,
		"src/pkg/app.py",
		'from pkg.helper import load, Base\n\nKINDS = ["a", "b", "c"]\n\n\nclass App(Base):\n    def run(self):\n        return load("x")\n\n\ndef show(path):\n    return load(path)\n',
	);
	write(
		dir,
		"src/pkg/other.py",
		'KINDS = ["a", "b", "c"]\n\n\ndef consume():\n    from pkg.app import show\n    return show("p")\n',
	);
	write(
		dir,
		"tests/test_app.py",
		'from pkg.app import KINDS, show\n\n\ndef test_show():\n    assert show("f") is not None\n',
	);
	write(
		dir,
		"web/util.ts",
		"export const NAMES = ['x', 'y', 'z'];\nexport function parse(input: string): number {\n  return Number(input);\n}\nexport class Client {\n  fetch(url: string) {\n    return parse(url);\n  }\n}\n",
	);
	write(
		dir,
		"web/main.ts",
		"import { parse, Client } from './util';\nconst c = new Client();\nexport default function main() {\n  return parse(c.fetch('u'));\n}\n",
	);
	for (const name of ["a", "b"])
		write(
			dir,
			`.github/workflows/${name}.yml`,
			`name: ${name}\non: push\npermissions:\n  contents: read\njobs:\n  x:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo hi\n`,
		);
	write(
		dir,
		"infra/main.tf",
		'resource "aws_s3_bucket" "logs" {\n  bucket = "logs"\n  lifecycle {\n    prevent_destroy = true\n  }\n}\nresource "aws_s3_bucket" "data" {\n  bucket = "data"\n  lifecycle {\n    prevent_destroy = true\n  }\n}\noutput "o" {\n  value = aws_s3_bucket.gone.id\n}\n',
	);
	write(dir, "infra/gone.tf", 'resource "aws_s3_bucket" "gone" {\n  bucket = "gone"\n}\n');
	git(dir, "add", ".");
	git(dir, "commit", "-qm", "base");
	const base = git(dir, "rev-parse", "HEAD");
	write(
		dir,
		"src/pkg/app.py",
		'from pkg.helper import load, Base\n\nKINDS = ["a", "b", "c", "d"]\n\n\nclass App(Base):\n    def run(self):\n        return load("x")\n\n\ndef show(path, strict=False):\n    return load(path)\n',
	);
	write(
		dir,
		"web/util.ts",
		"export const NAMES = ['x', 'y', 'z'];\nexport function parse(input: string, radix = 10): number {\n  return Number(input);\n}\nexport class Client {\n  fetch(url: string) {\n    return parse(url);\n  }\n}\n",
	);
	write(
		dir,
		".github/workflows/new.yml",
		"name: New\non:\n  pull_request_target:\n    types: [opened]\njobs:\n  x:\n    runs-on: ubuntu-latest\n    permissions:\n      id-token: write\n    steps:\n      - uses: actions/checkout@v4\n        with:\n          ref: " +
			GH +
			' github.event.pull_request.head.sha }}\n      - run: |\n          echo "' +
			GH +
			' github.event.pull_request.title }}"\n',
	);
	rmSync(join(dir, "infra/gone.tf"));
	write(dir, "infra/fresh.tf", 'resource "aws_s3_bucket" "fresh" {\n  bucket = "fresh"\n}\n');
	git(dir, "add", "-A");
	git(dir, "commit", "-qm", "change");
	const head = git(dir, "rev-parse", "HEAD");
	return { dir, base, head };
}

describe("the structural reference", () => {
	test("resolves definitions, callers, tests and signature changes through imports, in Python and TypeScript", () => {
		const repo = mixedRepo();
		const out = py<{
			stats: Record<string, number>;
			block: string;
			showCallers: Array<[string, number, string, boolean]>;
			parseCallers: Array<[string, number, string, boolean]>;
			tests: Array<[string, number, string]>;
			callees: Array<[string, string[]]>;
			enclosing: string | null;
			before: string | null;
			symbolView: string;
			families: Array<[string, number, string | null, string[]]>;
		}>(`
repo, files, reader = opened(${JSON.stringify(repo.dir)}, ${JSON.stringify(repo.base)}, ${JSON.stringify(repo.head)})
R = ref.Reference(repo, files, base_repo=deep.Repo(repo.root, ${JSON.stringify(repo.base)})).build()
show = R.symbols_named("show")[0]
emit({"stats": R.stats, "block": R.block(),
      "showCallers": [[c.path, c.line, c.enclosing, bool(c.targets)] for c in R.callers("show")],
      "parseCallers": [[c.path, c.line, c.enclosing, bool(c.targets)] for c in R.callers("parse")],
      "tests": R.tests_of("show"), "callees": [[c.callee, c.targets] for c in R.callees("App.run")],
      "enclosing": (R.enclosing("src/pkg/app.py", 12) or ref.Symbol("", "", "", "", 0, 0)).qualname or None,
      "before": R.signature_before(show), "symbolView": R.symbol_view(show, body=True),
      "families": [[f.path, f.line, f.name, list(f.items)] for f in R.index["src/pkg/app.py"].families]})`);
		// The index reaches the importer (other.py), the imported module (helper.py), the test and the TS caller.
		expect(out.stats.files).toBeGreaterThanOrEqual(8);
		expect(out.stats.symbols).toBeGreaterThan(8);
		// Callers of show: resolved through `from pkg.app import show` (other.py) and the test's import.
		expect(out.showCallers).toEqual([
			["src/pkg/other.py", 6, "consume", true],
			["tests/test_app.py", 5, "test_show", true],
		]);
		// TS: parse's callers are resolved through `import { parse } from './util'` and inside the class.
		expect(out.parseCallers).toContainEqual(["web/main.ts", 4, "main", true]);
		expect(out.parseCallers).toContainEqual(["web/util.ts", 7, "Client.fetch", true]);
		expect(out.tests).toEqual([["tests/test_app.py", 5, "test_show"]]);
		expect(out.callees).toEqual([["load", ["src/pkg/helper.py::load"]]]);
		expect(out.enclosing).toBe("show");
		expect(out.before).toBe("def show(path)");
		expect(out.symbolView).toContain("def show(path, strict=False) [was: def show(path)]");
		expect(out.symbolView).toContain("callers outside the change (2)");
		expect(out.symbolView).toMatch(/ {3}11 \| def show\(path, strict=False\):/);
		expect(out.families).toEqual([["src/pkg/app.py", 3, "KINDS", ["a", "b", "c", "d"]]]);
		// The block the frames read: the changed definitions with their callers, and the facts with ids.
		expect(out.block).toContain("[D1] src/pkg/app.py:11-12 def show(path, strict=False) [was: def show(path)]");
		expect(out.block).toContain("[D2] web/util.ts:2-4 export function parse(input: string, radix = 10): number {");
		expect(out.block).toMatch(
			/\[F\d\] The change edits the literal family `KINDS` at src\/pkg\/app\.py:3 \('a', 'b', 'c', 'd'\); the same family is listed at src\/pkg\/other\.py:1 \(`KINDS`\): missing 'd'\. A file not in this change lists it differently\./,
		);
		expect(out.block).toMatch(
			/\[F\d\] The signature of show changed: was `def show\(path\)`, now `def show\(path, strict=False\)`; 2 caller\(s\) outside the change/,
		);
	});

	test("states workflow and Terraform facts, each a candidate finding with a citation the host checked", () => {
		const repo = mixedRepo();
		const out = py<{
			facts: Array<[string, string, string, number]>;
			findings: Array<{
				category: string;
				level: string;
				file: string;
				line: number;
				claim: string;
				citations: Array<{ path: string; line: number; quote: string }>;
				source: string;
				beyond_diff: boolean;
			}>;
		}>(`
repo, files, reader = opened(${JSON.stringify(repo.dir)}, ${JSON.stringify(repo.base)}, ${JSON.stringify(repo.head)})
R = ref.Reference(repo, files).build()
emit({"facts": [[f["id"], f["kind"], f["path"], f["line"]] for f in R.facts()], "findings": R.structural_findings()})`);
		const kinds = out.facts.map(([, kind]) => kind);
		expect(kinds.filter((kind) => kind === "workflow")).toHaveLength(3);
		expect(kinds.filter((kind) => kind === "terraform")).toHaveLength(2);
		const byClaim = Object.fromEntries(out.findings.map((finding) => [finding.claim.slice(0, 40), finding]));
		const claims = out.findings.map((finding) => finding.claim);
		expect(claims).toContainEqual(
			expect.stringContaining("grants `id-token: write` although no step requests an OIDC token"),
		);
		expect(claims).toContainEqual(
			expect.stringContaining(
				"checks out the pull request head under a pull_request_target trigger without a same-repository guard",
			),
		);
		expect(claims).toContainEqual(
			expect.stringContaining(
				`interpolates the user-controlled expression \`${GH} github.event.pull_request.title }}\``,
			),
		);
		expect(claims).toContainEqual("`output.o` still references `aws_s3_bucket.gone`, which this change removes.");
		expect(claims).toContainEqual(expect.stringContaining("lacks the `prevent_destroy` lifecycle rule"));
		for (const finding of out.findings) {
			expect(finding.source).toBe("deep:structure");
			expect(finding.citations.length).toBeGreaterThan(0);
			for (const citation of finding.citations) expect(citation.quote.length).toBeGreaterThan(3);
		}
		// The guard and the injection are medium (a concrete scenario); hygiene is low.
		expect(byClaim["The workflow grants `id-token: write` al"]!.level).toBe("low");
		expect(out.findings.find((finding) => finding.claim.includes("same-repository guard"))!.level).toBe("medium");
		// The removed resource is cited in a file the change does not touch: evidence beyond the diff.
		const removed = out.findings.find((finding) => finding.claim.startsWith("`output.o`"))!;
		expect(removed).toMatchObject({ file: "infra/main.tf", line: 14, beyond_diff: true });
		expect(removed.citations[0]!.quote).toBe("value = aws_s3_bucket.gone.id");
	});

	test("a workflow that hands `id-token: write` to another repository's reusable workflow is a fact, with the mutable ref named", () => {
		const dir = mkdtempSync(join(tmpdir(), "ultron-ref-ext-"));
		try {
			git(dir, "init", "-q", "-b", "main");
			git(dir, "config", "user.email", "t@t");
			git(dir, "config", "user.name", "t");
			write(
				dir,
				".github/workflows/old.yml",
				"name: Old\non: push\npermissions:\n  contents: read\njobs:\n  a:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo hi\n",
			);
			git(dir, "add", "-A");
			git(dir, "commit", "-q", "-m", "base");
			const base = git(dir, "rev-parse", "HEAD");
			write(
				dir,
				".github/workflows/review.yml",
				"name: Review\non:\n  pull_request:\n    types: [opened]\npermissions:\n  contents: read\n  pull-requests: write\n  id-token: write\njobs:\n  review:\n    uses: other-org/.github/.github/workflows/review.yml@main\n    secrets: inherit\n",
			);
			git(dir, "add", "-A");
			git(dir, "commit", "-q", "-m", "add review workflow");
			const head = git(dir, "rev-parse", "HEAD");
			const out = py<{ facts: string[]; claims: string[] }>(`
repo, files, reader = opened(${JSON.stringify(dir)}, ${JSON.stringify(base)}, ${JSON.stringify(head)})
reference = ref.Reference(repo, files, base_repo=deep.Repo(${JSON.stringify(dir)}, ${JSON.stringify(base)})).build()
emit({"facts": [f["text"] for f in reference.facts()], "claims": [f["claim"] for f in reference.structural_findings()]})`);
			expect(out.facts).toEqual([
				expect.stringContaining(
					".github/workflows/review.yml:8 grants `id-token: write` to the external reusable workflow `other-org/.github/.github/workflows/review.yml@main` (a mutable ref)",
				),
			]);
			expect(out.claims).toEqual([
				"The workflow grants `id-token: write` to the external reusable workflow `other-org/.github/.github/workflows/review.yml@main`.",
			]);
		} finally {
			rmSync(dir, { recursive: true, force: true });
		}
	});

	test("the YAML reader and the Terraform scanner read what workflows and manifests use", () => {
		const out = py<{
			doc: unknown;
			lines: Record<string, number>;
			blocks: Array<[string, string, string, number, number, Record<string, string>, string[]]>;
		}>(`
node = ref.parse_yaml("""# a workflow
name: CI
on:
  push:
    branches: [main, 'release/*']
  workflow_dispatch:
env:
  FOO: "bar # not a comment"
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - name: Test
        run: |
          echo one
          echo two
      - run: echo three
    strategy:
      matrix: {node: [18, 20]}
""")
blocks = ref.parse_terraform("main.tf", """resource "aws_iam_role" "r" {
  name = "r"
  assume_role_policy = <<EOF
{ "ref": "aws_s3_bucket.other.arn" }
EOF
  lifecycle {
    prevent_destroy = true
  }
  tags = { a = var.name }
}
module "m" {
  source = "./m"
  role   = aws_iam_role.r.arn
}
""")
emit({"doc": node.value, "lines": node.lines, "blocks": [[b.kind, b.type, b.name, b.line, b.end, b.attrs, sorted(b.refs)] for b in blocks]})`);
		expect(out.doc).toEqual({
			name: "CI",
			on: { push: { branches: ["main", "release/*"] }, workflow_dispatch: null },
			env: { FOO: "bar # not a comment" },
			jobs: {
				build: {
					"runs-on": "ubuntu-latest",
					steps: [
						{ uses: "actions/checkout@v4" },
						{ name: "Test", run: "echo one\necho two" },
						{ run: "echo three" },
					],
					strategy: { matrix: { node: ["18", "20"] } },
				},
			},
		});
		expect(out.lines).toEqual({ name: 2, on: 3, env: 7, jobs: 9 });
		expect(out.blocks).toEqual([
			["resource", "aws_iam_role", "r", 1, 10, { name: '"r"', tags: "{ a = var.name }" }, ["var.name"]],
			["module", "m", "", 11, 14, { source: '"./m"', role: "aws_iam_role.r.arn" }, ["aws_iam_role.r"]],
		]);
	});

	test("the brief carries the reference and resolved callers; investigators get its block and its lookups; structural findings join the deep pass and reach the verifier", () => {
		const repo = mixedRepo();
		const out = py<{
			briefText: string;
			referenceText: string;
			stats: Record<string, number>;
			served: Record<string, string>;
			fallback: string;
			rejected: string;
			deepSources: string[];
			deepView: string;
			deepTask: string;
			audiences: Record<string, { investigators: boolean; finders: boolean }>;
			verified: Array<{
				source: string;
				verification: string;
				level: string;
				file: string;
				claim: string;
				note?: string;
			}>;
			retrieval: Record<string, unknown>;
		}>(`
repo, files, reader = opened(${JSON.stringify(repo.dir)}, ${JSON.stringify(repo.base)}, ${JSON.stringify(repo.head)})
brief = deep.build_brief(repo, files, reader, base=${JSON.stringify(repo.base)})
served = {}
for request in ({"symbol": {"name": "show"}}, {"callers": {"symbol": "load"}}, {"callees": {"symbol": "App.run"}},
                {"tests_of": {"symbol": "show"}}, {"definition": {"symbol": "show"}}, {"references": {"symbol": "show"}}):
    title, body = deep.serve_request(repo, request)
    served[next(iter(request))] = title + "\\n" + body
# A name the reference does not know falls back to the text lookups; callees needs a definition.
fallback = deep.serve_request(repo, {"callers": {"symbol": "KINDS"}})[0]
try:
    deep.serve_request(repo, {"callees": {"symbol": "nothing_here"}})
    rejected = ""
except deep.Rejected as error:
    rejected = str(error)
texts = []
tasks_seen = []
finder_texts = []
def investigator(lens, text, round):
    texts.append(text)
    return {"findings": [], "requests": [], "done": True}
class FakeRlm:
    def __init__(self):
        self.calls = []
    async def map(self, tasks, items=None, **options):
        tasks = [tasks] * len(items) if isinstance(tasks, str) else list(tasks)
        out = MapResults()
        for task, item in zip(tasks, items):
            text = item if isinstance(item, str) else "\\n".join(item)
            if task in LENS:
                tasks_seen.append(task)
                out.append(investigator(LENS[task], text, 1))
            elif task in (p.AUTOREVIEW_VERIFIER_TASK, p.AUTOREVIEW_VERIFIER_BATCH_TASK):
                self.calls.append(text)
                # The fake confirms by quoting the marked source line of each finding, as the real verifier must.
                def verdict(section):
                    marked = re.search(r"^>\\s*\\d+ \\| ?(.*)$", section, re.M)
                    quote = (marked.group(1).strip() if marked else "") or "no marked line"
                    return {"verdict": "confirmed", "evidence": f"\`{quote}\` shows it", "corrected_line": None, "severity": "medium", "scenario_holds": True}
                if task == p.AUTOREVIEW_VERIFIER_BATCH_TASK:
                    sections = text.split("=== Finding ")[1:]
                    out.append([dict(verdict(section), finding=n) for n, section in enumerate(sections, 1)])
                else:
                    out.append(verdict(text))
            else:
                # A finder's context views travel in the map's context option, not in its items.
                finder_texts.append("\\n".join(list(options.get("context") or [])) + "\\n" + text)
                out.append([])
        out.spent = {"calls": len(items), "tokens": 100 * len(items)}
        out.usage = {"input_tokens": 80 * len(items), "output_tokens": 20 * len(items), "cost": 0.001 * len(items)}
        out.budget = {}
        out.remaining = {}
        return out
rlm = FakeRlm()
spec = {"repoDir": repo.root, "base": ${JSON.stringify(repo.base)}, "head": ${JSON.stringify(repo.head)}, "mode": "both"}
result = asyncio.run(a.run(rlm, spec))
# Who sees the reference's rendering: by default the investigators, never the finders; a setting widens or narrows it.
audiences = {}
for audience in ("default", "all", "none"):
    texts.clear(); finder_texts.clear(); tasks_seen.clear()
    asyncio.run(a.run(FakeRlm(), spec if audience == "default" else dict(spec, referenceView=audience)))
    seen = lambda items: any("Reference, built by the host" in t for t in items)
    audiences[audience] = {"investigators": seen(texts), "finders": seen(finder_texts)}
texts.clear(); finder_texts.clear(); tasks_seen.clear()
result = asyncio.run(a.run(FakeRlm(), spec))
emit({"audiences": audiences, "briefText": brief.text, "referenceText": brief.reference_text, "stats": brief.reference_stats, "served": served,
      "fallback": fallback, "rejected": rejected, "deepView": texts[0] if texts else "", "deepTask": tasks_seen[0] if tasks_seen else "",
      "deepSources": sorted({f["source"] for f in result["findings"]}),
      "verified": [{k: f.get(k) for k in ("source", "verification", "level", "file", "claim", "note")} for f in result["findings"] if f["source"] == "deep:structure"],
      "retrieval": result["timing"]["reference"]})`);
		// The brief's uses of a changed symbol come from resolved call sites, not a word grep.
		expect(out.briefText).toContain("Changed or added `show` (callers resolved):");
		expect(out.briefText).toContain('used at: src/pkg/other.py:6: return show("p")');
		expect(out.referenceText).toContain("[D1] src/pkg/app.py:11-12 def show(path, strict=False)");
		expect(out.stats.symbols).toBeGreaterThan(8);
		// The lookups, served from the index.
		expect(out.served.symbol).toContain("symbol show -> 1 definition(s)");
		expect(out.served.symbol).toMatch(/ {3}11 \| def show\(path, strict=False\):/);
		expect(out.served.callers).toContain("callers load -> 2 call sites");
		expect(out.served.callers).toContain("src/pkg/app.py:8 in App.run (resolved)");
		expect(out.served.callees).toContain("src/pkg/app.py:8: load(...) -> src/pkg/helper.py::load");
		expect(out.served.tests_of).toContain("tests/test_app.py:5 test_show");
		// definition and references answer from the index first, then the text search.
		expect(out.served.definition).toContain("def show(path, strict=False) [was: def show(path)]");
		expect(out.served.references).toContain("Call sites resolved by the reference:");
		expect(out.served.references).toContain("Text matches:");
		expect(out.fallback).toMatch(/^references KINDS -> \d+ matches/);
		expect(out.rejected).toContain("no definition of nothing_here in the reference");
		// The reference's rendering is the investigators' view by default, never the finders'; `all` and `none` move it.
		expect(out.audiences).toEqual({
			default: { investigators: true, finders: false },
			all: { investigators: true, finders: true },
			none: { investigators: false, finders: false },
		});
		// Every investigator sees the reference block, and the prompt names the new requests.
		expect(out.deepView).toContain("Reference, built by the host from the repository at the reviewed commit");
		expect(out.deepTask).toContain('{"callers": {"symbol": "..."}}');
		// The structural findings went through the verifier (confirmed by the fake) and are posted with their source.
		expect(out.deepSources).toContain("deep:structure");
		expect(out.verified.length).toBeGreaterThanOrEqual(4);
		for (const finding of out.verified)
			expect([finding.verification, finding.note ?? "", finding.claim]).toEqual(["confirmed", "", finding.claim]);
		expect(out.verified.map((finding) => finding.file)).toContain(".github/workflows/new.yml");
		expect(out.retrieval).toMatchObject({
			symbols: expect.any(Number),
			files: expect.any(Number),
			ms: expect.any(Number),
		});
	});
});

describe("the precision rule for tests findings", () => {
	test("a missing-test finding blocks only when the host proved it by a mutation, and never for a configuration value", () => {
		const out = py<{
			levels: Record<string, string>;
			generic: Record<string, string | null>;
			code: Record<string, boolean>;
		}>(`
def tests_finding(**extra):
    base = {"category": "tests", "level": "medium", "scenario": "", "file": "src/app.py",
            "unpinned": {"behaviour": "the guard at src/app.py:3", "change": "remove the guard"}}
    return dict(base, **extra)
verdict = {"severity": "medium", "scenario_holds": "unknown"}
levels = {
    "proven": a.final_level(tests_finding(proof="proven", test_run=2), verdict),
    "reading": a.final_level(tests_finding(), verdict),
    "verifier_high": a.final_level(tests_finding(), {"severity": "high", "scenario_holds": True}),
    "correctness_medium": a.final_level({"category": "correctness", "level": "medium", "scenario": "x fails", "file": "src/app.py"}, verdict),
}
generic = {
    "code": a.generic_reason(tests_finding()),
    "workflow": a.generic_reason(tests_finding(file=".github/workflows/deploy.yml")),
    "terraform_mutation": a.generic_reason(tests_finding(unpinned={"behaviour": "the ALWAYS_SEND wiring", "change": "drop the setting", "mutation": {"path": "infra/main.tf", "line": 3, "replacement": "x"}})),
    "sql": a.generic_reason(tests_finding(file="sql/07_evidence.sql")),
    "unnamed": a.generic_reason({"category": "tests", "level": "medium", "file": "src/app.py", "unpinned": {"behaviour": "", "change": ""}}),
}
code = {path: deep.is_code(path) for path in ("src/app.py", "web/x.tsx", "deploy.yml", "infra/main.tf", "sql/x.sql", "run.sh", "README.md", "Dockerfile", ".env.example")}
emit({"levels": levels, "generic": generic, "code": code})`);
		expect(out.levels).toEqual({
			proven: "medium",
			reading: "low",
			verifier_high: "low",
			correctness_medium: "medium",
		});
		expect(out.generic.code).toBeNull();
		expect(out.generic.workflow).toBe("a tests finding about a configuration, manifest, script or document value");
		expect(out.generic.terraform_mutation).toBe(
			"a tests finding about a configuration, manifest, script or document value",
		);
		expect(out.generic.sql).toBe("a tests finding about a configuration, manifest, script or document value");
		expect(out.generic.unnamed).toBe("a tests finding that names no change an existing test would miss");
		expect(out.code).toEqual({
			"src/app.py": true,
			"web/x.tsx": true,
			"deploy.yml": false,
			"infra/main.tf": false,
			"sql/x.sql": false,
			"run.sh": false,
			"README.md": false,
			Dockerfile: false,
			".env.example": false,
		});
	});
});
