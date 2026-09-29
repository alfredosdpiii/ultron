import { execFileSync } from "node:child_process";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import type { Context } from "@ultron/agent-core";
import { afterEach, describe, expect, test } from "vitest";
import { RlmKernel } from "../src/ultron/rlm/kernel.ts";
import { isConcreteEvidence, validateVerdict, verdictTag } from "../src/ultron/rlm/verdict.ts";
import { deferred, hostFixture } from "./ultron-host-fixtures.ts";

/**
 * Subagents prove what they did: `rlm.finish` records a checked verdict, the host compares its declared
 * `changed_files` with what changed on disk while the child ran, and `rlm.collect`, completion events and workflow
 * nodes carry the verdict and the check. Children that never call `rlm.finish` still return their reply, unverified.
 */

const runtimePath = fileURLToPath(new URL("../src/ultron/rlm/runtime.py", import.meta.url));

const directories: string[] = [];
const fixtures: Array<ReturnType<typeof hostFixture>> = [];
afterEach(async () => {
	for (const fixture of fixtures.splice(0)) await fixture.host.close();
	for (const directory of directories.splice(0)) await rm(directory, { recursive: true, force: true });
});

async function repository(): Promise<string> {
	const root = await mkdtemp(join(tmpdir(), "ultron-verdict-"));
	directories.push(root);
	execFileSync("git", ["init", "-q"], { cwd: root });
	await writeFile(join(root, ".gitignore"), "cache/\n");
	await writeFile(join(root, "existing.ts"), "old");
	return root;
}

type Result = {
	status: string;
	value?: string;
	error?: string;
	verdict: {
		status: string;
		summary: string;
		evidence: string[];
		changed_files: string[];
		outputs: Record<string, unknown>;
	} | null;
	check: {
		outcome: string;
		unobserved: string[];
		unreported: string[];
		unlisted: string[];
		concurrent: string[];
		reason?: string;
		problems?: string[];
	};
	unverified?: true;
};

type Finish = (payload: Record<string, unknown>) => Promise<unknown>;
type ChildScript = (finish: Finish, workspace: string, lane: string, context: Context) => Promise<string>;

/** A host whose children run `scripts[prompt]`; each script may call `rlm.finish` from its own lane. */
function verdictFixture(workspace: string, scripts: Record<string, ChildScript>) {
	const fixture = hostFixture({
		workspace,
		script: async (lane, prompt, laneContext) => {
			const script = scripts[prompt];
			if (!script) throw new Error(`no script for ${prompt}`);
			const finish: Finish = (payload) => fixture.host.handle("rlm.finish", payload, laneContext, { lane });
			return script(finish, workspace, lane, laneContext);
		},
	});
	fixtures.push(fixture);
	const spawn = async (prompt: string) =>
		(await fixture.call<{ rlm_child_id: string }>("rlm.spawn", { prompt, kwargs: { name: prompt } })).rlm_child_id;
	const collect = async (ids: string[]) =>
		(await fixture.call<{ results: Array<{ id: string; result: Result }> }>("rlm.collect", { selectors: ids }))
			.results;
	const run = async (prompt: string) => (await collect([await spawn(prompt)]))[0]!.result;
	return { fixture, spawn, collect, run };
}

