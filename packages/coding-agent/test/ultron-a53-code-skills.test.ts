import { existsSync, mkdtempSync, readdirSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, describe, expect, test } from "vitest";
import { getRlmRuntimePath } from "../src/config.ts";
import { loadSkillsFromDir } from "../src/core/skills.ts";
import { CODE_SKILLS_PROMPT, CodeSkills, codeSkillsToolSection } from "../src/ultron/code-skills.ts";
import type { JevMemoryPolicy } from "../src/ultron/jev.ts";
import { createMemoryModuleStore, type NativeHostApi, ROOT_CALLER } from "../src/ultron/rlm/host-module.ts";
import { RlmKernel } from "../src/ultron/rlm/kernel.ts";
import { createSkillModule } from "../src/ultron/skills.ts";

const SLUG_V1 = `"""Turn free text into URL slugs."""
import re

def slugify(text: str) -> str:
    """Lowercase, keep alphanumerics, join words with hyphens."""
    return "-".join(re.findall(r"[a-z0-9]+", text.lower()))
`;
const SLUG_V2 = `"""Turn free text into URL slugs (v2: underscores)."""
import re

def slugify(text: str) -> str:
    return "_".join(re.findall(r"[a-z0-9]+", text.lower()))
`;
const SLUG_TEST_V1 = `from code_skills import slug
def test_slugify():
    assert slug.slugify("Hello, World!") == "hello-world"
`;
const SLUG_TEST_V2 = `from skill import slugify
async def test_slugify():
    assert slugify("Hello, World!") == "hello_world"
`;
const BROKEN = `def slugify(text):\n    return text.upper()\n`;
const BROKEN_TEST = `from code_skills import slug\ndef test_slugify():\n    assert slug.slugify("a b") == "a-b"\n`;

const roots: string[] = [];
const kernels: RlmKernel[] = [];
afterEach(async () => {
	await Promise.all(kernels.splice(0).map((kernel) => kernel.shutdown()));
	for (const root of roots.splice(0)) rmSync(root, { recursive: true, force: true });
});

function skillsDir(): string {
	const root = mkdtempSync(join(tmpdir(), "ultron-a53-"));
	roots.push(root);
	return join(root, "skills");
}

/** A kernel as a worker starts one: code skills come from the profile's skills directory. */
function kernel(dir: string, host: (type: string, payload: Record<string, unknown>) => unknown = () => null) {
	const created = new RlmKernel(
		{ cwd: tmpdir(), runtimePath: getRlmRuntimePath(), env: { ULTRON_CODE_SKILLS_DIR: dir } },
		host,
	);
	kernels.push(created);
	return created;
}

async function run(target: RlmKernel, code: string): Promise<string> {
	const result = await target.execute(code);
	if (result.status === "error") return `ERROR ${result.error?.ename}: ${result.error?.evalue}`;
	return [result.stdout, result.result === "None" ? "" : result.result].filter(Boolean).join("\n").trim();
}

const noJev = { skillPolicy: async (): Promise<JevMemoryPolicy> => ({ action: "keep", confidence: 0.9 }) };

