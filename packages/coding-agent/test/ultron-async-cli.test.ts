/**
 * Asynchronous execution in the real CLI (scripted provider, RPC mode):
 * - a plain `bash` still running after ULTRON_BASH_YIELD_AFTER returns running, with a `[hint:job-detached]` line,
 *   and its completion event re-invokes the model;
 * - the model starts a slow job with `yield_after=0` and ends its turn; the job's completion arrives as a
 *   `<runtime_event>` custom message that re-invokes the model, which fetches the result;
 * - a completion during a running turn is placed at the next turn boundary, without a second run;
 * - Esc on the turn stops the job and nothing restarts the model;
 * - the provider prompt-cache prefix: every root request starts with the previous request's messages, byte for byte,
 *   when no compaction or collapse happens (events are only ever appended).
 */
import { spawn } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { describe, expect, test } from "vitest";
import { RpcClient } from "../src/modes/rpc/rpc-client.ts";
import {
	ScriptedProvider,
	type ScriptedReply,
	type ScriptedRequest,
	scriptedModelsJson,
} from "./support/scripted-provider.ts";
import { tempServerDir } from "./support/server-dir.ts";

const EVENT =
	/<runtime_event kind="job_done" id="(job-[0-9a-f]+)" status="([a-z_]+)" summary="([^"]*)" fetch="([^"]*)" \/>/;

type Flow = {
	client: RpcClient;
	provider: ScriptedProvider;
	projectDir: string;
	agentEnds: () => number;
	agentStarts: () => number;
	close: () => Promise<void>;
};

async function startFlow(
	prefix: string,
	script: (request: ScriptedRequest) => ScriptedReply,
	env: Record<string, string> = {},
): Promise<Flow> {
	const root = mkdtempSync(join(tmpdir(), `ultron-${prefix}-`));
	const agentDir = join(root, "agent");
	const projectDir = join(root, "project");
	mkdirSync(agentDir, { recursive: true });
	mkdirSync(projectDir, { recursive: true });
	const provider = new ScriptedProvider(script);
	await provider.start();
	writeFileSync(join(agentDir, "models.json"), scriptedModelsJson(provider.baseUrl));
	const client = new RpcClient({
		cliPath: resolve(__dirname, "../src/cli.ts"),
		cwd: projectDir,
		provider: "scripted",
		model: "scripted",
		args: ["--no-session"],
		env: {
			NODE_OPTIONS: `--import ${resolve(__dirname, "../src/experimental/source-resolver.ts")}`,
			ULTRON_CODING_AGENT_DIR: agentDir,
			ULTRON_SERVER_DIR: tempServerDir(`u-${prefix}-`),
			ULTRON_HINDSIGHT_URL: "off",
			ULTRON_TOOL_ROUNDS_NUDGE: "0",
			ULTRON_SKILL_NUDGE: "0",
			PI_OFFLINE: "1",
			...env,
		},
	});
	let ends = 0;
	let starts = 0;
	client.onEvent((event) => {
		if (event.type === "agent_end") ends += 1;
		if (event.type === "agent_start") starts += 1;
	});
	await client.start();
	return {
		client,
		provider,
		projectDir,
		agentEnds: () => ends,
		agentStarts: () => starts,
		close: async () => {
			await client.stop().catch(() => {});
			await provider.stop();
			rmSync(root, { recursive: true, force: true });
		},
	};
}

const PROMPT = "ASYNC: run the slow test suite, and tell me when it passes";

/** Starts a 5-second job with yield_after=0, answers at once, and acts on the completion event when it arrives. */
function jobScript(request: ScriptedRequest): ScriptedReply {
	if (request.firstUser !== PROMPT) return { text: "unexpected" };
	const event = EVENT.exec(request.lastUser);
	if (event && request.lastToolResult === undefined)
		return {
			tool: "rlm",
			args: { code: `j = await rlm.job("${event[1]}")\nprint("RESULT", j.exit_code, j.text.strip())` },
		};
	if (request.lastToolResult?.startsWith("RESULT")) return { text: `TESTS PASSED: ${request.lastToolResult}` };
	if (request.turn === 0)
		return {
			tool: "rlm",
			args: {
				code: "job = await bash('''sleep 5; touch job.done; echo '5 passed in 5.0s' ''', yield_after=0)\nprint('STARTED', job.running)",
			},
		};
	if (request.lastUser === "SECOND: thanks") return { text: "YOU ARE WELCOME" };
	return { text: "STARTED THE TESTS; I will report when they finish." };
}

describe("asynchronous execution in the real CLI", () => {
	test("a slow job started with yield_after=0 does not block the turn; its completion event re-invokes the model", async () => {
		const flow = await startFlow("async-job", jobScript);
		try {
			await flow.client.promptAndWait(PROMPT, undefined, 60_000);
			// The turn ended while the 5-second job was still running: its completion marker does not exist yet.
			expect(existsSync(join(flow.projectDir, "job.done"))).toBe(false);
			expect(await flow.client.getLastAssistantText()).toContain("STARTED THE TESTS");
			expect(flow.provider.requests[0]!.lastToolResult).toBeUndefined();
			expect(flow.provider.requests[1]!.lastToolResult).toContain("STARTED True");
			// The completion starts exactly one more run, and the model fetches the full result.
			await expect.poll(flow.agentEnds, { timeout: 30_000, interval: 100 }).toBe(2);
			expect(await flow.client.getLastAssistantText()).toBe("TESTS PASSED: RESULT 0 5 passed in 5.0s");
			const eventRequests = flow.provider.requests.filter((request) => EVENT.test(request.lastUser));
			expect(eventRequests.length).toBeGreaterThanOrEqual(1);
			const [, id, status, summary, fetch] = EVENT.exec(eventRequests[0]!.lastUser)!;
			expect([status, summary]).toEqual(["completed", "exit 0; 5 passed in 5.0s"]);
			expect(fetch).toBe(`await rlm.job(&quot;${id}&quot;)`);
			// RPC clients see the event as a normal Pi custom message in the transcript.
			const messages = await flow.client.getMessages();
			expect(messages).toContainEqual(
				expect.objectContaining({ role: "custom", customType: "ultron-runtime-event", display: true }),
			);
			// Nothing loops: no further runs after the model answered the event.
			await new Promise((resolveWait) => setTimeout(resolveWait, 1500));
			expect(flow.agentStarts()).toBe(2);
			expect(flow.provider.requests).toHaveLength(4);
		} finally {
			await flow.close();
		}
	}, 120_000);

	test("a plain bash that runs past ULTRON_BASH_YIELD_AFTER detaches with a hint; its completion event re-invokes the model", async () => {
		const DETACH = "DETACH: run the slow tests";
		const flow = await startFlow(
			"async-detach",
			(request) => {
				if (request.firstUser !== DETACH) return { text: "unexpected" };
				const event = EVENT.exec(request.lastUser);
				if (event && request.lastToolResult === undefined)
					return {
						tool: "rlm",
						args: { code: `j = await rlm.job("${event[1]}")\nprint("RESULT", j.exit_code, j.text.strip())` },
					};
				if (request.lastToolResult?.startsWith("RESULT")) return { text: `DONE: ${request.lastToolResult}` };
				if (request.turn === 0)
					return {
						tool: "rlm",
						args: {
							code: "out = await bash('''sleep 4; touch job.done; echo '3 passed' ''')\nprint('RUNNING', out.running, out.exit_code)\nout",
						},
					};
				return { text: "THE TESTS ARE RUNNING; I will report when they finish." };
			},
			// Situational hints are off by default; this row checks the job-detached hint.
			{ ULTRON_BASH_YIELD_AFTER: "1", ULTRON_HINTS: "on" },
		);
		try {
			await flow.client.promptAndWait(DETACH, undefined, 60_000);
			expect(existsSync(join(flow.projectDir, "job.done"))).toBe(false);
			expect(await flow.client.getLastAssistantText()).toContain("THE TESTS ARE RUNNING");
			const cell = flow.provider.requests[1]!.lastToolResult!;
			expect(cell).toMatch(/^RUNNING True None\n/);
			expect(cell).toMatch(
				/\[still running as job job-[0-9a-f]+ after 1 s; its completion will arrive as a runtime event/,
			);
			// The runtime's one-line hint ends the result.
			expect(cell.split("\n").at(-1)).toMatch(
				/^\[hint:job-detached\] The command was still running after 1 s, so it continues as job job-[0-9a-f]+\. Do not wait for it/,
			);
			await expect.poll(flow.agentEnds, { timeout: 30_000, interval: 100 }).toBe(2);
			expect(await flow.client.getLastAssistantText()).toBe("DONE: RESULT 0 3 passed");
			const event = flow.provider.requests.map((request) => EVENT.exec(request.lastUser)).find(Boolean)!;
			expect([event[2], event[3]]).toEqual(["completed", "exit 0; 3 passed"]);
			expect(flow.agentStarts()).toBe(2);
			expect(flow.provider.requests).toHaveLength(4);
		} finally {
			await flow.close();
		}
	}, 120_000);

	test("print mode (-p) follows the run a completion event starts and prints its answer", async () => {
		const root = mkdtempSync(join(tmpdir(), "ultron-async-print-"));
		const agentDir = join(root, "agent");
		const projectDir = join(root, "project");
		mkdirSync(agentDir, { recursive: true });
		mkdirSync(projectDir, { recursive: true });
		const provider = new ScriptedProvider(jobScript);
		await provider.start();
		writeFileSync(join(agentDir, "models.json"), scriptedModelsJson(provider.baseUrl));
		try {
			const child = spawn(
				"node",
				[
					resolve(__dirname, "../src/cli.ts"),
					"--provider",
					"scripted",
					"--model",
					"scripted",
					"--no-session",
					"-p",
					PROMPT,
				],
				{
					cwd: projectDir,
					env: {
						...process.env,
						NODE_OPTIONS: `--import ${resolve(__dirname, "../src/experimental/source-resolver.ts")}`,
						ULTRON_CODING_AGENT_DIR: agentDir,
						ULTRON_SERVER_DIR: tempServerDir("u-async-print-"),
						ULTRON_HINDSIGHT_URL: "off",
						PI_OFFLINE: "1",
					},
					stdio: ["ignore", "pipe", "pipe"],
				},
			);
			let stdout = "";
			child.stdout!.on("data", (data: Buffer) => {
				stdout += data.toString();
			});
			const code = await new Promise<number | null>((resolveExit) => child.once("exit", resolveExit));
			expect(code).toBe(0);
			expect(stdout.trim()).toBe("TESTS PASSED: RESULT 0 5 passed in 5.0s");
			expect(provider.requests).toHaveLength(4);
		} finally {
			await provider.stop();
			rmSync(root, { recursive: true, force: true });
		}
	}, 120_000);

	test("prompt-cache prefix: each root request extends the previous one byte for byte, events included", async () => {
		const flow = await startFlow("async-cache", jobScript);
		try {
			await flow.client.promptAndWait(PROMPT, undefined, 60_000);
			await expect.poll(flow.agentEnds, { timeout: 30_000, interval: 100 }).toBe(2);
			await flow.client.promptAndWait("SECOND: thanks", undefined, 60_000);
			expect(await flow.client.getLastAssistantText()).toBe("YOU ARE WELCOME");
			const requests = flow.provider.requests;
			// Two tool rounds, the event run's two requests, and the follow-up prompt.
			expect(requests).toHaveLength(5);
			const bodies = requests.map((request) => JSON.parse(request.raw) as { messages: unknown[]; tools?: unknown });
			for (let index = 1; index < bodies.length; index += 1) {
				const previous = bodies[index - 1]!;
				const current = bodies[index]!;
				expect(JSON.stringify(current.tools)).toBe(JSON.stringify(previous.tools));
				expect(current.messages.length).toBeGreaterThan(previous.messages.length);
				const prefix = current.messages
					.slice(0, previous.messages.length)
					.map((message) => JSON.stringify(message));
				expect(prefix).toEqual(previous.messages.map((message) => JSON.stringify(message)));
			}
			// The serialized request itself shares the prefix up to the end of the previous request's messages.
			const lastPrevious = JSON.stringify(bodies[3]!.messages.at(-1));
			expect(requests[4]!.raw.indexOf(lastPrevious)).toBeGreaterThan(0);
			expect(requests[4]!.raw.slice(0, requests[4]!.raw.indexOf(lastPrevious))).toBe(
				requests[3]!.raw.slice(0, requests[3]!.raw.indexOf(lastPrevious)),
			);
		} finally {
			await flow.close();
		}
	}, 120_000);

	test("collapse on return rewrites only the newest cell output, so the cached prefix before it survives", async () => {
		const ROOT = "COLLAPSE: invoke a child and continue";
		const flow = await startFlow("async-collapse", (request) => {
			if (request.firstUser.startsWith("CHILD")) return { text: "V".repeat(2000) };
			if (request.firstUser !== ROOT) return { text: "unexpected" };
			if (request.turn === 0)
				return {
					tool: "rlm",
					args: { code: 'r = await agents.invoke("rlm-child@1", {"prompt": "CHILD big"})\nprint(r)' },
				};
			if (request.turn === 1) return { tool: "rlm", args: { code: "print('second cell')" } };
			if (request.turn === 2) return { tool: "rlm", args: { code: "print('third cell')" } };
			return { text: "DONE" };
		});
		try {
			await flow.client.promptAndWait(ROOT, undefined, 60_000);
			const bodies = flow.provider.requests
				.filter((request) => request.firstUser === ROOT)
				.map((request) =>
					(JSON.parse(request.raw) as { messages: unknown[] }).messages.map((m) => JSON.stringify(m)),
				);
			expect(bodies).toHaveLength(4);
			// The cell output that returned the task is message 3 (system, user, assistant call, tool result).
			const target = 3;
			expect(bodies[1]![target]).toContain("V".repeat(1000));
			const collapsedAt = bodies.findIndex((body) => body[target]?.includes("collapsed on return"));
			expect(collapsedAt).toBeGreaterThan(1);
			for (let index = 1; index < bodies.length; index += 1) {
				const previous = bodies[index - 1]!;
				const current = bodies[index]!;
				if (index === collapsedAt) {
					// The one rewrite: everything before the collapsed output is the same, and the output was among the
					// previous request's newest messages (the edit lands at the first boundary after the model's answer).
					expect(current.slice(0, target)).toEqual(previous.slice(0, target));
					expect(previous.length - target).toBeLessThanOrEqual(3);
				} else expect(current.slice(0, previous.length)).toEqual(previous);
			}
		} finally {
			await flow.close();
		}
	}, 120_000);

	test("a completion during a running turn is delivered at the next boundary, without a second run", async () => {
		const flow = await startFlow("async-steer", (request) => {
			const event = EVENT.exec(request.lastUser);
			if (event) return { text: `SAW ${event[2]} ${event[3]}` };
			if (request.turn === 0)
				return { tool: "rlm", args: { code: "job = await bash('''sleep 0.5; echo quick-done''', yield_after=0)" } };
			if (request.turn === 1)
				return { tool: "rlm", args: { code: "await asyncio.sleep(3)\nprint('other work done')" } };
			return { text: "NO EVENT SEEN" };
		});
		try {
			await flow.client.promptAndWait("STEER: start a job, then do other work", undefined, 60_000);
			expect(await flow.client.getLastAssistantText()).toBe("SAW completed exit 0; quick-done");
			// The event followed the second cell's result in the same run.
			const last = flow.provider.requests.at(-1)!.body.messages;
			expect(last.at(-2)).toMatchObject({ role: "tool" });
			await new Promise((resolveWait) => setTimeout(resolveWait, 1500));
			expect(flow.agentStarts()).toBe(1);
			expect(flow.provider.requests).toHaveLength(3);
		} finally {
			await flow.close();
		}
	}, 120_000);

	test("a subagent's completion arrives as a child_done event that re-invokes the idle root", async () => {
		const CHILD_EVENT =
			/<runtime_event kind="child_done" id="([^"]+)" status="succeeded" summary="([^"]*)" fetch="([^"]*)" \/>/;
		const flow = await startFlow("async-child", (request) => {
			if (request.firstUser.startsWith("CHILD:")) return { text: "child found 3 files", delayMs: 1500 };
			const event = CHILD_EVENT.exec(request.lastUser);
			if (event) return { text: `ROOT GOT ${event[2]}` };
			if (request.turn === 0)
				return { tool: "rlm", args: { code: 'h = await rlm.spawn("CHILD: count the files", name="counter")' } };
			return { text: "DELEGATED" };
		});
		try {
			await flow.client.promptAndWait("PARENT: delegate the count", undefined, 60_000);
			expect(await flow.client.getLastAssistantText()).toBe("DELEGATED");
			await expect.poll(flow.agentEnds, { timeout: 30_000, interval: 100 }).toBe(2);
			// The event summary leads with the verdict tag: this child ended without rlm.finish.
			expect(await flow.client.getLastAssistantText()).toBe(
				"ROOT GOT rlm-child@1: [unverified] child found 3 files",
			);
			const event = flow.provider.requests.map((request) => CHILD_EVENT.exec(request.lastUser)).find(Boolean)!;
			expect(event[3]).toBe(`await rlm.collect([&quot;${event[1]}&quot;])`);
		} finally {
			await flow.close();
		}
	}, 120_000);

	test("a spent turn budget turns the re-invocation into quiet delivery with the next prompt", async () => {
		const flow = await startFlow(
			"async-budget",
			(request) => {
				if (EVENT.test(request.lastUser)) return { text: "SHOULD NOT BE RE-INVOKED" };
				if (request.lastUser === "NEXT: anything new?")
					return { text: request.raw.includes("exit 0; budget-done") ? "EVENT WAITED FOR ME" : "NO EVENT" };
				if (request.turn === 0)
					return {
						tool: "rlm",
						args: { code: "job = await bash('''sleep 1; echo budget-done''', yield_after=0)" },
					};
				return { text: "STARTED" };
			},
			{ ULTRON_MAX_TOTAL_TURNS: "2" },
		);
		try {
			await flow.client.promptAndWait("BUDGET: start a job", undefined, 60_000);
			expect(await flow.client.getLastAssistantText()).toBe("STARTED");
			await new Promise((resolveWait) => setTimeout(resolveWait, 3000));
			// Two model turns spent the root's budget of two: the completion does not start a run.
			expect(flow.agentStarts()).toBe(1);
			expect(flow.provider.requests).toHaveLength(2);
			await flow.client.promptAndWait("NEXT: anything new?", undefined, 60_000);
			expect(await flow.client.getLastAssistantText()).toBe("EVENT WAITED FOR ME");
		} finally {
			await flow.close();
		}
	}, 120_000);

	test("Esc on the turn stops its job and the completion never restarts the model", async () => {
		const flow = await startFlow("async-abort", (request) => {
			if (request.turn === 0)
				return {
					tool: "rlm",
					args: { code: "job = await bash('''echo $$ > job.pid; exec sleep 30''', yield_after=0.5)" },
				};
			if (request.turn === 1) return { tool: "rlm", args: { code: "await asyncio.sleep(30)" } };
			return { text: "SHOULD NOT RUN" };
		});
		try {
			await flow.client.prompt("ABORT: start a long job then wait");
			const pidFile = join(flow.projectDir, "job.pid");
			await expect.poll(() => flow.provider.requests.length, { timeout: 30_000, interval: 100 }).toBe(2);
			await expect.poll(() => existsSync(pidFile), { timeout: 10_000 }).toBe(true);
			const pid = Number(readFileSync(pidFile, "utf8").trim());
			expect(alive(pid)).toBe(true);
			await new Promise((resolveWait) => setTimeout(resolveWait, 500));
			await flow.client.abort();
			await expect.poll(flow.agentEnds, { timeout: 30_000, interval: 100 }).toBe(1);
			await expect.poll(() => alive(pid), { timeout: 10_000, interval: 100 }).toBe(false);
			await new Promise((resolveWait) => setTimeout(resolveWait, 2000));
			expect(flow.agentStarts()).toBe(1);
			expect(flow.provider.requests).toHaveLength(2);
		} finally {
			await flow.close();
		}
	}, 120_000);
});

function alive(pid: number): boolean {
	try {
		process.kill(pid, 0);
		return true;
	} catch {
		return false;
	}
}
