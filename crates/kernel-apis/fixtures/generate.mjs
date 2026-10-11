// Generates crates/kernel-apis/fixtures/<area>.json: Python cells calling the thin kernel APIs (skills_api.py,
// memory_api.py, grants_api.py, hints_api.py, ...) through the real runtime.py and the TypeScript kernel supervisor
// (kernel.ts), served by the TS host (NativeRlmHost with the area's host module or worker services). Each scenario runs
// its cells in one kernel against one fresh host and records every cell's outcome, then the host's durable state
// (module documents, the code-skills directory). Random ids, timestamps, durations and temp paths are normalized.
// No model is called: none of these APIs needs one.
// Usage: node crates/kernel-apis/fixtures/generate.mjs [area...]
// Other generate-<area>.mjs files import scenario/record/show/attempt/normalize/tree from here.
import { mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, relative, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, "../../..");
const pkg = join(root, "packages/coding-agent");
const load = (path) => import(pathToFileURL(join(pkg, path)).href);
const { RlmKernel } = await load("src/ultron/rlm/kernel.ts");
const { NativeRlmHost } = await load("src/ultron/rlm/native-host.ts");
const { createMemoryModuleStore } = await load("src/ultron/rlm/host-module.ts");
const { memoryStore, memoryDefinitionStore } = await load("test/ultron-host-fixtures.ts");
const { createSkillModule } = await load("src/ultron/skills.ts");
const { CodeSkills } = await load("src/ultron/code-skills.ts");
const { createWorkerServices } = await load("src/ultron/worker-services.ts");
const { createGrantModule } = await load("src/ultron/grants.ts");
const { CellHints } = await load("src/ultron/rlm/hints.ts");

export const FIXED_NOW = 1_760_000_000_000;
const runtimePath = join(pkg, "src/ultron/rlm/runtime.py");
const j = (...parts) => parts.join("");

