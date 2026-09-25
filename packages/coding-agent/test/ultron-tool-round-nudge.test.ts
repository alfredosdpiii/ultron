import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { describe, expect, test } from "vitest";
import { RpcClient } from "../src/modes/rpc/rpc-client.ts";
import { ToolRoundNudger, toolRoundsNudgeFromEnv } from "../src/ultron/tool-round-nudge.ts";
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
});
