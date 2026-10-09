/**
 * Loki guardrails through the real CLI and session worker, with a scripted model and the real bundled engine
 * (python3 >= 3.11; skipped otherwise):
 * - the session creates and commits only `.loki/`, tells the model once, and adds Loki's policy note as its own
 *   `<loki>` system-prompt section;
 * - an `edit()` that would add a hardcoded credential is refused before the write (the cell raises ValueError with
 *   Loki's finding) and the file is unchanged; the clean edit lands;
 * - a `bash` command that writes source (a credential into leaked.py) is refused before it runs and the file never
 *   exists; a shell write into notes is allowed;
 * - a credential written past both checks (`Path.write_text`) is reported with the next cell's result.
 *
 * Fake credentials are built from parts inside the kernel, so neither this file nor the transcript holds a whole one.
 */
import { spawnSync } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { afterEach, beforeEach, describe, expect, test } from "vitest";
import { getBundledLokiPath } from "../src/config.ts";
import { RpcClient } from "../src/modes/rpc/rpc-client.ts";
import { findPython } from "../src/ultron/loki.ts";
import type { SessionReport } from "../src/ultron/session-report.ts";
import {
	ScriptedProvider,
	type ScriptedReply,
	type ScriptedRequest,
	scriptedModelsJson,
} from "./support/scripted-provider.ts";
import { tempServerDir } from "./support/server-dir.ts";

const IDENTITY = {
	GIT_AUTHOR_NAME: "Test User",
	GIT_AUTHOR_EMAIL: "test@example.com",
	GIT_COMMITTER_NAME: "Test User",
	GIT_COMMITTER_EMAIL: "test@example.com",
};
/** The key's parts; the kernel joins them with "-". */
const KEY_PARTS = JSON.stringify(["sk", "proj", "A1b2C3d4".repeat(6)]);

const ready = "command" in (await findPython()) && getBundledLokiPath() !== undefined;

function git(cwd: string, ...args: string[]): string {
	const result = spawnSync("git", args, { cwd, env: { ...process.env, ...IDENTITY }, encoding: "utf8" });
	if (result.status !== 0) throw new Error(`git ${args.join(" ")} failed: ${result.stderr}`);
	return result.stdout.trim();
}

/** Each prompt is one scripted exchange: rlm cells in order, then a reply quoting every tool result. */
const CELLS: Record<string, string[]> = {
	"add the key": [
		`key = "-".join(${KEY_PARTS})\nawait edit("settings.py", "API_KEY = None", f'API_KEY = "{key}"')`,
		`await edit("settings.py", "API_KEY = None", 'API_KEY = os.environ.get("API_KEY")')`,
	],
	"write through bash": [
		`key = "-".join(${KEY_PARTS})\nawait bash(f"""printf 'TOKEN = "%s"\\\\n' '{key}' > leaked.py""")\nprint("written")`,
		`print(await bash("echo hello > notes.md && cat notes.md"))`,
		`from pathlib import Path\nPath("leaked.py").write_text(f'TOKEN = "{key}"\\n')\nprint("written past the checks")`,
		"await asyncio.sleep(4)\nprint('later')",
	],
};

function text(content: unknown): string {
	if (typeof content === "string") return content;
	return Array.isArray(content)
		? content.map((part) => (typeof part?.text === "string" ? (part.text as string) : "")).join("")
		: "";
}

function script(request: ScriptedRequest): ScriptedReply {
	// The prompt is the latest user message naming an exchange (Loki's one-time note follows the first prompt).
	const messages = request.body.messages;
	let index = messages.length - 1;
	let cells: string[] | undefined;
	for (; index >= 0; index--) {
		if (messages[index]!.role !== "user") continue;
		cells = Object.entries(CELLS).find(([prompt]) => text(messages[index]!.content).includes(prompt))?.[1];
		if (cells) break;
	}
	if (!cells) return { text: "ok" };
	const done = messages.slice(index).filter((message) => message.role === "tool").length;
	if (done < cells.length) return { tool: "rlm", args: { code: cells[done]! } };
	return { text: `results:\n${request.lastToolResult ?? ""}` };
}

