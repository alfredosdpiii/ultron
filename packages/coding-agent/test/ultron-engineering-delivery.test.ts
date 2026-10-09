/**
 * Engineering mode end to end: the always-on skills (`alwaysSkills`) reach the request the root model actually
 * receives while the mode is on, leave it when `/engineering false` is applied, and come back when it is turned on
 * again, in a running session, from the next request. Covered for the native root (the scripted provider records
 * each request) and the `--claude` root (the fake `claude` records the system prompt file it was given).
 */
import { chmodSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { BACKGROUND_CONTEXT, JsonlSessionRepo, TODO_CONTEXT } from "@ultron/agent-core";
import { NodeExecutionEnv } from "@ultron/agent-core/node";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import { ENV_AGENT_DIR } from "../src/config.ts";
import { applyWorkerSetting } from "../src/experimental/services/worker-settings.ts";
import { createUltronRuntime, type UltronRuntime } from "../src/experimental/session-worker.ts";
import { ScriptedProvider, scriptedModelsJson } from "./support/scripted-provider.ts";

const here = dirname(fileURLToPath(import.meta.url));
const fakeClaude = resolve(here, "fixtures/fake-claude-agent.mjs");
const cliPath = resolve(here, "../src/cli.ts");
const sourceResolverPath = resolve(here, "../src/experimental/source-resolver.ts");

/** A line no other prompt text contains, so finding it proves the skill's own text arrived. */
const MARKER = "CANARY-ALWAYS-ON-7f3a: end every answer with the word ponytail.";

describe("engineering mode reaches the root model", () => {
	let work: string;
	let project: string;
	let agentDir: string;
	let sessionDir: string;
	let logDir: string;
	const saved = new Map<string, string | undefined>();
	const runtimes: Array<{ runtime: UltronRuntime; repo: JsonlSessionRepo; env: NodeExecutionEnv }> = [];
	const set = (name: string, value: string | undefined) => {
		if (!saved.has(name)) saved.set(name, process.env[name]);
		if (value === undefined) delete process.env[name];
		else process.env[name] = value;
	};

	beforeAll(() => {
		work = mkdtempSync(join(tmpdir(), "ultron-engineering-"));
		project = join(work, "project");
		agentDir = join(work, "agent");
		sessionDir = join(work, "sessions");
		logDir = join(work, "fake-claude");
		for (const dir of [project, sessionDir, logDir, join(agentDir, "skills", "canary")])
			mkdirSync(dir, { recursive: true });
		writeFileSync(
			join(agentDir, "skills", "canary", "SKILL.md"),
			`---\nname: canary\ndescription: A canary skill for the engineering mode delivery test.\n---\n\n${MARKER}\n`,
		);
		writeFileSync(
			join(agentDir, "settings.json"),
			JSON.stringify({ alwaysSkills: ["canary"], engineering: true, hindsightUrl: "off" }),
		);
		chmodSync(fakeClaude, 0o755);
		set(ENV_AGENT_DIR, agentDir);
		set("XDG_RUNTIME_DIR", join(work, "run"));
		mkdirSync(join(work, "run"), { recursive: true });
		set("ULTRON_CLAUDE_CODE_BIN", fakeClaude);
		set("FAKE_CLAUDE_LOG", logDir);
		set("ULTRON_SELF_COMMAND", JSON.stringify([process.execPath, "--import", sourceResolverPath, cliPath]));
		set("ULTRON_BUNDLED_SKILLS", "off");
		set("CLAUDECODE", undefined);
	});

	afterAll(async () => {
		for (const { runtime, repo, env } of runtimes.splice(0)) {
			await runtime.closeRlm?.().catch(() => {});
			await runtime.harness.close(TODO_CONTEXT).catch(() => {});
			await repo.close(TODO_CONTEXT).catch(() => {});
			await env.cleanup(TODO_CONTEXT).catch(() => {});
		}
		for (const [name, value] of saved) {
			if (value === undefined) delete process.env[name];
			else process.env[name] = value;
		}
		rmSync(work, { recursive: true, force: true, maxRetries: 10, retryDelay: 200 });
	}, 30_000);

	async function open(provider: string, model: string): Promise<UltronRuntime> {
		const env = new NodeExecutionEnv({ cwd: project });
		const repo = new JsonlSessionRepo({ fileSystem: env, sessionsRoot: sessionDir });
		const session = await repo.create({ cwd: project }, TODO_CONTEXT);
		const { id, createdAt, storageVersion, cwd, path, modifiedAt } = session.metadata;
		const runtime = await createUltronRuntime(
			session,
			{
				sessionDir,
				metadata: { id, createdAt, storageVersion, cwd, path, modifiedAt },
				provider,
				model,
				extensionMode: "print",
				pluginManifestPaths: [],
			},
			env,
		);
		runtimes.push({ runtime, repo, env });
		return runtime;
	}

	/** `/engineering true|false` as the TUI applies it: the worker's setting, live, from the next request. */
	const engineering = (runtime: UltronRuntime, on: boolean) =>
		applyWorkerSetting(
			{ harness: runtime.harness, lane: runtime.lane!, settingsManager: runtime.settingsManager! } as never,
			"engineering",
			on,
			BACKGROUND_CONTEXT,
		);

	test("native root: each request carries the skill only while the mode is on, toggled in a running session", async () => {
		const provider = new ScriptedProvider(() => ({ text: "ok" }));
		await provider.start();
		try {
			writeFileSync(join(agentDir, "models.json"), scriptedModelsJson(provider.baseUrl));
			set("ULTRON_ROOT", undefined);
			const runtime = await open("scripted", "scripted");
			const lane = runtime.lane!;
			const ask = async (prompt: string) => {
				expect(await lane.prompt(prompt, undefined, BACKGROUND_CONTEXT)).toMatchObject({ ok: true });
				return provider.requests.at(-1)!.system;
			};

			const on = await ask("first");
			expect(on).toContain("Always-on skills: the user set these to apply to every task.");
			expect(on).toContain(MARKER);
			expect(on).not.toContain("<name>canary</name>");

			expect(await engineering(runtime, false)).toEqual({ applied: "live" });
			const off = await ask("second");
			expect(off).not.toContain(MARKER);
			expect(off).not.toContain("Always-on skills");
			// Off, the skill is still available: listed for the model to read when a task matches.
			expect(off).toContain("<name>canary</name>");

			expect(await engineering(runtime, true)).toEqual({ applied: "live" });
			expect(await ask("third")).toContain(MARKER);
			// The mode is saved, so a new session starts with it.
			expect(JSON.parse(readFileSync(join(agentDir, "settings.json"), "utf8"))).toMatchObject({ engineering: true });
		} finally {
			await provider.stop();
		}
	}, 120_000);

	test("--claude root: the system prompt Claude Code is started with carries the skill while the mode is on", async () => {
		set("ULTRON_ROOT", "claude");
		const runtime = await open("claude-code", "claude-opus-5-5");
		const lane = runtime.lane!;
		const spawned = () =>
			readFileSync(join(logDir, "calls.jsonl"), "utf8")
				.split("\n")
				.filter(Boolean)
				.map((line) => JSON.parse(line) as { phase: string; systemPrompt: string | null })
				.filter((call) => call.phase === "spawn");

		expect(await lane.prompt("hello", undefined, BACKGROUND_CONTEXT)).toMatchObject({ ok: true });
		expect(spawned().at(-1)?.systemPrompt).toContain(MARKER);

		await engineering(runtime, false);
		expect(await lane.prompt("again", undefined, BACKGROUND_CONTEXT)).toMatchObject({ ok: true });
		const off = spawned().at(-1)?.systemPrompt ?? "";
		expect(off).not.toContain(MARKER);
		expect(off).toContain("<name>canary</name>");
		await engineering(runtime, true);
	}, 120_000);
});