describe("verdict validation", () => {
	test("status, summary and evidence are checked mechanically", () => {
		expect(validateVerdict({ status: "done", summary: "x" })).toEqual({
			problems: [expect.stringContaining('status must be "passed", "failed" or "blocked"')],
		});
		expect(validateVerdict({ status: "passed", summary: " " })).toMatchObject({
			problems: [expect.stringContaining("summary"), expect.stringContaining('"passed" needs evidence')],
		});
		expect(validateVerdict({ status: "passed", summary: "Fixed it", evidence: ["looks good", "all fine"] })).toEqual({
			problems: [expect.stringContaining("must be concrete")],
		});
		expect(validateVerdict({ status: "passed", summary: "s", evidence: ["x"], bogus: 1 })).toMatchObject({
			problems: expect.arrayContaining([expect.stringContaining('unknown field "bogus"')]),
		});
		expect(
			validateVerdict({ status: "failed", summary: "s", outputs: [], evidence: "no", changed_files: [1] }),
		).toMatchObject({
			problems: [
				expect.stringContaining("outputs must be a JSON object"),
				expect.stringContaining("evidence must be a list"),
				expect.stringContaining("changed_files must be a list"),
			],
		});
		// Failed and blocked verdicts need no evidence: saying why is enough.
		expect(validateVerdict({ status: "blocked", summary: "No network access to the registry" })).toEqual({
			verdict: {
				status: "blocked",
				summary: "No network access to the registry",
				outputs: {},
				evidence: [],
				changed_files: [],
			},
		});
	});

	test("concrete evidence names a command outcome, a count, a file or a line", () => {
		for (const item of [
			"npm test: exit 0",
			"pytest -q -> exited with code 1",
			"42 passed, 0 failed",
			"src/app.ts handles the empty case",
			"README.md documents the flag",
			"see parser:120-140",
		])
			expect(isConcreteEvidence(item), item).toBe(true);
		for (const item of ["done", "tests pass", "I checked everything carefully", "e.g. it works"])
			expect(isConcreteEvidence(item), item).toBe(false);
	});

	test("changed files are normalized relative to the workspace", () => {
		const result = validateVerdict(
			{
				status: "passed",
				summary: "s",
				evidence: ["a.ts:1"],
				changed_files: ["./src/a.ts", "/work/src/b.ts", "src/a.ts", "dir/", "/elsewhere/c.ts"],
			},
			"/work",
		);
		expect(result).toMatchObject({ verdict: { changed_files: ["src/a.ts", "src/b.ts", "dir", "/elsewhere/c.ts"] } });
	});

	test("completion events lead with the verdict and its check", () => {
		expect(
			verdictTag({
				verdict: { status: "passed" },
				check: { outcome: "verified", unobserved: [], unreported: ["x"] },
			}),
		).toBe("[passed, verified; 1 undeclared change(s)]");
		expect(
			verdictTag({
				verdict: { status: "passed" },
				check: { outcome: "contradicted", unobserved: ["a.ts"], unreported: [] },
			}),
		).toBe("[passed, contradicted; declared but unchanged: a.ts]");
		expect(verdictTag({ verdict: null, unverified: true })).toBe("[unverified]");
		expect(verdictTag({ verdict: null, unverified: true, check: { outcome: "invalid" } })).toBe(
			"[unverified: verdict rejected]",
		);
		expect(verdictTag({})).toBeUndefined();
	});
});

