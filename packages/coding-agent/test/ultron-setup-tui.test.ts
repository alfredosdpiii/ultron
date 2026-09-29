import { spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, statSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { afterEach, describe, expect, it, vi } from "vitest";
import { VirtualTerminal } from "../../tui/test/virtual-terminal.ts";
import { createProfileRuntime, runSetupOnTerminal } from "../src/cli/setup/command.ts";
import type { SetupDeps } from "../src/cli/setup/wizard.ts";
import { SettingsManager } from "../src/core/settings-manager.ts";

const ENTER = "\r";
const DOWN = "\x1b[B";
const CTRL_C = "\x03";

const dirs: string[] = [];
afterEach(() => {
	for (const dir of dirs.splice(0)) rmSync(dir, { recursive: true, force: true });
});

function screen(terminal: VirtualTerminal): string {
	return [...terminal.getScrollBuffer(), ...terminal.getViewport()].join("\n");
}

async function waitFor(terminal: VirtualTerminal, text: string, timeoutMs = 5_000): Promise<void> {
	const started = Date.now();
	for (;;) {
		await terminal.flush();
		if (terminal.getViewport().join("\n").includes(text)) return;
		if (Date.now() - started > timeoutMs) throw new Error(`Timed out waiting for "${text}":\n${screen(terminal)}`);
		await new Promise((resolveWait) => setTimeout(resolveWait, 20));
	}
}

function type(terminal: VirtualTerminal, text: string): void {
	for (const char of text) terminal.sendInput(char);
}

describe("ultron setup in a terminal", () => {
	it("walks the steps with the keyboard, masks secrets and stops on Ctrl+C", async () => {
		const root = mkdtempSync(join(tmpdir(), "ultron-setup-tui-"));
		dirs.push(root);
		const agentDir = join(root, "agent");
		mkdirSync(agentDir);
		const settings = SettingsManager.create(root, agentDir, { projectTrusted: false });
		const testModel = vi.fn<SetupDeps["testModel"]>(async () => ({ ok: true, reply: "ok", ms: 300 }));
		const deps: SetupDeps = {
			agentDir,
			env: {},
			nodeVersion: "22.19.0",
			probe: (command) => (command === "python3" ? "Python 3.12.0" : undefined),
			fetch: (async (url: string | URL | Request) => {
				if (String(url).endsWith("/models"))
					return new Response(JSON.stringify({ data: [{ id: "m1" }, { id: "m2" }] }));
				throw new TypeError("fetch failed", { cause: new Error("connect ECONNREFUSED 127.0.0.1:8888") });
			}) as typeof fetch,
			settings,
			createRuntime: () => createProfileRuntime(agentDir),
			testModel,
			checkJevKey: async () => ({ ok: true }),
			runDocker: async () => ({ ok: false, output: "unexpected" }),
			hindsightWaitMs: 10,
		};
		const terminal = new VirtualTerminal(100, 40);
		const run = runSetupOnTerminal(settings, deps, terminal);

		await waitFor(terminal, "How should Ultron reach a model?");
		expect(screen(terminal)).toContain("✓ python3: Python 3.12.0");
		terminal.sendInput(DOWN);
		terminal.sendInput(DOWN);
		terminal.sendInput(ENTER);

		await waitFor(terminal, "Base URL of the endpoint");
		terminal.sendInput(ENTER); // the suggested http://localhost:11434/v1
		await waitFor(terminal, "API key (leave empty");
		type(terminal, "sk-tui-secret");
		await waitFor(terminal, "•••••••••••••");
		expect(screen(terminal)).not.toContain("sk-tui-secret");
		terminal.sendInput(ENTER);
		await waitFor(terminal, "A short name for this provider");
		terminal.sendInput(ENTER); // "custom"
		await waitFor(terminal, "Which API does it speak?");
		terminal.sendInput(ENTER);
		await waitFor(terminal, "Model ids to use");
		expect(screen(terminal)).toContain("The endpoint lists 2 models: m1, m2");
		terminal.sendInput(ENTER);
		await waitFor(terminal, "Enter at least one model id"); // validation keeps the question open
		type(terminal, "m1");
		terminal.sendInput(ENTER);
		await waitFor(terminal, "Default model (1 from custom)");
		terminal.sendInput(ENTER);

		await waitFor(terminal, "Step 3/6: Jev API key");
		expect(screen(terminal)).toContain("✓ Live test passed in 0.3s");
		terminal.sendInput(ENTER); // Paste a key
		await waitFor(terminal, "Input is hidden");
		type(terminal, "jev-tui-key");
		terminal.sendInput(ENTER);
		await waitFor(terminal, "Check the key now?");
		terminal.sendInput(DOWN);
		terminal.sendInput(ENTER); // No

		await waitFor(terminal, "Set up Hindsight");
		expect(screen(terminal)).toContain("Hindsight is not reachable at http://localhost:8888 (connect ECONNREFUSED");
		terminal.sendInput(CTRL_C);
		const result = await run;
		await terminal.flush();

		expect(result.completed).toBe(false);
		expect(screen(terminal)).toContain("Setup stopped");
		for (const secret of ["sk-tui-secret", "jev-tui-key"]) expect(screen(terminal)).not.toContain(secret);
		expect(JSON.parse(readFileSync(join(agentDir, "models.json"), "utf8")).providers.custom.apiKey).toBe(
			"sk-tui-secret",
		);
		expect(readFileSync(join(agentDir, "jev-api-key"), "utf8")).toBe("jev-tui-key\n");
		expect(statSync(join(agentDir, "jev-api-key")).mode & 0o777).toBe(0o600);
		expect(JSON.parse(readFileSync(join(agentDir, "settings.json"), "utf8"))).toMatchObject({
			defaultProvider: "custom",
			defaultModel: "m1",
		});
		expect(testModel).toHaveBeenCalledTimes(1);
	});
});

describe("ultron setup without a terminal", () => {
	it("refuses to prompt and explains where configuration goes", () => {
		const root = mkdtempSync(join(tmpdir(), "ultron-setup-cli-"));
		dirs.push(root);
		const child = spawnSync(process.execPath, [resolve(__dirname, "../src/cli.ts"), "setup"], {
			cwd: root,
			env: {
				...process.env,
				NODE_OPTIONS: `--import ${resolve(__dirname, "../src/experimental/source-resolver.ts")}`,
				ULTRON_CODING_AGENT_DIR: join(root, "agent"),
				PI_OFFLINE: "1",
			},
			stdio: ["pipe", "pipe", "pipe"],
			input: "",
			encoding: "utf8",
			timeout: 60_000,
		});
		expect(child.status).toBe(1);
		expect(child.stderr).toContain("ultron setup is interactive and needs a terminal");
		expect(child.stdout).toBe("");
	});
});