/** Replaces what differs between runs: uuids, ISO timestamps, durations, the scenario's temp dir. */
export function normalize(text, dir) {
	let out = dir ? text.split(dir).join("<dir>") : text;
	out = out.replace(/[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}/g, "<uuid>");
	out = out.replace(/\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z/g, "<time>");
	out = out.replace(/(durationMs\\*["']):\s?\d+/g, "$1: 0");
	out = out.replace(/[^\s"'\\]*ultron-skill-test-[A-Za-z0-9_]+/g, "<skilltest>");
	return out;
}

/** Files under `dir` (relative path → content), skipping lock files. */
export function tree(dir) {
	const files = {};
	const walk = (path) => {
		let entries;
		try {
			entries = readdirSync(path).sort();
		} catch {
			return;
		}
		for (const name of entries) {
			if (name === ".lock") continue;
			const full = join(path, name);
			if (statSync(full).isDirectory()) walk(full);
			else files[relative(dir, full)] = readFileSync(full, "utf8");
		}
	};
	walk(dir);
	return files;
}

const stubHarness = {
	hooks: { on: () => () => {} },
	lane: async () => {
		throw new Error("no lanes in this fixture");
	},
};

/**
 * Runs `cells` in one kernel against a NativeRlmHost built by `make(dir)` (which returns {modules, services,
 * env, state}). Records each cell as {code, status, stdout, result, error} and `state()` at the end.
 */
export async function scenario(name, make, cells) {
	const dir = mkdtempSync(join(tmpdir(), "ultron-kernel-apis-"));
	try {
		const made = await make(dir);
		const host = new NativeRlmHost(stubHarness, {}, {
			store: memoryStore(),
			definitionStore: memoryDefinitionStore(),
			modules: made.modules ?? [],
			services: made.services,
		});
		const kernel = new RlmKernel(
			{ cwd: dir, runtimePath, treeMemoryBackend: "off", env: { ULTRON_RLM_SPILL_DIR: "", TMPDIR: dir, ...(made.env ?? {}) } },
			(type, payload, signal) => host.handle(type, payload, signal ? { abortSignal: signal } : {}, undefined),
		);
		const results = [];
		try {
			for (const code of cells) {
				const { status, stdout, result, error } = await kernel.execute(code);
				results.push({ code, status, stdout, result: result ?? null, error: error ? `${error.ename}: ${error.evalue}` : null });
			}
		} finally {
			await kernel.shutdown();
			await host.close?.();
		}
		const state = made.state ? await made.state() : null;
		return JSON.parse(normalize(JSON.stringify({ name, cells: results, state }), dir));
	} finally {
		rmSync(dir, { recursive: true, force: true });
	}
}

// ---------------------------------------------------------------------------------------------------------------
// skills: code skills (propose_code / propose_policy / rollback / code_list / code_history) and the skill catalog.

export const SLUG_V1 = `"""Turn free text into URL slugs."""
import re

def slugify(text: str) -> str:
    """Lowercase, keep alphanumerics, join words with hyphens."""
    return "-".join(re.findall(r"[a-z0-9]+", text.lower()))
`;
export const SLUG_V2 = `"""Turn free text into URL slugs (v2: underscores)."""
import re

def slugify(text: str) -> str:
    return "_".join(re.findall(r"[a-z0-9]+", text.lower()))
`;
export const SLUG_TEST_V1 = `from code_skills import slug
def test_slugify():
    assert slug.slugify("Hello, World!") == "hello-world"
`;
export const SLUG_TEST_V2 = `from skill import slugify
async def test_slugify():
    assert slugify("Hello, World!") == "hello_world"
`;
const BROKEN = `def slugify(text):\n    return text.upper()\n`;
const BROKEN_TEST = `from code_skills import slug\ndef test_slugify():\n    assert slug.slugify("a b") == "a-b"\n`;
const py = (value) => JSON.stringify(value);

/** Catalog skills the fixture's loader returns (Pi's loader is not part of this module). */
export const CATALOG = [
	{
		name: "deploy-notes",
		description: "Write deployment notes for a release",
		filePath: "/skills/deploy-notes/SKILL.md",
		content: "---\nname: deploy-notes\ndescription: Write deployment notes for a release\nallowed-tools: [bash]\nmodel: big\n---\nCollect the changelog and write release notes for the deploy.\n",
	},
	{
		name: "slug-helper",
		description: "Make URL slugs",
		filePath: "/skills/slug-helper/SKILL.md",
		content: "---\nname: slug-helper\ndescription: Make URL slugs\n---\nUse slugify on titles. Release slugs are lowercase.\n",
	},
	{
		name: "hidden",
		description: "Internal only",
		filePath: "/skills/hidden/SKILL.md",
		content: "---\nname: hidden\ndescription: Internal only\ndisable-model-invocation: true\n---\nRelease internals.\n",
		disableModelInvocation: true,
	},
];

function skillsHost(dir) {
	const skillsDir = join(dir, "skills");
	mkdirSync(skillsDir, { recursive: true });
	const store = createMemoryModuleStore();
	const code = new CodeSkills({ dir: skillsDir, testTimeoutMs: 4000 });
	const module = createSkillModule({ store, loadSkills: async () => structuredClone(CATALOG), code, now: () => FIXED_NOW });
	return {
		modules: [module],
		env: { ULTRON_CODE_SKILLS_DIR: skillsDir },
		state: async () => ({ module: (await store.read()) ?? null, files: tree(skillsDir), describe: code.describe() }),
	};
}

export const show = (expr) => `import json as _json\nprint(_json.dumps(${expr}, sort_keys=True))`;
export const attempt = (call) => `try:\n    r = await ${call}\n    print("ok", _json.dumps(r, sort_keys=True))\nexcept Exception as e:\n    print(type(e).__name__, e)`;

const skillsScenarios = [
	[
		"propose, reject, import, version, rollback",
		[
			`import json as _json\nr = await skills.propose_code("slug", ${py(BROKEN)}, ${py(BROKEN_TEST)}, "first try")\nprint(r["status"], r["version"], r["test"]["passed"], "AssertionError" in r["test"]["tests"][0]["error"])\nprint(_json.dumps({k: v for k, v in r.items() if k != "test"}, sort_keys=True))`,
			"from code_skills import slug",
			"import code_skills.slug",
			`r = await skills.propose_code("slug", ${py(SLUG_V1)}, "x = 1\\n", "no tests")\nprint(r["status"], r["test"]["error"])`,
			`r = await skills.propose_code("slug", "def slugify(:\\n", ${py(SLUG_TEST_V1)}, "typo")\nprint(r["status"], "SyntaxError" in r["test"]["error"])`,
			show(`await skills.propose_code("slug", ${py(SLUG_V1)}, ${py(SLUG_TEST_V1)}, {"task": "slugs", "worked": True})`),
			"from code_skills import slug\nprint(slug.__skill_version__, slug.slugify('Hi There'))",
			"import code_skills.slug\nfrom code_skills.slug import slugify\nprint(code_skills.slug.slugify('A  B'), slugify('C d'))",
			`r = await skills.propose_code("slug", ${py(BROKEN)}, ${py(BROKEN_TEST)}, "regression")\nprint(r["status"], r["version"], r["previous"])\nfrom code_skills import slug\nprint(slug.__skill_version__, slug.slugify('A b'))`,
			show(`await skills.propose_code("slug", ${py(SLUG_V2)}, ${py(SLUG_TEST_V2)}, ["v2", {"ref": "blog"}])`),
			"from code_skills import slug\nprint(slug.__skill_version__, slug.slugify('A b'))",
			`import json as _json\n${attempt('skills.rollback("slug", 4)')}\n${attempt('skills.rollback("slug", 6)')}\n${attempt('skills.rollback("slug", 99)')}\n${attempt('skills.rollback("nosuch", 1)')}\n${attempt('skills.rollback("slug", 0)')}`,
			"from code_skills import slug\nprint(slug.__skill_version__, slug.slugify('A b'))",
			`import json as _json\n${attempt('skills.rollback("slug", 4)')}\ntry:\n    from code_skills import slug\nexcept ImportError as e:\n    print("ImportError", e)\nprint(dir(__import__("code_skills")))`,
			show(`await skills.propose_code("slug", ${py(SLUG_V1)}, ${py(SLUG_TEST_V1)}, "v3")`),
			show("await skills.code_list()"),
			show('await skills.code_history("slug")'),
		],
	],
	[
		"validation, secrets, isolation, policy",
		[
			`import json as _json\n${[
				'skills.propose_code("Bad", "x=1", "def test_a(): pass", "e")',
				'skills.propose_code("class", "x=1", "def test_a(): pass", "e")',
				'skills.propose_code("a" * 65, "x=1", "def test_a(): pass", "e")',
				'skills.propose_code("ok", "x=1", "def test_a(): pass", "")',
				'skills.propose_code("ok", "x=1", "def test_a(): pass", {"a": [None, " "]})',
				'skills.propose_code("ok", "x=1", "def test_a(): pass", "e" * 17000)',
				'skills.propose_code("ok", "x" * 70000, "def test_a(): pass", "e")',
				'skills.propose_code("ok", "   ", "def test_a(): pass", "e")',
				'skills.propose_policy("ok", "def plan(): pass", "def test_a(): pass", "e", family="  ")',
				'skills.code_history("nosuch")',
				'skills.code_history("Bad")',
				'skills.code_list(kind="other")',
				'skills.code_list(family="")',
				'rlm.host_request("skills.propose_code", {"name": "x", "source": "a", "test_source": "b", "evidence": "e", "bogus": 1})',
				'rlm.host_request("skills.code_list", {"extra": 1})',
				'rlm.host_request("skills.unknown", {})',
			]
				.map(attempt)
				.join("\n")}`,
			`import json as _json\nfor name, source, test, evidence in [\n    ("deploy", 'API_KEY = "${j("sk-abcdefgh", "ijklmnop1234")}"\\ndef deploy():\\n    return API_KEY\\n', "def test_x():\\n    pass\\n", "deploys"),\n    ("fetcher", ${py(SLUG_V1)}, 'TOKEN = "${j("ghp_abcdefgh", "ijklmnopqrstuvwx")}"\\ndef test_x():\\n    pass\\n', "fetches"),\n    ("notes", ${py(SLUG_V1)}, ${py(SLUG_TEST_V1)}, "the key is ${j("AKIA", "ABCDEFGHIJKLMNOP")}"),\n]:\n    print(_json.dumps(await skills.propose_code(name, source, test, evidence), sort_keys=True))`,
			show(`await skills.propose_code("probe", '"""Probe."""\\nimport os\\nMARKER = os.getcwd()\\n', 'import os\\nfrom code_skills import probe\\ndef test_fresh_kernel():\\n    assert "state" not in globals() or globals()["state"] == {}\\n    assert os.path.basename(os.getcwd()).startswith("ultron-skill-test-")\\nasync def test_no_host():\\n    import __main__\\n    try:\\n        await __main__._STATE.namespace["skills"].list()\\n    except RuntimeError as error:\\n        print("refused:", error)\\n    else:\\n        raise AssertionError("host request succeeded")\\ndef helper():\\n    pass\\n', {"ref": "isolation"})`),
			`r = await skills.propose_code("sleepy", "def f():\\n    return 1\\n", "import time\\ndef test_slow():\\n    time.sleep(30)\\n", "slow")\nprint(r["status"], r["test"]["error"], r["test"]["tests"])`,
			`import os\nos.makedirs(os.path.join(os.environ["ULTRON_CODE_SKILLS_DIR"], "manual"), exist_ok=True)\nopen(os.path.join(os.environ["ULTRON_CODE_SKILLS_DIR"], "manual", "SKILL.md"), "w").write("---\\nname: manual\\ndescription: d\\n---\\nbody\\n")\nimport json as _json\n${attempt('skills.propose_code("manual", "x = 1", "def test_a(): pass", "e")')}`,
			show(`await skills.propose_policy("hourly", '"""Split a log into per-hour tasks."""\\ndef plan(handle, hours=24):\\n    return [{"task": f"summarize hour {h}", "slice": [h * 100, (h + 1) * 100]} for h in range(hours)]\\nclass Helper:\\n    """A helper."""\\nasync def fetch(url: str, *, retries: int = 3) -> bytes:\\n    """Fetch it.\\n\\n    Details."""\\n', 'from code_skills import hourly\\ndef test_plan():\\n    tasks = hourly.plan(None, hours=3)\\n    assert [t["slice"] for t in tasks] == [[0, 100], [100, 200], [200, 300]]\\n', {"map": "8 frames, all complete"}, family="logs-forensics")`),
			`r = await skills.propose_policy("noplan", "def split(x):\\n    return [x]\\n", "def test_ok():\\n    pass\\n", "x")\nprint(r["status"], r["test"]["tests"][0]["name"], r["test"]["tests"][0]["ok"], r["test"]["tests"][1:])`,
			`import tempfile, os, importlib.util\nsrc = "def by_chunks(handle, n=2):\\n    return [{'task': 'chunk', 'index': i} for i in range(n)]\\n"\npath = os.path.join(tempfile.mkdtemp(), "planner.py")\nopen(path, "w").write(src)\nspec = importlib.util.spec_from_file_location("planner", path); planner = importlib.util.module_from_spec(spec); spec.loader.exec_module(planner)\nr = await skills.propose_policy("chunked", planner.by_chunks, "from code_skills import chunked\\ndef test_plan():\\n    assert len(chunked.plan(None, n=3)) == 3\\n", "worked on logs", family="logs")\nfrom code_skills import chunked\nprint(r["status"], r["kind"], len(chunked.plan(None)))\nh = await skills.code_history("chunked")\nprint(h["versions"][0]["family"], [s["name"] for s in await skills.code_list(kind="policy", family="logs")])`,
			show("[await skills.code_list(kind='code'), await skills.code_list(kind='policy'), await skills.code_list(family='logs-forensics'), await skills.code_list(family='other')]"),
			`import os\np = os.path.join(os.environ["ULTRON_CODE_SKILLS_DIR"], "probe", ".versions", "1", "skill.py")\nopen(p, "w").write("MARKER = 'tampered'\\n")\ntry:\n    from code_skills import probe\nexcept ImportError as e:\n    print("ImportError", e)`,
		],
	],
	[
		"catalog: list, select, load, why, refresh",
		[
			show("await skills.list()"),
			show('await skills.select("write release notes for the deploy")'),
			show('await skills.select("slug-helper", limit=1)'),
			show('await skills.select("the a of")'),
			show('await skills.select("x z slug")'),
			`import json as _json\nd = await skills.select("release")\nprint(_json.dumps(d["candidates"], sort_keys=True))\nw = await skills.why(d["id"])\nprint(w == d)`,
			show('await skills.load("deploy-notes")'),
			`import json as _json\nv = (await skills.list())["skills"][1]["version"]\nprint(_json.dumps(await skills.load("slug-helper", version=v), sort_keys=True))\n${[
				'skills.load("slug-helper", version="0" * 64)',
				'skills.load("nosuch")',
				'skills.load("nosuch", version="0" * 64)',
				'skills.load("slug-helper", version="ABC")',
				'skills.load("")',
				'skills.select("")',
				'skills.select("x", limit=0)',
				'skills.select("x", limit=51)',
				'skills.select("x" * 4001)',
				'skills.why("nope")',
				'skills.invoke("hidden", "0" * 64, {})',
			]
				.map(attempt)
				.join("\n")}`,
			show("await skills.refresh()"),
		],
	],
];

// ---------------------------------------------------------------------------------------------------------------
// memory: worker services (createWorkerServices) with a deterministic in-memory Hindsight stand-in.

/**
 * A Hindsight stand-in with a lagging index: a retain appends a new indexed entry (older entries of the same
 * document stay recallable) and a delete leaves the entries recallable, so the journal's supersession and forget
 * rules are what keep them out of context. Recall returns the entries whose tags equal the request's, in order.
 */
export function fakeHindsight(calls) {
	const entries = [];
	const documents = new Map();
	const operations = new Map();
	let next = 0;
	return {
		namespace: "fake://bank",
		scopeTags: { session: ["ultron:session:s1"], project: ["ultron:project:p1"] },
		async recall(request) {
			calls.push(["recall", structuredClone(request)]);
			const same = (tags) => tags.length === request.tags.length && tags.every((tag) => request.tags.includes(tag));
			return {
				results: entries
					.filter((entry) => same(entry.tags) && (request.query === "*" || entry.text.toLowerCase().includes(request.query.toLowerCase())))
					.map((entry) => ({ id: entry.id, text: entry.text, tags: [...entry.tags], document_id: entry.document_id, type: "world", metadata: { ...entry.metadata } })),
			};
		},
		async retain(request) {
			calls.push(["retain", structuredClone(request)]);
			const item = request.items[0];
			next += 1;
			entries.push({ id: `entry-${next}`, text: item.content, tags: [...item.tags], document_id: item.document_id, metadata: { ...item.metadata } });
			documents.set(item.document_id, { id: item.document_id, original_text: item.content, tags: [...item.tags] });
			operations.set(request.operation_id, item.content.includes("pending") ? "pending" : "completed");
			if (item.content.includes("sync")) return { success: true, async: false };
			return { success: true, async: true, operation_id: request.operation_id };
		},
		async get(id) {
			calls.push(["get", id]);
			return structuredClone(documents.get(id) ?? { id, original_text: null, tags: [] });
		},
		async delete(id) {
			calls.push(["delete", id]);
			documents.delete(id);
			return { success: true, document_id: id };
		},
		async operation(id) {
			calls.push(["operation", id]);
			return { operation_id: id, status: operations.get(id) ?? "not_found" };
		},
	};
}

function memoryHost() {
	const values = new Map();
	const session = {
		getValue: async (address) => (values.has(`${address.namespace}/${address.key}`) ? { value: structuredClone(values.get(`${address.namespace}/${address.key}`)) } : undefined),
		setValue: async (address, next) => {
			values.set(`${address.namespace}/${address.key}`, structuredClone(next));
		},
		scanValues: async () => [],
	};
	const calls = [];
	const services = createWorkerServices({ session, sessionId: "s1", cwd: "/work", backend: fakeHindsight(calls) });
	return {
		services,
		state: async () => ({ journal: values.get("ultron.memory.state/root") ?? null, backend: calls }),
	};
}

const memoryScenarios = [
	[
		"propose, prepare, correct, forget",
		[
			show('await memory.propose("The build uses cargo workspaces", [{"ref": "Cargo.toml"}])'),
			show('await memory.prepare("cargo")'),
			"import json as _json\na = await memory.prepare('cargo')\nb = await memory.prepare('cargo')\nc = await memory.prepare('cargo', task_id='other')\nprint(a == b, a['operation']['id'] == c['operation']['id'])",
			"import json as _json\nops = await memory.list()\nmid = ops[0]['memoryId']\nprint(_json.dumps(await memory.get(mid), sort_keys=True))",
			`import json as _json\nmid = (await memory.list())[0]['memoryId']\nprint(_json.dumps(await memory.correct(mid, "The build uses cargo with resolver 3", [{"ref": "Cargo.toml", "sha256": "${"a".repeat(64)}"}]), sort_keys=True))`,
			show('await memory.prepare("cargo")'),
			"import json as _json\nmid = (await memory.list())[0]['memoryId']\nprint(_json.dumps(await memory.get(mid), sort_keys=True))",
			"import json as _json\nmid = (await memory.list())[0]['memoryId']\nprint(_json.dumps(await memory.forget(mid), sort_keys=True))",
			show('await memory.prepare("cargo")'),
			`import json as _json\nmid = (await memory.list())[0]['memoryId']\n${[
				"memory.get(mid)",
				'memory.correct(mid, "again", [{"ref": "x"}])',
				"memory.forget(mid)",
			]
				.map(attempt)
				.join("\n")}`,
			show("await memory.list()"),
			show('await memory.why("rlm")'),
		],
	],
	[
		"scopes, gate, validation",
		[
			show('await memory.propose("Project style: tabs, width 120", [{"ref": "biome.json"}], scope="project")'),
			show('await memory.propose("Session note sync stored", [{"ref": "a"}, {"ref": "b"}])'),
			show('await memory.propose("still pending work", [{"ref": "a"}])'),
			"import json as _json\nops = await memory.list()\nfor op in ops[1:3]:\n    print(_json.dumps(await memory.get(op['memoryId']), sort_keys=True))",
			show('[await memory.prepare("*"), await memory.prepare("*", scope="project")]'),
			show(`await memory.propose("the ${j("api", "_key")} = ${j("abc12", "3xyz")}", [{"ref": "env"}])`),
			show(`await memory.propose("ssn ${j("123-45", "-6789")}", [{"ref": "form"}])`),
			`import json as _json\n${[
				'memory.propose("x", [])',
				'memory.propose("", [{"ref": "a"}])',
				'memory.propose("x", [{"ref": "a"}], scope="global")',
				'memory.propose("x", [{"ref": "a"}], scope="team")',
				'memory.propose("x", [{"ref": "a", "extra": 1}])',
				'memory.propose("x", [{"ref": "a", "sha256": "zz"}])',
				'memory.prepare("q", scope="global")',
				'memory.prepare("")',
				'memory.get("nope")',
				'memory.forget("nope")',
				'memory.correct("nope", "t", [{"ref": "a"}])',
				'memory.why("")',
				'rlm.host_request("memory.list", {"x": 1})',
				'rlm.host_request("memory.prepare", {"query": "q", "taskId": "t", "refresh": True})',
				'rlm.host_request("memory.propose", {"text": "verified fact", "evidence": [{"ref": "t"}], "evidenceClass": "verified"})',
				'rlm.host_request("memory.propose", {"text": "x", "evidence": [{"ref": "t"}], "evidenceClass": "guess"})',
				'rlm.host_request("memory.nope", {})',
			]
				.map(attempt)
				.join("\n")}`,
			show('await memory.prepare("fact")'),
			show("await memory.why('rlm')"),
		],
	],
];

// ---------------------------------------------------------------------------------------------------------------
// grants and hints: host modules over the session's module store.


function grantsHost() {
	const store = createMemoryModuleStore();
	let clock = FIXED_NOW;
	const module = createGrantModule({ store, now: () => clock });
	return {
		modules: [module, { prefixes: ["test.clock"], handle: async (request) => ((clock += request.payload.ms), clock) }],
		state: async () => (await store.read()) ?? null,
	};
}

const grantsScenarios = [
	[
		"issue, list, check, revoke",
		[
			show("await grants.list()"),
			show('await grants.issue("repo:/work/ultron", "abc123", "deploy-policy@1", "deploy", 60000)'),
			"import json as _json\ng = (await grants.list())['grants'][0]\nprint(_json.dumps(await grants.check(g['id'], 'repo:/work/ultron', 'abc123', 'deploy-policy@1', 'deploy'), sort_keys=True))\nprint(_json.dumps(await grants.check(g['id'], 'task:other', 'def', 'deploy-policy@2', 'push'), sort_keys=True))",
			"import json as _json\nawait rlm.host_request('test.clock', {'ms': 60000})\ng = (await grants.list())['grants'][0]\nprint(_json.dumps(await grants.check(g['id'], 'repo:/work/ultron', 'abc123', 'deploy-policy@1', 'deploy'), sort_keys=True))",
			"import json as _json\ng = await grants.issue('task:t1', 'sha256:ff', 'p.x@v1', 'merge.pr', 1000)\nprint(_json.dumps(await grants.revoke(g['id']), sort_keys=True))\nawait rlm.host_request('test.clock', {'ms': 5})\nprint(_json.dumps(await grants.revoke(g['id']), sort_keys=True))\nprint(_json.dumps(await grants.check(g['id'], 'task:t1', 'sha256:ff', 'p.x@v1', 'merge.pr'), sort_keys=True))",
			`import json as _json\n${[
				"grants.issue('', 'r', 'p@1', 'a', 1)",
				"grants.issue('Repo:x', 'r', 'p@1', 'a', 1)",
				"grants.issue('a' * 33 + ':x', 'r', 'p@1', 'a', 1)",
				"grants.issue('repo: x', 'r', 'p@1', 'a', 1)",
				"grants.issue('repo:' + 'x' * 1024, 'r', 'p@1', 'a', 1)",
				"grants.issue('repo:' + 'x' * 1025, 'r', 'p@1', 'a', 1)",
				"grants.issue('repo:x', 'r', 'p@1', 'a', 0)",
				"grants.issue('repo:x', 'r', 'p@1', 'a', 1.5)",
				"grants.issue('repo:x', 'r', 'p@1', 'a', 30 * 24 * 3600 * 1000 + 1)",
				"grants.issue('repo:x', 'r', 'p@1', 'a', True)",
				"grants.issue('repo:x', 'r' * 257, 'p@1', 'a', 1)",
				"grants.issue('repo:x', 'r r', 'p@1', 'a', 1)",
				"grants.issue('repo:x', 'r', 'p', 'a', 1)",
				"grants.issue('repo:x', 'r', 'P@1', 'a', 1)",
				"grants.issue('repo:x', 'r', 'p@1', 'a b', 1)",
				"grants.issue('repo:x', 'r', 'p@1', 'A', 1)",
				"grants.revoke('nope')",
				"grants.revoke('')",
				"grants.revoke(' ')",
				"grants.check('nope', 'repo:x', 'r', 'p@1', 'a')",
				"grants.check('nope', 'repo:x', 'r', 'p', 'a')",
				"rlm.host_request('grants.list', {'x': 1})",
				"rlm.host_request('grants.issue', {'scope': 'repo:x', 'revision': 'r', 'policy': 'p@1', 'action': 'a', 'ttl_ms': 5, 'owner': 'me'})",
				"rlm.host_request('grants.nope', {})",
			]
				.map(attempt)
				.join("\n")}`,
			show("await grants.list()"),
		],
	],
];

function hintsHost() {
	const store = createMemoryModuleStore();
	const hints = new CellHints({ store, now: () => FIXED_NOW });
	return { modules: [hints.module], state: async () => (await store.read()) ?? null };
}

const hintsScenarios = [
	[
		"mute, unmute, muted",
		[
			"print(await hints.muted())",
			"print(await hints.mute('stuck-loop'))",
			"print(await hints.mute('stuck-loop', 'repeated-failure'))",
			"print(await hints.muted())",
			"print(await hints.unmute('stuck-loop'))",
			"print(await hints.unmute('stuck-loop'))",
			`import json as _json\n${["hints.mute('bogus')", "hints.mute()", "hints.mute('')", "rlm.host_request('hints.mute', {'tags': []})", "rlm.host_request('hints.mute', {'tags': [1]})", "rlm.host_request('hints.nope', {})"]
				.map((call) => `try:\n    print("ok", await ${call})\nexcept Exception as e:\n    print(type(e).__name__, e)`)
				.join("\n")}`,
			"print(repr(hints))",
		],
	],
];

const areas = {
	skills: [skillsHost, skillsScenarios],
	memory: [memoryHost, memoryScenarios],
	grants: [grantsHost, grantsScenarios],
	hints: [hintsHost, hintsScenarios],
};

/** Records each area's scenarios to fixtures/<area>.json. */
export async function record(areas, wanted = []) {
	for (const [area, [make, scenarios]] of Object.entries(areas)) {
		if (wanted.length && !wanted.includes(area)) continue;
		const recorded = [];
		for (const [name, cells] of scenarios) recorded.push(await scenario(name, make, cells));
		writeFileSync(join(here, `${area}.json`), `${JSON.stringify({ area, scenarios: recorded }, null, "\t")}\n`);
		console.log(`${area}: ${recorded.length} scenarios`);
	}
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) await record(areas, process.argv.slice(2));
