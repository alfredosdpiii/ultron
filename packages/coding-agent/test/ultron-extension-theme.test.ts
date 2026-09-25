import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { expect, test } from "vitest";
import { RpcClient } from "../src/modes/rpc/rpc-client.ts";
import { ScriptedProvider, scriptedModelsJson } from "./support/scripted-provider.ts";
import { tempServerDir } from "./support/server-dir.ts";

test("extension tools that use the theme run in the session worker", async () => {
	const root = mkdtempSync(join(tmpdir(), "ultron-ext-theme-"));
	const agentDir = join(root, "agent");
	const projectDir = join(root, "project");
	mkdirSync(join(agentDir, "extensions"), { recursive: true });
	mkdirSync(projectDir, { recursive: true });
	// Like pi-mcp-adapter: the tool formats its output with the theme while executing.
	writeFileSync(
		join(agentDir, "extensions", "themed.ts"),
		`export default function (pi) {
	pi.registerTool({
		name: "themed",
		label: "themed",
		description: "Returns themed text",
		parameters: { type: "object", properties: {} },
		async execute(_id, _params, _signal, _onUpdate, ctx) {
			return { content: [{ type: "text", text: "themed-ok " + ctx.ui.theme.fg("accent", "x") }], details: {} };
		},
	});
}
`,
	);
	const provider = new ScriptedProvider((request) =>
		request.turn === 0 ? { tool: "themed", args: {} } : { text: request.lastToolResult ?? "no result" },
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
			ULTRON_SERVER_DIR: tempServerDir("u-theme-"),
		},
	});
	try {
		await client.start();
		await client.promptAndWait("use the themed tool", undefined, 120_000);
		const answer = (await client.getLastAssistantText()) ?? "";
		expect(answer).toContain("themed-ok");
		expect(answer).not.toContain("Theme not initialized");
	} finally {
		await client.stop();
		await provider.stop();
		rmSync(root, { recursive: true, force: true });
	}
}, 180_000);
