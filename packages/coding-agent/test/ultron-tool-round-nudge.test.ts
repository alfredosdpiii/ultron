import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { describe, expect, test } from "vitest";
import { RpcClient } from "../src/modes/rpc/rpc-client.ts";
import {
	SkillExtractionNudger,
	ToolRoundNudger,
	toolRoundsNudgeFromEnv,
	waitNudgeMessage,
} from "../src/ultron/tool-round-nudge.ts";
import { ScriptedProvider, scriptedModelsJson } from "./support/scripted-provider.ts";
import { tempServerDir } from "./support/server-dir.ts";

describe("tool round nudge", () => {
	test("steers once at the threshold, again at twice it, and resets when the model answers", () => {
		const steered: string[] = [];
		const nudger = new ToolRoundNudger(3, async (message) => steered.push(message));
		for (let round = 0; round < 7; round += 1) nudger.turnEnded("run", 2);
		expect(steered).toHaveLength(2);
		expect(steered[0]).toContain("3 rounds");
		expect(steered[1]).toContain("Stop calling tools");
		nudger.turnEnded("run", 0);
		nudger.turnEnded("run", 1);
		nudger.turnEnded("run", 1);
		expect(steered).toHaveLength(2);
		expect(toolRoundsNudgeFromEnv(undefined)).toBe(10);
		expect(toolRoundsNudgeFromEnv("0")).toBe(0);
		expect(toolRoundsNudgeFromEnv("junk")).toBe(10);
	});

	test("with subagents running, busy rounds are steered to wait instead of checking on them", () => {
		const steered: string[] = [];
		const nudger = new ToolRoundNudger(10, async (message) => steered.push(message));
		// The spawn cell and two cells of checking while six children run.
		for (let round = 0; round < 3; round += 1) nudger.turnEnded("run", 1, 6);
		expect(steered).toEqual([waitNudgeMessage(6)]);
		expect(steered[0]).toContain("6 subagents or tasks you started are still running");
		expect(steered[0]).toContain("`child_done`");
		expect(steered[0]).toContain("do not check on them through their files, logs or progress");
		expect(steered[0]).toContain("`await rlm.collect(handles)`, or end your turn");
		// Once per running phase; at the brake threshold the wait wording replaces "answer now".
		for (let round = 3; round < 10; round += 1) nudger.turnEnded("run", 1, 2);
		expect(steered).toHaveLength(2);
		expect(steered[1]).toBe(waitNudgeMessage(2));
		expect(steered.join("\n")).not.toContain("answer now");
		// The last child ended: reconciling its results starts a fresh streak, so no brake at round 10 or 20.
		for (let round = 0; round < 9; round += 1) nudger.turnEnded("run", 1, 0);
		expect(steered).toHaveLength(2);
		nudger.turnEnded("run", 1, 0);
		expect(steered[2]).toContain("10 rounds of tool calls");
		// A new spawn phase may be nudged again.
		nudger.runEnded("run");
		for (let round = 0; round < 3; round += 1) nudger.turnEnded("run", 1, 1);
		expect(steered[3]).toContain("1 subagent or task you started is still running");
		// A turn without tools (waiting ended the turn) breaks the waiting streak.
		const quiet: string[] = [];
		const patient = new ToolRoundNudger(10, async (message) => quiet.push(message), { asyncEvents: false });
		patient.turnEnded("r", 1, 3);
		patient.turnEnded("r", 0, 3);
		patient.turnEnded("r", 1, 3);
		patient.turnEnded("r", 1, 3);
		expect(quiet).toEqual([]);
		patient.turnEnded("r", 1, 3);
		// Without completion events the only way to wait is rlm.collect.
		expect(quiet).toEqual([waitNudgeMessage(3, false)]);
		expect(quiet[0]).toContain("`await rlm.collect(handles)`");
		expect(quiet[0]).not.toContain("child_done");
		expect(quiet[0]).not.toContain("end your turn");
	});

	test("the skill hint is suppressed while subagents run", () => {
		const steered: string[] = [];
		const nudger = new SkillExtractionNudger(3, async (message) => steered.push(message));
		for (let round = 0; round < 8; round += 1) nudger.turnEnded("run", 1, 0, 4);
		expect(steered).toEqual([]);
		// Rounds after the children ended count from zero.
		nudger.turnEnded("run", 1, 0, 0);
		nudger.turnEnded("run", 1, 0, 0);
		expect(steered).toEqual([]);
		nudger.turnEnded("run", 1, 0, 0);
		expect(steered).toHaveLength(1);
		expect(steered[0]).toContain("save it as a tested skill");
	});

	test("a model that keeps calling tools is steered to answer in the real CLI", async () => {
		const root = mkdtempSync(join(tmpdir(), "ultron-nudge-"));
		const agentDir = join(root, "agent");
		const projectDir = join(root, "project");
		mkdirSync(agentDir, { recursive: true });
		mkdirSync(projectDir, { recursive: true });
		// Keeps researching forever unless it sees the nudge.
		const provider = new ScriptedProvider((request) =>
			request.raw.includes("[Ultron] You have used")
				? { text: "ANSWERED AFTER NUDGE" }
				: { tool: "bash", args: { command: `echo round ${request.turn}` } },
		);
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
				ULTRON_SERVER_DIR: tempServerDir("u-nudge-"),
				ULTRON_TOOL_ROUNDS_NUDGE: "3",
				// The script calls Pi's native bash tool, which only the opt-out makes active.
				ULTRON_TOOLS: "native",
			},
		});
		try {
			await client.start();
			await client.promptAndWait("research something open-ended", undefined, 120_000);
			expect(await client.getLastAssistantText()).toBe("ANSWERED AFTER NUDGE");
			// Three tool rounds, then the steered answer; no runaway.
			expect(provider.requests.length).toBeLessThanOrEqual(5);
		} finally {
			await client.stop();
			await provider.stop();
			rmSync(root, { recursive: true, force: true });
		}
	}, 180_000);

	test("a root checking on a running subagent is steered to wait for it in the real CLI", async () => {
		const root = mkdtempSync(join(tmpdir(), "ultron-wait-nudge-"));
		const agentDir = join(root, "agent");
		const projectDir = join(root, "project");
		mkdirSync(agentDir, { recursive: true });
		mkdirSync(projectDir, { recursive: true });
		// The root spawns a slow child, then keeps checking on it until it is told to wait.
		const provider = new ScriptedProvider((request) => {
			if (request.firstUser.startsWith("CHILD:")) return { text: "child fixed it", delayMs: 6000 };
			if (request.lastToolResult?.startsWith("COLLECTED")) return { text: `DONE ${request.lastToolResult}` };
			if (request.raw.includes("still running; each result comes to you"))
				return {
					tool: "rlm",
					args: { code: 'r = await rlm.collect([h])\nprint("COLLECTED", r[0]["result"]["status"])' },
				};
			if (request.turn === 0)
				return { tool: "rlm", args: { code: 'h = await rlm.spawn("CHILD: fix the service", name="fixer")' } };
			return { tool: "rlm", args: { code: `print("checking ${request.turn}")` } };
		});
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
				ULTRON_SERVER_DIR: tempServerDir("u-wait-nudge-"),
				ULTRON_HINDSIGHT_URL: "off",
				ULTRON_SKILL_NUDGE: "2",
				PI_OFFLINE: "1",
			},
		});
		try {
			await client.start();
			await client.promptAndWait("PARENT: delegate the fix", undefined, 120_000);
			expect(await client.getLastAssistantText()).toBe("DONE COLLECTED succeeded");
			const rootRequests = provider.requests.filter((request) => !request.firstUser.startsWith("CHILD:"));
			// Spawn, two checks, the steer, collect, answer: the wait steer came after the third busy round.
			expect(rootRequests.length).toBe(5);
			const steered = rootRequests[3]!.raw;
			expect(steered).toContain("1 subagent or task you started is still running");
			// The skill hint (threshold 2 here) never fired while the child ran.
			expect(rootRequests.map((request) => request.raw).join("\n")).not.toContain("save it as a tested skill");
		} finally {
			await client.stop();
			await provider.stop();
			rmSync(root, { recursive: true, force: true });
		}
	}, 180_000);
});