describe("subagent verdicts on the host", () => {
	test("an honest child is verified", async () => {
		const workspace = await repository();
		const { run } = verdictFixture(workspace, {
			honest: async (finish, root) => {
				await writeFile(join(root, "feature.ts"), "export const x = 1;");
				await writeFile(join(root, "existing.ts"), "new");
				await mkdir(join(root, "cache"));
				await writeFile(join(root, "cache", "tmp.bin"), "ignored by Git");
				expect(
					await finish({
						status: "passed",
						summary: "Added feature.ts and updated existing.ts",
						evidence: ["npm test: exit 0, 3 passed", "feature.ts:1 exports x"],
						outputs: { exported: "x" },
						changed_files: ["feature.ts", "existing.ts"],
					}),
				).toMatchObject({ recorded: true, status: "passed" });
				return "Done: feature.ts added, existing.ts updated.";
			},
		});
		const result = await run("honest");
		expect(result).toMatchObject({
			status: "succeeded",
			value: "Done: feature.ts added, existing.ts updated.",
			verification: "unverified",
			verdict: {
				status: "passed",
				outputs: { exported: "x" },
				changed_files: ["feature.ts", "existing.ts"],
			},
			check: { outcome: "verified", unobserved: [], unreported: [], unlisted: [], concurrent: [] },
		});
		expect(result.unverified).toBeUndefined();
	});

	test("a child claiming a change it did not make is contradicted", async () => {
		const workspace = await repository();
		const { run } = verdictFixture(workspace, {
			liar: async (finish, root) => {
				await writeFile(join(root, "real.ts"), "real");
				await finish({
					status: "passed",
					summary: "Updated existing.ts and created ghost.ts",
					evidence: ["existing.ts:1 updated"],
					changed_files: ["real.ts", "existing.ts", "ghost.ts"],
				});
				return "";
			},
		});
		const result = await run("liar");
		// An empty final reply after a verdict falls back to its summary.
		expect(result.value).toBe("Updated existing.ts and created ghost.ts");
		expect(result.check).toMatchObject({ outcome: "contradicted", unobserved: ["existing.ts", "ghost.ts"] });
	});

	test("a change the child did not declare is reported, not failed", async () => {
		const workspace = await repository();
		const { run } = verdictFixture(workspace, {
			quiet: async (finish, root) => {
				await writeFile(join(root, "declared.ts"), "d");
				await writeFile(join(root, "undeclared.ts"), "u");
				await finish({
					status: "passed",
					summary: "Added declared.ts",
					evidence: ["declared.ts:1"],
					changed_files: ["declared.ts"],
				});
				return "ok";
			},
		});
		expect((await run("quiet")).check).toMatchObject({
			outcome: "verified",
			unobserved: [],
			unreported: ["undeclared.ts"],
		});
	});

	test("a legacy child without rlm.finish returns its reply unverified, with what changed while it ran", async () => {
		const workspace = await repository();
		const { run } = verdictFixture(workspace, {
			legacy: async (_finish, root) => {
				await writeFile(join(root, "legacy.ts"), "l");
				return "I changed legacy.ts";
			},
		});
		const result = await run("legacy");
		expect(result).toMatchObject({
			status: "succeeded",
			value: "I changed legacy.ts",
			verdict: null,
			unverified: true,
			check: {
				outcome: "unchecked",
				unreported: ["legacy.ts"],
				reason: expect.stringContaining("without a verdict"),
			},
		});
	});

	test("a rejected verdict can be fixed; running out of attempts leaves the reply unverified", async () => {
		const workspace = await repository();
		const errors: string[] = [];
		const attempt = async (finish: Finish, payload: Record<string, unknown>) => {
			try {
				await finish(payload);
			} catch (error) {
				errors.push((error as Error).message);
			}
		};
		const { run } = verdictFixture(workspace, {
			fixes: async (finish) => {
				await attempt(finish, { status: "passed", summary: "Done" });
				await attempt(finish, { status: "passed", summary: "Done", evidence: ["verified.md:3 lists the checks"] });
				return "fixed";
			},
			exhausts: async (finish) => {
				await attempt(finish, { status: "passed", summary: "Done" });
				await attempt(finish, { status: "passed", summary: "Done", evidence: ["it works"] });
				await attempt(finish, { status: "complete", summary: "Done" });
				await attempt(finish, { status: "blocked", summary: "Too late" });
				return "gave up";
			},
		});
		const fixed = await run("fixes");
		expect(errors).toHaveLength(1);
		expect(errors[0]).toContain('"passed" needs evidence');
		expect(errors[0]).toContain("2 attempts left");
		expect(fixed).toMatchObject({ verdict: { status: "passed" }, check: { outcome: "verified" } });

		errors.length = 0;
		const exhausted = await run("exhausts");
		expect(errors[1]).toContain("must be concrete");
		expect(errors[1]).toContain("1 attempt left");
		expect(errors[2]).toContain("No attempts left");
		expect(errors[3]).toContain("no attempts left");
		expect(exhausted).toMatchObject({
			value: "gave up",
			verdict: null,
			unverified: true,
			check: { outcome: "invalid", problems: [expect.stringContaining("status must be")] },
		});
	});

	test("rlm.finish is refused outside a subagent", async () => {
		const workspace = await repository();
		const { fixture } = verdictFixture(workspace, {});
		await expect(
			fixture.call("rlm.finish", { status: "passed", summary: "s", evidence: ["a.ts:1"] }),
		).rejects.toThrow("rlm.finish is for subagents");
	});

	test("concurrent siblings: undeclared changes are listed with the overlapping tasks, never failed", async () => {
		const workspace = await repository();
		const aStarted = deferred();
		const bWrote = deferred();
		const aDone = deferred();
		const { spawn, collect } = verdictFixture(workspace, {
			a: async (finish, root) => {
				aStarted.resolve();
				await writeFile(join(root, "a.ts"), "a");
				await bWrote.promise;
				await finish({ status: "passed", summary: "Wrote a.ts", evidence: ["a.ts:1"], changed_files: ["a.ts"] });
				return "a";
			},
			b: async (finish, root) => {
				// A's run (and its starting snapshot) is under way before B writes.
				await aStarted.promise;
				await writeFile(join(root, "b.ts"), "b");
				bWrote.resolve();
				await aDone.promise;
				await finish({ status: "passed", summary: "Wrote b.ts", evidence: ["b.ts:1"], changed_files: ["b.ts"] });
				return "b";
			},
		});
		const a = await spawn("a");
		const b = await spawn("b");
		const [first] = await collect([a]);
		aDone.resolve();
		const [second] = await collect([b]);
		// A ended before B declared anything: B's file is listed for A, with B named as concurrent work.
		expect(first!.result.check).toMatchObject({ outcome: "verified", unreported: ["b.ts"], concurrent: [b] });
		// B ended after A's verdict declared a.ts, so A's file is explained and not listed for B.
		expect(second!.result.check).toMatchObject({ outcome: "verified", unreported: [], concurrent: [a] });
	});

	test("workflow nodes pass on checked verdicts and fail on failed or contradicted ones", async () => {
		const workspace = await repository();
		const { fixture } = verdictFixture(workspace, {
			build: async (finish, root) => {
				await writeFile(join(root, "built.ts"), "b");
				await finish({
					status: "passed",
					summary: "Built",
					evidence: ["npm run build: exit 0"],
					outputs: { artifact: "built.ts" },
					changed_files: ["built.ts"],
				});
				return "built";
			},
			ship: async () => "shipped",
			broken: async (finish) => {
				await finish({ status: "failed", summary: "Compiler error in parser.ts:12" });
				return "failed";
			},
			claims: async (finish) => {
				await finish({ status: "passed", summary: "Edited", evidence: ["x.ts:1"], changed_files: ["x.ts"] });
				return "claimed";
			},
			after: async () => "should not run",
		});
		const outcome = await fixture.call<Record<string, Result & { reason?: string }>>("workflows.run", {
			nodes: [
				{ id: "build", definition: "rlm-child@1", input: { prompt: "build" } },
				{
					id: "ship",
					definition: "rlm-child@1",
					input: { prompt: "ship" },
					dependsOn: ["build"],
					when: { node: "build", field: "check", equals: "verified" },
				},
				{ id: "broken", definition: "rlm-child@1", input: { prompt: "broken" } },
				{ id: "afterBroken", definition: "rlm-child@1", input: { prompt: "after" }, dependsOn: ["broken"] },
				{ id: "claims", definition: "rlm-child@1", input: { prompt: "claims" } },
				{ id: "afterClaims", definition: "rlm-child@1", input: { prompt: "after" }, dependsOn: ["claims"] },
			],
		});
		expect(outcome.build).toMatchObject({ status: "succeeded", check: { outcome: "verified" } });
		expect(outcome.ship).toMatchObject({ status: "succeeded", value: "shipped" });
		expect(outcome.broken).toMatchObject({
			status: "failed",
			error: "Subagent verdict failed: Compiler error in parser.ts:12",
		});
		expect(outcome.afterBroken).toMatchObject({ status: "skipped" });
		expect(outcome.claims).toMatchObject({ status: "failed", error: expect.stringContaining("contradicted") });
		expect(outcome.afterClaims).toMatchObject({ status: "skipped" });
	});

	test("without a workspace a verdict is recorded but its files are not checked", async () => {
		const fixture = hostFixture({
			script: async (lane, _prompt, laneContext) => {
				await fixture.host.handle(
					"rlm.finish",
					{ status: "passed", summary: "s", evidence: ["a.ts:1"], changed_files: ["a.ts"] },
					laneContext,
					{ lane },
				);
				return "done";
			},
		});
		fixtures.push(fixture);
		const { rlm_child_id } = await fixture.call<{ rlm_child_id: string }>("rlm.spawn", {
			prompt: "p",
			kwargs: { name: "p" },
		});
		const [entry] = (
			await fixture.call<{ results: Array<{ result: Result }> }>("rlm.collect", { selectors: [rlm_child_id] })
		).results;
		expect(entry!.result).toMatchObject({
			verdict: { status: "passed" },
			check: { outcome: "unchecked", reason: "no workspace to compare" },
		});
	});
});

describe("rlm.finish in the kernel", () => {
	test("forwards the verdict to the host and raises the host's rejection", async () => {
		const requests: Array<{ type: string; payload: unknown }> = [];
		const kernel = new RlmKernel({ cwd: process.cwd(), runtimePath }, async (type, payload) => {
			requests.push({ type, payload });
			if ((payload as { status?: string }).status === "bad") throw new Error("rlm.finish rejected: status");
			return { recorded: true };
		});
		try {
			expect(
				await kernel.execute(
					"await rlm.finish('passed', 'Did it', evidence=('a.py:1',), changed_files=[Path('a.py')], outputs={'n': 1})",
				),
			).toMatchObject({ status: "ok", result: "{'recorded': True}" });
			expect(requests).toEqual([
				{
					type: "rlm.finish",
					payload: {
						status: "passed",
						summary: "Did it",
						evidence: ["a.py:1"],
						outputs: { n: 1 },
						changed_files: ["a.py"],
					},
				},
			]);
			const rejected = await kernel.execute("await rlm.finish('bad', 'x')");
			expect(JSON.stringify(rejected)).toContain("rlm.finish rejected: status");
		} finally {
			await kernel.shutdown();
		}
	});
});
