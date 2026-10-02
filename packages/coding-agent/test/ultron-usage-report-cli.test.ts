/**
 * The session report end to end: the real CLI runs a scripted session with frames, nested subagents, a worktree
 * child and a failing cell; then `/usage`'s inspection request and the offline `ultron usage` reader must agree on
 * what the session did.
 */
import { execFileSync } from "node:child_process";
import { mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { describe, expect, test } from "vitest";
import { RpcClient } from "../src/modes/rpc/rpc-client.ts";
import { readSessionLog } from "../src/ultron/session-log.ts";
import { buildSessionReport, SESSION_REPORT_SCHEMA, type SessionReport } from "../src/ultron/session-report.ts";
import { runUsageCommand } from "../src/ultron/usage-cli.ts";
import {
	ScriptedProvider,
	type ScriptedReply,
	type ScriptedRequest,
	scriptedModelsJson,
} from "./support/scripted-provider.ts";
import { tempServerDir } from "./support/server-dir.ts";

const ROOT_CELLS = [
	[
		'a = await rlm.spawn("CHILD-A: delegate once", name="alpha", depth=1)',
		'b = await rlm.spawn("CHILD-B: just answer", name="beta")',
		'w = await rlm.spawn("CHILD-W: write a file", name="writer", worktree=True)',
		"done = await rlm.collect([a, b, w])",
		'labels = await rlm.map("FRAME: label this", ["x", "y", "z"], contract=str)',
		'number = await rlm.infer("FRAME: the number", context=["forty-two"], contract={"type": "integer"})',
		"merged = await rlm.merge([w])",
		'print("ROOT", len(done), len(labels), number, [m["status"] for m in merged["results"]])',
	].join("\n"),
	'raise ValueError("boom")',
	'out = await bash("echo hello")\nprint(out)',
];

function script(request: ScriptedRequest): ScriptedReply {
	if (request.system.startsWith("You are an inference frame"))
		return { text: request.raw.includes("the number") ? "42" : '"label"' };
	const first = request.firstUser;
	if (first.includes("GRANDCHILD")) return { text: "gamma done" };
	if (first.includes("CHILD-A")) {
		if (request.turn === 0)
			return {
				tool: "rlm",
				args: {
					code: 'g = await rlm.spawn("GRANDCHILD: answer", name="gamma")\nprint(await rlm.collect([g]))',
				},
			};
		if (request.turn === 1)
			return {
				tool: "rlm",
				args: { code: 'await rlm.finish("passed", "delegated to gamma", evidence=["collect exited with code 0"])' },
			};
		return { text: "alpha done" };
	}
	if (first.includes("CHILD-B")) return { text: "beta done" };
	if (first.includes("CHILD-W")) {
		if (request.turn === 0)
			return {
				tool: "rlm",
				args: {
					code: 'print(await write("made.txt", "made by the worktree child\\n"))\nawait rlm.finish("passed", "wrote made.txt", evidence=["made.txt: 1 line"], changed_files=["made.txt"])',
				},
			};
		return { text: "writer done" };
	}
	if (first.startsWith("ROOT")) {
		const code = ROOT_CELLS[request.turn];
		return code === undefined ? { text: "ROOT DONE" } : { tool: "rlm", args: { code } };
	}
	throw new Error(`Unscripted request: ${first.slice(0, 80)}`);
}

function git(cwd: string, ...args: string[]): void {
	execFileSync("git", args, {
		cwd,
		stdio: "ignore",
		env: {
			...process.env,
			GIT_AUTHOR_NAME: "t",
			GIT_AUTHOR_EMAIL: "t@example.invalid",
			GIT_COMMITTER_NAME: "t",
			GIT_COMMITTER_EMAIL: "t@example.invalid",
		},
	});
}

function sessionFiles(root: string): string[] {
	const files: string[] = [];
	for (const directory of readdirSync(root, { withFileTypes: true })) {
		if (!directory.isDirectory()) continue;
		for (const name of readdirSync(join(root, directory.name)))
			if (name.endsWith(".jsonl")) files.push(join(root, directory.name, name));
	}
	return files;
}

describe("session report of a real session", () => {
	test("frames, nested subagents, a worktree child and a failed cell are reported live and offline", async () => {
		const root = mkdtempSync(join(tmpdir(), "ultron-usage-report-"));
		const agentDir = join(root, "agent");
		const projectDir = join(root, "project");
		mkdirSync(agentDir, { recursive: true });
		mkdirSync(projectDir, { recursive: true });
		writeFileSync(join(projectDir, "README.md"), "demo\n");
		git(projectDir, "init", "-q", "-b", "main");
		git(projectDir, "add", ".");
		git(projectDir, "commit", "-q", "-m", "init");
		const provider = new ScriptedProvider(script);
		await provider.start();
		writeFileSync(join(agentDir, "models.json"), scriptedModelsJson(provider.baseUrl));
		const client = new RpcClient({
			cliPath: resolve(__dirname, "../src/cli.ts"),
			cwd: projectDir,
			provider: "scripted",
			model: "scripted",
			env: {
				NODE_OPTIONS: `--import ${resolve(__dirname, "../src/experimental/source-resolver.ts")}`,
				ULTRON_CODING_AGENT_DIR: agentDir,
				ULTRON_SERVER_DIR: tempServerDir("u-usage-"),
				ULTRON_HINDSIGHT_URL: "http://127.0.0.1:9",
				ULTRON_ASYNC_EVENTS: "off",
				ULTRON_LOKI: "off",
				GIT_AUTHOR_NAME: "t",
				GIT_AUTHOR_EMAIL: "t@example.invalid",
				GIT_COMMITTER_NAME: "t",
				GIT_COMMITTER_EMAIL: "t@example.invalid",
				PI_OFFLINE: "1",
			},
		});
		let live: SessionReport;
		try {
			await client.start();
			await client.promptAndWait("ROOT: go", undefined, 150_000);
			const response = (await (
				client as unknown as {
					send(command: object): Promise<{ success: boolean; data?: unknown; error?: string }>;
				}
			).send({ type: "inspect", request: "usage.report", payload: {} })) as {
				success: boolean;
				data?: unknown;
				error?: string;
			};
			expect(response.error).toBeUndefined();
			live = response.data as SessionReport;
		} finally {
			await client.stop().catch(() => {});
			await provider.stop();
		}
		try {
			if (process.env.USAGE_DEBUG) console.log(JSON.stringify(live, null, 1));
			const sessionsRoot = join(agentDir, "experimental", "sessions");
			const files = sessionFiles(sessionsRoot);
			expect(files).toHaveLength(1);
			const offline = buildSessionReport(await readSessionLog(files[0]!));
			if (process.env.USAGE_DEBUG) console.log(readFileSync(files[0]!, "utf8").length);

			for (const report of [live, offline]) {
				expect(report.schema).toBe(SESSION_REPORT_SCHEMA);
				expect(report.mode).toBe("ultron");
				expect(report.turns).toMatchObject({ count: 1, completed: 1, running: 0 });
				expect(report.root.models).toEqual([{ model: "scripted/scripted", responses: 4 }]);
				// Three root cells (one failed), two in alpha and one in the worktree child.
				expect(report.cells?.source).toBe("transcript");
				expect(report.cells?.root).toMatchObject({ count: 3, failed: 1 });
				expect(report.cells?.root.apis).toMatchObject({
					bash: 1,
					"rlm.spawn": 1,
					"rlm.map": 1,
					"rlm.infer": 1,
					"rlm.collect": 1,
					"rlm.merge": 1,
				});
				expect(report.cells?.subagents).toMatchObject({ count: 3, failed: 0 });
				expect(report.cells?.total.count).toBe(6);
				// Depth: four subagents, one of them spawned by alpha; four frames from one map and one infer.
				expect(report.depth.verdict).toBe("depth 2: 4 frames, 4 sub-agents (1 nested)");
				expect(report.depth.level).toBe(2);
				expect(report.depth.frames).toMatchObject({ count: 4, complete: 4, incomplete: 0, failed: 0, nested: 0 });
				expect(report.depth.frames.calls).toEqual({ infer: 1, map: 1 });
				expect(report.depth.frames.byModel).toMatchObject([{ model: "scripted/scripted", count: 4 }]);
				expect(report.depth.subagents).toMatchObject({ count: 4, maxDepth: 2, nested: 1, completed: 4 });
				expect(report.depth.subagents.verdicts).toMatchObject({
					verified: 2,
					contradicted: 0,
					unverified: 2,
					none: 2,
				});
				expect(report.depth.subagents.claims).toEqual({ passed: 2, failed: 0, blocked: 0 });
				expect(report.depth.subagents.byModel).toMatchObject([
					{ model: "scripted/scripted", count: 4, unmeasured: 0 },
				]);
				expect(report.depth.subagents.worktrees).toHaveLength(1);
				expect(report.depth.subagents.worktrees[0]).toMatchObject({ changedFiles: 1, merge: "merged" });
				expect(report.depth.subagents.worktrees[0]!.branch).toMatch(/^ultron\//);
				expect(report.depth.workflows).toEqual({ runs: 0 });
				// Every scripted response is in exactly one lane kind.
				const { lanes, total, models } = report.usage;
				expect(lanes.root.responses).toBe(4);
				expect(lanes.frames.responses).toBe(4);
				expect(lanes.subagents.responses).toBe(3 + 1 + 2 + 1);
				expect(total.responses).toBe(provider.requests.length);
				expect(total.totalTokens).toBe(
					lanes.root.totalTokens + lanes.frames.totalTokens + lanes.subagents.totalTokens,
				);
				expect(total.totalTokens).toBeGreaterThan(0);
				expect(models).toHaveLength(1);
				// The scripted model has no price: its cost is unknown, never $0.
				expect(total.cost).toEqual({
					reportedUsd: null,
					subscriptionUsd: null,
					unpricedResponses: total.responses,
				});
				expect(report.guardrails.usageLimitBlocks).toBe(0);
				expect(report.guardrails.secretsMasked).toBe(0);
				expect(report.guardrails.countersSince).toBeNull();
			}
			// The file holds everything the worker reported.
			expect(offline.depth).toEqual(live.depth);
			expect(offline.usage).toEqual(live.usage);
			expect(offline.cells).toEqual(live.cells);

			// `ultron usage` finds the session by directory and by id prefix, and prints the same report.
			const run = async (args: string[]): Promise<string[]> => {
				const lines: string[] = [];
				const handled = await runUsageCommand(
					["usage", ...args],
					{ stdout: (line) => lines.push(line), stderr: (line) => lines.push(`ERR ${line}`) },
					{ cwd: projectDir, env: {}, agentDir },
				);
				expect(handled).toBe(true);
				return lines;
			};
			const text = await run([]);
			expect(text.join("\n")).toContain("depth 2: 4 frames, 4 sub-agents (1 nested)");
			expect(text.join("\n")).toContain("verdicts: 2 verified · 0 contradicted · 2 unverified");
			expect(JSON.parse((await run([offline.session.id.slice(0, 13), "--json"])).join("\n"))).toEqual(offline);
			const table = await run(["--last", "5"]);
			expect(table[0]).toMatch(
				/^LAST ACTIVE\s+ID\s+CWD\s+MODE\s+MODEL\s+TURNS\s+CELLS\s+FRAMES\s+SUBS\s+TOKENS\s+COST\s+DEPTH$/,
			);
			expect(table[1]).toMatch(
				/scripted\/scripted\s+1\s+6\s+4\s+4\s+\S+\s+unknown\s+depth 2: 4 frames, 4 sub-agents \(1 nested\)$/,
			);
		} finally {
			rmSync(root, { recursive: true, force: true });
		}
	}, 240_000);
});