describe.skipIf(!ready)("Loki in a real session", () => {
	const cliPath = resolve(__dirname, "../src/cli.ts");
	const sourceResolverPath = resolve(__dirname, "../src/experimental/source-resolver.ts");
	let root: string;
	let projectDir: string;
	let provider: ScriptedProvider;
	let client: RpcClient | undefined;

	beforeEach(async () => {
		root = mkdtempSync(join(tmpdir(), "ultron-loki-worker-"));
		const agentDir = join(root, "agent");
		projectDir = join(root, "project");
		mkdirSync(agentDir, { recursive: true });
		mkdirSync(projectDir, { recursive: true });
		git(projectDir, "init", "-q", "-b", "main");
		git(projectDir, "config", "commit.gpgsign", "false");
		writeFileSync(join(projectDir, "settings.py"), "import os\n\nAPI_KEY = None\n");
		git(projectDir, "add", "settings.py");
		git(projectDir, "commit", "-qm", "init");
		provider = new ScriptedProvider(script);
		await provider.start();
		writeFileSync(join(agentDir, "models.json"), scriptedModelsJson(provider.baseUrl));
		client = new RpcClient({
			cliPath,
			cwd: projectDir,
			provider: "scripted",
			model: "scripted",
			args: ["--no-session"],
			env: {
				NODE_OPTIONS: `--import ${sourceResolverPath}`,
				ULTRON_CODING_AGENT_DIR: agentDir,
				ULTRON_HINDSIGHT_URL: "off",
				ULTRON_SERVER_DIR: tempServerDir("u-loki-"),
				PI_OFFLINE: "1",
				ULTRON_LOKI: "on",
				LOKI_DAEMON: "0",
				CI: "",
				HOME: root,
				...IDENTITY,
			},
		});
	});

	afterEach(async () => {
		await client?.stop().catch(() => {});
		await provider.stop();
		rmSync(root, { recursive: true, force: true });
	});

	test("sets up and commits .loki/, blocks a secret edit and a shell write before they land, reports one past both", async () => {
		await client!.start();
		await client!.promptAndWait("add the key to settings.py", undefined, 120_000);

		// Only .loki/ was created and committed, with the configured identity and no other change.
		expect(readdirSync(join(projectDir, ".loki")).sort()).toEqual(["loki.json", "loki.py"]);
		expect(git(projectDir, "log", "-1", "--format=%s|%an")).toBe("Add Loki guardrails|Test User");
		expect(git(projectDir, "show", "--name-only", "--format=", "HEAD").split("\n").sort()).toEqual([
			".loki/loki.json",
			".loki/loki.py",
		]);
		for (const other of [".claude", ".pi", ".github", ".ruff.toml"])
			expect(existsSync(join(projectDir, other)), other).toBe(false);

		const first = provider.requests[0]!;
		expect(first.system).toContain("<loki>\nLOKI guardrails: edit() and write() are checked");
		expect(first.raw).toMatch(/Loki set up and committed \([0-9a-f]+\)/);
		const answer = (await client!.getLastAssistantText()) ?? "";
		expect(answer).toContain("Edited settings.py");
		const blocked = provider.requests.find((request) => request.lastToolResult?.includes("was not written"));
		expect(blocked?.lastToolResult).toContain(
			"ValueError: settings.py was not written: [Loki] settings.py:3: loki/secret",
		);
		expect(readFileSync(join(projectDir, "settings.py"), "utf8")).toBe(
			'import os\n\nAPI_KEY = os.environ.get("API_KEY")\n',
		);

		await client!.promptAndWait("write through bash", undefined, 120_000);
		// The shell write into source was refused before it ran; the one into notes ran.
		const refused = provider.requests.find((request) => request.lastToolResult?.includes("was not run"));
		expect(refused?.lastToolResult).toContain("bash command was not run: [Loki] loki: `redirect` into leaked.py");
		expect(readFileSync(join(projectDir, "notes.md"), "utf8")).toBe("hello\n");
		const later = (await client!.getLastAssistantText()) ?? "";
		expect(later).toContain("later");
		expect(later).toContain("[Loki] 1 new finding from this cell's changes; fix these:");
		expect(later).toContain("leaked.py:1: loki/secret");
		// The notice is shown once.
		expect(provider.requests.at(-1)!.raw.match(/Loki set up and committed/g)).toHaveLength(1);

		// The session report (`/usage`) holds what Loki did: the counters outlive the worker's memory.
		const response = (await (
			client as unknown as { send(command: object): Promise<{ success: boolean; data?: unknown }> }
		).send({ type: "inspect", request: "usage.report", payload: {} })) as { success: boolean; data?: unknown };
		const loki = (response.data as SessionReport).guardrails.guards?.Loki;
		expect(loki).toBeDefined();
		expect(loki!.checks).toBeGreaterThanOrEqual(2);
		expect(loki!.blocked).toBe(2);
		expect(loki!.afterChecks).toBeGreaterThanOrEqual(1);
		expect(loki!.afterFindings).toBeGreaterThanOrEqual(1);
		expect(loki!.ms).toBeGreaterThan(0);
	}, 240_000);
});
