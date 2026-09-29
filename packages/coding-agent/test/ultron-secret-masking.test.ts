/**
 * Secrets in RLM cell output are masked before the model sees them (output-secrets.ts):
 * - a file the cell reads and prints, a `bash` result, stdout and a traceback show `[REDACTED:<kind>]` in place of a
 *   credential and are otherwise unchanged; runtime event summaries are masked too;
 * - values inside the kernel keep the real content: a programmatic edit of text from `read` keeps the secret;
 * - `edit(old_str=...)` with a marker copied from masked output fails with a clear message and leaves the file alone;
 * - ULTRON_MASK_SECRETS=off shows output unmasked.
 *
 * Fake credentials are built from parts at run time, so this file never holds a whole one.
 */
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { NodeExecutionEnv } from "@ultron/agent-core/node";
import { BACKGROUND_CONTEXT } from "@ultron/chord/context";
import { afterEach, beforeEach, describe, expect, test, vi } from "vitest";
import { createLocalBashOperations } from "../src/core/tools/bash.ts";
import { createUltronRlmTool, type UltronRlmTool } from "../src/experimental/session-worker.ts";
import { runtimeEventText } from "../src/ultron/async-events.ts";
import { runHostBash } from "../src/ultron/rlm/host-bash.ts";

const OPENAI = ["s", "k-proj-", "FAKE0a1B2c3D4e5F6g7H8i9J0k1L2m3N4o5P6q7R8s9T0u"].join("");
const GITHUB = ["g", "hp_", "FAKE0a1B2c3D4e5F6g7H8i9J0k1L2m3N4o5P"].join("");
const CONFIG = `DEBUG = False\nOPENAI_API_KEY = "${OPENAI}"\nNAME = "demo"\n`;

describe("secret masking in cell output", () => {
	let dir: string;
	let tool: UltronRlmTool;
	let call = 0;
	const run = async (code: string): Promise<string> => {
		call += 1;
		const invocation = {
			invocationId: `mask-${call}`,
			operationId: `mask-op-${call}`,
			turnId: "mask-turn",
			getMemo: async () => undefined,
			setMemo: async () => undefined,
		};
		try {
			const result = (await tool.execute(
				`mask-${call}`,
				{ code },
				() => {},
				{ env: new NodeExecutionEnv({ cwd: dir }) },
				invocation,
				BACKGROUND_CONTEXT,
			)) as { content: Array<{ text: string }> };
			return result.content.map((part) => part.text).join("");
		} catch (error) {
			return `ERROR ${error instanceof Error ? error.message : String(error)}`;
		}
	};

	beforeEach(() => {
		dir = mkdtempSync(join(tmpdir(), "ultron-mask-"));
		writeFileSync(join(dir, "config.py"), CONFIG);
		tool = createUltronRlmTool(
			dir,
			async (type, payload, signal) => {
				if (type === "bash") return runHostBash(payload, dir, createLocalBashOperations({}), signal);
				throw new Error(`unexpected host request ${type}`);
			},
			async () => "main",
		);
	});

	afterEach(async () => {
		await tool.close();
		rmSync(dir, { recursive: true, force: true });
	});

	test("read, bash, stdout and tracebacks are masked; the rest is intact", async () => {
		const read = await run("print(await read('config.py'))");
		expect(read).toBe('DEBUG = False\nOPENAI_API_KEY = "[REDACTED:openai_key]"\nNAME = "demo"\n');
		const bash = await run("await bash('cat config.py')");
		expect(bash).toContain('OPENAI_API_KEY = "[REDACTED:openai_key]"');
		expect(bash).not.toContain(OPENAI);
		// The last expression's value is shown masked as well.
		const value = await run(`"${GITHUB[0]}" + "${GITHUB.slice(1)}"`);
		expect(value).toBe("'[REDACTED:github_token]'");
		const failure = await run(`raise ValueError("bad token " + "${GITHUB.slice(0, 3)}" + "${GITHUB.slice(3)}")`);
		expect(failure).toContain("ValueError: bad token [REDACTED:github_token]");
		expect(failure).not.toContain(GITHUB);
	});

	test("values in the kernel keep the real text: a programmatic edit keeps the secret", async () => {
		await run(
			"from pathlib import Path\ntext = await read('config.py')\nPath('config.py').write_text(text.replace('DEBUG = False', 'DEBUG = True'))",
		);
		expect(readFileSync(join(dir, "config.py"), "utf8")).toBe(CONFIG.replace("DEBUG = False", "DEBUG = True"));
	});

	test("edit with a marker copied from masked output fails clearly and leaves the file alone", async () => {
		const failure = await run(
			`await edit('config.py', 'OPENAI_API_KEY = "[REDACTED:openai_key]"', 'OPENAI_API_KEY = os.environ["OPENAI_API_KEY"]')`,
		);
		expect(failure).toContain(
			"old_str contains [REDACTED:openai_key], a mask that cell output shows in place of a secret",
		);
		expect(failure).toContain("holds the real value, unchanged");
		expect(readFileSync(join(dir, "config.py"), "utf8")).toBe(CONFIG);
		// Editing around the secret works, and the secret survives.
		expect(await run("await edit('config.py', 'NAME = \"demo\"', 'NAME = \"live\"')")).toBe("'Edited config.py'");
		expect(readFileSync(join(dir, "config.py"), "utf8")).toBe(CONFIG.replace('"demo"', '"live"'));
		// A file that really contains a marker can still be edited through it.
		writeFileSync(join(dir, "notes.txt"), "token = [REDACTED:github_token]\n");
		expect(await run("await edit('notes.txt', '[REDACTED:github_token]', 'none')")).toBe("'Edited notes.txt'");
	});

	test("ULTRON_MASK_SECRETS=off shows the output as it is", async () => {
		vi.stubEnv("ULTRON_MASK_SECRETS", "off");
		expect(await run("print(await read('config.py'))")).toContain(OPENAI);
	});
});

describe("secret masking in runtime events", () => {
	test("a job summary is masked", () => {
		const text = runtimeEventText([
			{ kind: "job_done", id: "job-1", status: "completed", summary: `exit 0; pushed with ${GITHUB}`, fetch: "x" },
		] as never);
		expect(text).toContain("pushed with [REDACTED:github_token]");
		expect(text).not.toContain(GITHUB);
	});
});