describe("A53: code skills are tested before activation, versioned, and roll back", () => {
	test("a proposed skill whose test fails is never importable", async () => {
		const dir = skillsDir();
		const skills = new CodeSkills({ dir, jev: noJev });
		const rejected = await skills.propose("code", {
			name: "slug",
			source: BROKEN,
			test_source: BROKEN_TEST,
			evidence: "first try",
		});
		expect(rejected.status).toBe("rejected");
		expect(rejected.test?.passed).toBe(false);
		expect(rejected.test?.tests[0]?.error).toContain("AssertionError");
		// The version is kept for inspection, but nothing is published or importable.
		expect(existsSync(join(dir, "slug", ".versions", "1", "skill.py"))).toBe(true);
		expect(existsSync(join(dir, "slug", "skill.py"))).toBe(false);
		expect(loadSkillsFromDir({ dir, source: "user" }).skills).toEqual([]);
		const fresh = kernel(dir);
		expect(await run(fresh, "from code_skills import slug")).toMatch(/^ERROR ImportError/);
		expect(await run(fresh, "import code_skills.slug")).toMatch(/^ERROR ModuleNotFoundError/);
		// A test with no test functions, and a skill that does not import, fail too.
		const empty = await skills.propose("code", {
			name: "slug",
			source: SLUG_V1,
			test_source: "x = 1\n",
			evidence: "no tests",
		});
		expect(empty.status).toBe("rejected");
		expect(empty.test?.error).toContain("no test_* functions");
		const syntax = await skills.propose("code", {
			name: "slug",
			source: "def slugify(:\n",
			test_source: SLUG_TEST_V1,
			evidence: "typo",
		});
		expect(syntax.status).toBe("rejected");
		expect(syntax.test?.error).toContain("SyntaxError");
		expect(await run(fresh, "from code_skills import slug")).toMatch(/^ERROR ImportError/);

		// Once a version is active, a failing successor never replaces it.
		expect(
			(await skills.propose("code", { name: "slug", source: SLUG_V1, test_source: SLUG_TEST_V1, evidence: "ok" }))
				.status,
		).toBe("active");
		const regression = await skills.propose("code", {
			name: "slug",
			source: BROKEN,
			test_source: BROKEN_TEST,
			evidence: "regression",
		});
		expect(regression.status).toBe("rejected");
		expect(
			await run(kernel(dir), "from code_skills import slug\nprint(slug.__skill_version__, slug.slugify('A b'))"),
		).toBe("4 a-b");
		expect(skills.history("slug").versions.map((item) => item.state)).toEqual([
			"rejected",
			"rejected",
			"rejected",
			"active",
			"rejected",
		]);
	}, 60_000);

	test("the test runs in a fresh isolated kernel without host capabilities", async () => {
		const dir = skillsDir();
		const skills = new CodeSkills({ dir });
		const probe = await skills.propose("code", {
			name: "probe",
			source: `"""Probe."""\nimport os\nMARKER = os.getcwd()\n`,
			test_source: `import os\nfrom code_skills import probe\ndef test_fresh_kernel():\n    assert "state" not in globals() or globals()["state"] == {}\n    assert os.path.basename(os.getcwd()).startswith("ultron-skill-test-")\nasync def test_no_host():\n    import __main__\n    assert "skills" not in globals()\n    try:\n        await __main__._STATE.namespace["skills"].list()\n    except RuntimeError as error:\n        assert "unavailable" in str(error)\n    else:\n        raise AssertionError("host request succeeded")\n`,
			evidence: { ref: "isolation" },
		});
		expect(probe, JSON.stringify(probe)).toMatchObject({ status: "active" });
		expect(probe.test?.tests).toEqual([
			{ name: "test_fresh_kernel", ok: true },
			{ name: "test_no_host", ok: true },
		]);
		expect(probe.status).toBe("active");
		// Jev is not configured: the proposal is not blocked, and the missing judgement is recorded.
		expect(probe.jev).toEqual({ status: "unavailable", reason: "Jev is not configured" });
	}, 60_000);

	test("a passing skill is importable in every kernel, listed with its docstring, and survives worker restart", async () => {
		const dir = skillsDir();
		const store = createMemoryModuleStore();
		const module = createSkillModule({
			store,
			loadSkills: async () => [],
			code: new CodeSkills({ dir, jev: noJev }),
		});
		const host = { callerTaskId: () => null } as unknown as NativeHostApi;
		const request = (type: string, payload: Record<string, unknown>) =>
			module.handle({ type, payload, caller: ROOT_CALLER, context: {} as never }, host);
		// The model proposes through the kernel API, as it would in the rlm tool.
		const proposer = kernel(dir, (type, payload) => request(type, payload));
		const output = await run(
			proposer,
			`r = await skills.propose_code("slug", ${JSON.stringify(SLUG_V1)}, ${JSON.stringify(SLUG_TEST_V1)}, {"task": "slugs", "worked": True})\nprint(r["status"], r["version"], r["import"])`,
		);
		expect(output).toBe("active 1 from code_skills import slug");
		// The already-running kernel sees it without restarting.
		expect(await run(proposer, "from code_skills import slug\nslug.slugify('Hi There')")).toBe("'hi-there'");
		// The journal recorded the outcome.
		const journal = (await store.read()) as { code: Array<Record<string, unknown>> };
		expect(journal.code).toMatchObject([
			{ action: "propose_code", name: "slug", version: 1, status: "active", test: { passed: true, tests: 1 } },
		]);
		await proposer.shutdown();

		// A new worker (fresh CodeSkills, fresh kernels) finds the active version on disk.
		const restarted = new CodeSkills({ dir });
		expect(restarted.list()).toMatchObject([
			{
				name: "slug",
				version: 1,
				kind: "code",
				doc: "Turn free text into URL slugs.",
				functions: [{ name: "slugify", signature: "slugify(text: str) -> str" }],
			},
		]);
		const description = codeSkillsToolSection(restarted);
		expect(description).toContain(CODE_SKILLS_PROMPT);
		expect(description).toContain("- slug v1: Turn free text into URL slugs. [slugify(text: str) -> str]");
		for (const statement of [
			"from code_skills import slug\nprint(slug.slugify('A  B'))",
			"import code_skills.slug\nprint(code_skills.slug.slugify('A  B'))",
			"from code_skills.slug import slugify\nprint(slugify('A  B'))",
		])
			expect(await run(kernel(dir), statement)).toBe("a-b");
		// The published SKILL.md is a valid Pi skill, so the ordinary skill catalog lists it as well.
		const listed = loadSkillsFromDir({ dir, source: "user" });
		expect(listed.skills.map((skill) => [skill.name, skill.description])).toEqual([
			["slug", "Turn free text into URL slugs."],
		]);
		expect(readFileSync(join(dir, "slug", "SKILL.md"), "utf8")).toContain("from code_skills import slug");
		// The `skills` API object is untouched by the package.
		expect(await run(kernel(dir), "type(skills).__name__")).toBe("'Skills'");
	}, 60_000);

	test("rollback follows refinement semantics and never leaves a stale import", async () => {
		const dir = skillsDir();
		const skills = new CodeSkills({ dir });
		expect(
			(await skills.propose("code", { name: "slug", source: SLUG_V1, test_source: SLUG_TEST_V1, evidence: "v1" }))
				.status,
		).toBe("active");
		const live = kernel(dir);
		expect(await run(live, "from code_skills import slug\nslug.slugify('A b')")).toBe("'a-b'");
		const second = await skills.propose("code", {
			name: "slug",
			source: SLUG_V2,
			test_source: SLUG_TEST_V2,
			evidence: "v2",
		});
		expect(second).toMatchObject({ status: "active", version: 2, previous: 1 });
		expect(await run(live, "from code_skills import slug\nslug.slugify('A b')")).toBe("'a_b'");
		// Only the active version can be rolled back.
		await expect(skills.rollback({ name: "slug", version: 1 })).rejects.toThrow("Only the active version");
		expect(await skills.rollback({ name: "slug", version: 2 })).toEqual({ name: "slug", rolledBack: 2, active: 1 });
		expect(await run(live, "from code_skills import slug\nprint(slug.__skill_version__, slug.slugify('A b'))")).toBe(
			"1 a-b",
		);
		expect(readFileSync(join(dir, "slug", "skill.py"), "utf8")).toBe(SLUG_V1);
		expect(skills.history("slug").versions.map((item) => [item.version, item.state])).toEqual([
			[1, "active"],
			[2, "rolled_back"],
		]);
		// Rolling back the first version leaves nothing importable and nothing published.
		expect(await skills.rollback({ name: "slug", version: 1 })).toEqual({
			name: "slug",
			rolledBack: 1,
			active: null,
		});
		expect(await run(live, "from code_skills import slug")).toMatch(/^ERROR ImportError/);
		expect(readdirSync(join(dir, "slug")).filter((name) => !name.startsWith("."))).toEqual([]);
		expect(loadSkillsFromDir({ dir, source: "user" }).skills).toEqual([]);
		// A later passing proposal starts a fresh chain.
		const third = await skills.propose("code", {
			name: "slug",
			source: SLUG_V1,
			test_source: SLUG_TEST_V1,
			evidence: "v3",
		});
		expect(third).toMatchObject({ status: "active", version: 3, previous: null });
		// A file tampered with after activation is refused rather than imported.
		const { writeFileSync } = await import("node:fs");
		writeFileSync(join(dir, "slug", ".versions", "3", "skill.py"), "def slugify(text):\n    return 'tampered'\n");
		expect(await run(kernel(dir), "from code_skills import slug")).toMatch(/^ERROR ImportError/);
	}, 90_000);

	test("Jev scores proposals: sensitive ones are refused, an outage never blocks", async () => {
		const dir = skillsDir();
		const decisions: string[] = [];
		const skills = (policy: () => Promise<JevMemoryPolicy>) =>
			new CodeSkills({
				dir,
				jev: {
					skillPolicy: async (name) => {
						decisions.push(name);
						return policy();
					},
				},
			});
		const kept = await skills(async () => ({ action: "keep", confidence: 0.8 })).propose("code", {
			name: "slug",
			source: SLUG_V1,
			test_source: SLUG_TEST_V1,
			evidence: "slugs for the blog",
		});
		expect(kept.jev).toEqual({ status: "ok", action: "keep", confidence: 0.8 });
		expect(decisions).toEqual(["slug"]);
		// A low-relevance judgement is recorded but a deliberate proposal still proceeds, as for memory.
		const skipped = await skills(async () => ({ action: "skip", confidence: 0.9 })).propose("code", {
			name: "slug",
			source: SLUG_V2,
			test_source: SLUG_TEST_V2,
			evidence: "one-off",
		});
		expect(skipped).toMatchObject({ status: "active", jev: { action: "skip" } });
		const outage = await skills(async () => {
			throw new Error("Jev UNAVAILABLE");
		}).propose("code", {
			name: "other",
			source: SLUG_V1,
			test_source: "from code_skills import other\ndef test_x():\n    assert other.slugify('A b') == 'a-b'\n",
			evidence: "outage",
		});
		expect(outage.jev).toEqual({ status: "unavailable", reason: "Jev UNAVAILABLE" });
		expect(outage.status).toBe("active");
		const sensitive = await skills(async () => ({ action: "sensitive", confidence: 0.95 })).propose("code", {
			name: "leaky",
			source: SLUG_V1,
			test_source: SLUG_TEST_V1,
			evidence: "customer records",
		});
		expect(sensitive).toMatchObject({ status: "refused", version: null, test: null });
		expect(existsSync(join(dir, "leaky", ".versions"))).toBe(false);
		// Secrets are refused deterministically even when Jev is down or absent.
		const secret = await new CodeSkills({ dir }).propose("code", {
			name: "deploy",
			source: `API_KEY = "sk-abcdefghijklmnop1234"\ndef deploy():\n    return API_KEY\n`,
			test_source: "def test_x():\n    pass\n",
			evidence: "deploys",
		});
		expect(secret).toMatchObject({ status: "refused", jev: { status: "deterministic", action: "sensitive" } });
		expect(existsSync(join(dir, "deploy", ".versions"))).toBe(false);
	}, 90_000);

	test("policy skills store any plan function with a test, filterable by task family", async () => {
		const dir = skillsDir();
		const skills = new CodeSkills({ dir });
		const plan = `"""Split a log into per-hour tasks."""\ndef plan(handle, hours=24):\n    return [{"task": f"summarize hour {h}", "slice": [h * 100, (h + 1) * 100]} for h in range(hours)]\n`;
		const test = `from code_skills import hourly\ndef test_plan():\n    tasks = hourly.plan(None, hours=3)\n    assert [t["slice"] for t in tasks] == [[0, 100], [100, 200], [200, 300]]\n`;
		const stored = await skills.propose("policy", {
			name: "hourly",
			source: plan,
			test_source: test,
			evidence: { map: "8 frames, all complete" },
			family: "logs-forensics",
		});
		expect(stored).toMatchObject({ status: "active", kind: "policy", version: 1 });
		expect(stored.test?.tests.map((item) => item.name)).toEqual(["policy_defines_plan", "test_plan"]);
		expect(skills.list({ kind: "policy", family: "logs-forensics" }).map((item) => item.name)).toEqual(["hourly"]);
		expect(skills.list({ family: "other" })).toEqual([]);
		expect(skills.describe()).toContain("- hourly v1 (policy, logs-forensics): Split a log into per-hour tasks.");
		// A module without plan() is not a policy, whatever its own test says.
		const noPlan = await skills.propose("policy", {
			name: "noplan",
			source: "def split(x):\n    return [x]\n",
			test_source: "def test_ok():\n    pass\n",
			evidence: "x",
		});
		expect(noPlan.status).toBe("rejected");
		expect(noPlan.test?.tests[0]).toMatchObject({ name: "policy_defines_plan", ok: false });

		// Through the kernel API a plan function importable from a file is accepted as-is; its name is aliased to plan.
		const store = createMemoryModuleStore();
		const module = createSkillModule({ store, loadSkills: async () => [], code: skills });
		const host = { callerTaskId: () => null } as unknown as NativeHostApi;
		const live = kernel(dir, (type, payload) =>
			module.handle({ type, payload, caller: ROOT_CALLER, context: {} as never }, host),
		);
		const output = await run(
			live,
			`import textwrap, types, linecache
src = "def by_chunks(handle, n=2):\\n    return [{'task': 'chunk', 'index': i} for i in range(n)]\\n"
import tempfile, os, importlib.util
path = os.path.join(tempfile.mkdtemp(), "planner.py")
open(path, "w").write(src)
spec = importlib.util.spec_from_file_location("planner", path); planner = importlib.util.module_from_spec(spec); spec.loader.exec_module(planner)
r = await skills.propose_policy("chunked", planner.by_chunks, "from code_skills import chunked\\ndef test_plan():\\n    assert len(chunked.plan(None, n=3)) == 3\\n", "worked on logs", family="logs")
from code_skills import chunked
print(r["status"], r["kind"], len(chunked.plan(None)))
h = await skills.code_history("chunked")
print(h["versions"][0]["family"], [s["name"] for s in await skills.code_list(kind="policy", family="logs")])`,
		);
		expect(output).toBe("active policy 2\nlogs ['chunked']");
		await expect(
			module.handle(
				{
					type: "skills.propose_policy",
					payload: { name: "x", source: "", test_source: "", evidence: "e", bogus: 1 },
					caller: ROOT_CALLER,
					context: {} as never,
				},
				host,
			),
		).rejects.toThrow("Unknown payload field: bogus");
		const journal = (await store.read()) as { code: Array<Record<string, unknown>> };
		expect(journal.code.map((item) => [item.action, item.status])).toEqual([
			["propose_policy", "active"],
			["propose_policy", "failed"],
		]);
	}, 90_000);
});
