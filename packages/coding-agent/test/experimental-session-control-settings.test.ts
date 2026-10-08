import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { AgentHarness, AgentLane, ThinkingLevel } from "@ultron/agent-core";
import type { ApiKeyCredential, Model, Provider } from "@ultron/ai";
import { BACKGROUND_CONTEXT } from "@ultron/chord/context";
import { afterEach, describe, expect, test, vi } from "vitest";
import { CONFIG_DIR_NAME } from "../src/config.ts";
import { AuthStorage } from "../src/core/auth-storage.ts";
import { ModelRuntime } from "../src/core/model-runtime.ts";
import { SettingsManager } from "../src/core/settings-manager.ts";
import { createSessionControl, debugEnvironment } from "../src/experimental/services/session-control-provider.ts";
import { settingsFragment, workerProjectTrusted } from "../src/experimental/services/worker-settings.ts";
import { nativeSessionDetails } from "../src/experimental/session-listing.ts";

const directories: string[] = [];

function temporaryDirectory(prefix: string): string {
	const directory = mkdtempSync(join(tmpdir(), prefix));
	directories.push(directory);
	return directory;
}

afterEach(() => {
	for (const directory of directories.splice(0)) rmSync(directory, { recursive: true, force: true });
	vi.unstubAllEnvs();
});

/** A harness and lane that record what the worker applies to the running Session. */
function fakeSession() {
	const state = {
		compaction: { enabled: true, reserveTokens: 16384, keepRecentTokens: 20000 },
		retry: { enabled: true, maxRetries: 3, baseDelayMs: 1000, maxAgentDelayMs: 60000 },
		stream: { timeoutMs: 300_000 } as Record<string, unknown>,
		steeringMode: "all" as "all" | "one-at-a-time",
		followUpMode: "all" as "all" | "one-at-a-time",
		thinkingLevel: "off" as ThinkingLevel,
	};
	const harness = {
		getCompactionSettings: async () => ({ ...state.compaction }),
		setCompactionSettings: vi.fn(async (next: typeof state.compaction) => {
			state.compaction = next;
		}),
		getRetryPolicy: async () => ({ ...state.retry }),
		setRetryPolicy: vi.fn(async (next: typeof state.retry) => {
			state.retry = next;
		}),
		getStreamOptions: async () => ({ ...state.stream }),
		setStreamOptions: vi.fn(async (next: Record<string, unknown>) => {
			state.stream = next;
		}),
		getSteeringMode: async () => state.steeringMode,
		setSteeringMode: vi.fn(async (mode: "all" | "one-at-a-time") => {
			state.steeringMode = mode;
		}),
		getFollowUpMode: async () => state.followUpMode,
		setFollowUpMode: vi.fn(async (mode: "all" | "one-at-a-time") => {
			state.followUpMode = mode;
		}),
	};
	const lane = {
		getModel: async () => ({ provider: "test", id: "one" }),
		getThinkingLevel: async () => state.thinkingLevel,
		setThinkingLevel: vi.fn(async (level: ThinkingLevel) => {
			state.thinkingLevel = level;
		}),
	};
	return { state, harness, lane };
}

function workerControl(options: { cwd: string; agentDir: string; settingsManager?: SettingsManager }) {
	const session = fakeSession();
	const settingsManager = options.settingsManager ?? SettingsManager.create(options.cwd, options.agentDir);
	const configureHttpIdleTimeout = vi.fn();
	const control = createSessionControl({
		harness: session.harness as unknown as AgentHarness,
		lane: session.lane as unknown as AgentLane,
		cwd: options.cwd,
		agentDir: options.agentDir,
		settingsManager,
		configureHttpIdleTimeout,
		inspect: async (request) => (request === "rlm.pool" ? { live: 1, maxLive: 16, evictions: 0 } : null),
	});
	return { control, settingsManager, configureHttpIdleTimeout, ...session };
}

function globalSettings(agentDir: string): Record<string, unknown> {
	return JSON.parse(readFileSync(join(agentDir, "settings.json"), "utf8")) as Record<string, unknown>;
}

describe("SessionControl settings in the Session worker", () => {
	test("setSetting persists through the worker's SettingsManager and applies live where Pi does", async () => {
		const cwd = temporaryDirectory("ultron-settings-cwd-");
		const agentDir = temporaryDirectory("ultron-settings-agent-");
		const { control, state, harness, lane, configureHttpIdleTimeout } = workerControl({ cwd, agentDir });

		expect(await control.setSetting("compaction.enabled", false, BACKGROUND_CONTEXT)).toEqual({ applied: "live" });
		expect(state.compaction.enabled).toBe(false);
		expect(harness.setCompactionSettings).toHaveBeenCalledOnce();
		expect(await control.setSetting("retry.enabled", false, BACKGROUND_CONTEXT)).toEqual({ applied: "live" });
		expect(state.retry.enabled).toBe(false);
		await control.setSetting("steeringMode", "one-at-a-time", BACKGROUND_CONTEXT);
		await control.setSetting("followUpMode", "one-at-a-time", BACKGROUND_CONTEXT);
		expect([state.steeringMode, state.followUpMode]).toEqual(["one-at-a-time", "one-at-a-time"]);
		await control.setSetting("transport", "sse", BACKGROUND_CONTEXT);
		expect(state.stream.transport).toBe("sse");
		await control.setSetting("httpIdleTimeoutMs", 60_000, BACKGROUND_CONTEXT);
		expect(configureHttpIdleTimeout).toHaveBeenCalledWith(60_000);
		expect(state.stream.timeoutMs).toBe(60_000);
		// A per-model thinking level for the current model changes the lane now; for another model it is only saved.
		expect(
			await control.setSetting(
				"modelThinkingLevel",
				{ provider: "test", modelId: "one", level: "high" },
				BACKGROUND_CONTEXT,
			),
		).toEqual({ applied: "live" });
		expect(state.thinkingLevel).toBe("high");
		expect(
			await control.setSetting(
				"modelThinkingLevel",
				{ provider: "test", modelId: "other", level: "low" },
				BACKGROUND_CONTEXT,
			),
		).toEqual({ applied: "saved" });
		expect(lane.setThinkingLevel).toHaveBeenCalledOnce();
		// Presentation settings are saved for the client; restart-only ones say so.
		expect(await control.setSetting("hideThinkingBlock", true, BACKGROUND_CONTEXT)).toEqual({ applied: "saved" });
		// Engineering mode changes the system prompt the worker renders for the next request.
		expect(await control.setSetting("engineering", true, BACKGROUND_CONTEXT)).toEqual({ applied: "live" });
		await expect(control.setSetting("engineering", "yes", BACKGROUND_CONTEXT)).rejects.toThrow(
			"engineering must be true or false",
		);
		expect(await control.setSetting("terminal.clearOnShrink", true, BACKGROUND_CONTEXT)).toEqual({
			applied: "saved",
		});
		expect(await control.setSetting("defaultProjectTrust", "never", BACKGROUND_CONTEXT)).toEqual({
			applied: "restart",
		});
		await control.setSetting("enabledModels", ["test/one", "test/two"], BACKGROUND_CONTEXT);

		expect(globalSettings(agentDir)).toMatchObject({
			compaction: { enabled: false },
			retry: { enabled: false },
			engineering: true,
			steeringMode: "one-at-a-time",
			followUpMode: "one-at-a-time",
			transport: "sse",
			httpIdleTimeoutMs: 60_000,
			modelThinkingLevels: { "test/one": "high", "test/other": "low" },
			hideThinkingBlock: true,
			terminal: { clearOnShrink: true },
			defaultProjectTrust: "never",
			enabledModels: ["test/one", "test/two"],
		});
		// A fresh manager over the same profile (the next worker) sees every change.
		const reread = await control.readSettings(BACKGROUND_CONTEXT);
		expect(reread.values).toMatchObject({
			"compaction.enabled": false,
			hideThinkingBlock: true,
			"terminal.clearOnShrink": true,
			enabledModels: ["test/one", "test/two"],
		});
		expect(SettingsManager.create(cwd, agentDir).getHideThinkingBlock()).toBe(true);

		// Removing the current model's override falls back to the default thinking level.
		await control.setSetting(
			"modelThinkingLevel",
			{ provider: "test", modelId: "one", level: null },
			BACKGROUND_CONTEXT,
		);
		expect(state.thinkingLevel).toBe("medium");
		expect(globalSettings(agentDir).modelThinkingLevels).toEqual({ "test/other": "low" });
	});

	test("setSetting rejects unknown keys and invalid values without writing", async () => {
		const cwd = temporaryDirectory("ultron-settings-cwd-");
		const agentDir = temporaryDirectory("ultron-settings-agent-");
		const { control } = workerControl({ cwd, agentDir });
		await expect(control.setSetting("apiKey", "x", BACKGROUND_CONTEXT)).rejects.toThrow("Unknown setting: apiKey");
		await expect(control.setSetting("steeringMode", "sometimes", BACKGROUND_CONTEXT)).rejects.toThrow(
			"steeringMode must be one of",
		);
		await expect(control.setSetting("editorPaddingX", -1, BACKGROUND_CONTEXT)).rejects.toThrow("at least 0");
		expect(() => readFileSync(join(agentDir, "settings.json"))).toThrow();
	});

	test("readSettings reports the worker's values, cwd, profile and trust", async () => {
		const cwd = temporaryDirectory("ultron-settings-cwd-");
		const agentDir = temporaryDirectory("ultron-settings-agent-");
		writeFileSync(join(agentDir, "settings.json"), JSON.stringify({ theme: "light", treeFilterMode: "user-only" }));
		const { control } = workerControl({ cwd, agentDir });
		const read = await control.readSettings(BACKGROUND_CONTEXT);
		expect(read).toMatchObject({ cwd, agentDir, projectTrusted: true, savedTrust: null });
		expect(read.values).toMatchObject({
			theme: "light",
			treeFilterMode: "user-only",
			"compaction.enabled": true,
			steeringMode: "all",
		});

		await control.setProjectTrust([{ path: cwd, decision: false }], BACKGROUND_CONTEXT);
		const saved = (await control.readSettings(BACKGROUND_CONTEXT)).savedTrust;
		expect(saved).toEqual({ path: expect.stringContaining("ultron-settings-cwd-"), decision: false });
		expect(JSON.parse(readFileSync(join(agentDir, "trust.json"), "utf8"))).toEqual({ [saved!.path]: false });
	});

	test("a worker loads project settings only for a trusted project", () => {
		const cwd = temporaryDirectory("ultron-trust-cwd-");
		const agentDir = temporaryDirectory("ultron-trust-agent-");
		// Without trust-requiring project resources a project is trusted, as in Pi.
		expect(workerProjectTrusted(cwd, agentDir)).toBe(true);
		mkdirSync(join(cwd, CONFIG_DIR_NAME), { recursive: true });
		writeFileSync(join(cwd, CONFIG_DIR_NAME, "settings.json"), JSON.stringify({ theme: "light" }));
		// "ask" cannot prompt in a worker: trusted, as before /trust existed.
		expect(workerProjectTrusted(cwd, agentDir)).toBe(true);
		writeFileSync(join(agentDir, "trust.json"), JSON.stringify({ [cwd]: false }));
		expect(workerProjectTrusted(cwd, agentDir)).toBe(false);
		writeFileSync(join(agentDir, "trust.json"), JSON.stringify({}));
		writeFileSync(join(agentDir, "settings.json"), JSON.stringify({ defaultProjectTrust: "never" }));
		expect(workerProjectTrusted(cwd, agentDir)).toBe(false);
	});

	test("settingsFragment maps keys to the Settings shape the client overlays", () => {
		expect(settingsFragment("hideThinkingBlock", true)).toEqual({ hideThinkingBlock: true });
		expect(settingsFragment("terminal.clearOnShrink", true)).toEqual({ terminal: { clearOnShrink: true } });
		expect(settingsFragment("modelThinkingLevel", { provider: "a", modelId: "b", level: "low" })).toBeUndefined();
	});
});

function apiKeyProvider(id: string): Provider<"openai-completions"> {
	const model: Model<"openai-completions"> = {
		id: "fake-model",
		name: "Fake model",
		api: "openai-completions",
		provider: id,
		baseUrl: "https://example.invalid/v1",
		reasoning: false,
		input: ["text"],
		cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
		contextWindow: 1000,
		maxTokens: 100,
	};
	return {
		id,
		name: id,
		auth: {
			apiKey: {
				name: "API key",
				login: async (): Promise<ApiKeyCredential> => ({ type: "api_key", key: `${id}-key` }),
				check: async ({ credential }) => (credential ? { type: "api_key", source: "stored" } : undefined),
				resolve: async ({ credential }) =>
					credential ? { auth: { apiKey: credential.key }, source: "stored" } : undefined,
			},
		},
		getModels: () => [model],
		stream: () => {
			throw new Error("unused");
		},
		streamSimple: () => {
			throw new Error("unused");
		},
	};
}

describe("SessionControl auth reload and debug info", () => {
	test("reloadAuth makes credentials a client saved to auth.json available to the worker", async () => {
		const agentDir = temporaryDirectory("ultron-auth-agent-");
		const authPath = join(agentDir, "auth.json");
		const worker = await ModelRuntime.create({
			credentials: AuthStorage.create(authPath),
			modelsPath: null,
			allowModelNetwork: false,
			refreshOnCreate: false,
		});
		worker.registerNativeProvider(apiKeyProvider("fake-keys"));
		await worker.refresh({ allowNetwork: false });
		const { harness, lane } = fakeSession();
		const control = createSessionControl({
			harness: harness as unknown as AgentHarness,
			lane: lane as unknown as AgentLane,
			cwd: agentDir,
			agentDir,
			modelRuntime: worker,
		});
		expect(worker.getAvailableSnapshot().some((model) => model.provider === "fake-keys")).toBe(false);

		// The client's /login: its own runtime writes the credential through AuthStorage.
		const client = await ModelRuntime.create({
			credentials: AuthStorage.create(authPath),
			modelsPath: null,
			allowModelNetwork: false,
			refreshOnCreate: false,
		});
		client.registerNativeProvider(apiKeyProvider("fake-keys"));
		await client.login("fake-keys", "api_key", { prompt: async () => "unused", notify: () => {} });
		expect(JSON.parse(readFileSync(authPath, "utf8"))).toMatchObject({ "fake-keys": { type: "api_key" } });

		expect(await control.reloadAuth("fake-keys", BACKGROUND_CONTEXT)).toEqual({
			availableModels: expect.any(Number),
		});
		expect(worker.getAvailableSnapshot().some((model) => model.provider === "fake-keys")).toBe(true);

		// The client's /logout, then a reload drops the provider again.
		await client.logout("fake-keys");
		await control.reloadAuth(null, BACKGROUND_CONTEXT);
		expect(worker.getAvailableSnapshot().some((model) => model.provider === "fake-keys")).toBe(false);
	});

	test("debugInfo reports the worker process, kernel pool and redacted environment", async () => {
		const agentDir = temporaryDirectory("ultron-debug-agent-");
		vi.stubEnv("ULTRON_TOOLS", "rlm");
		vi.stubEnv("ULTRON_HINDSIGHT_API_KEY", "super-secret");
		vi.stubEnv("PI_AUTH_TOKEN", "also-secret");
		const { control } = workerControl({ cwd: agentDir, agentDir, settingsManager: SettingsManager.inMemory() });
		const info = await control.debugInfo(BACKGROUND_CONTEXT);
		expect(info).toMatchObject({
			pid: process.pid,
			cwd: agentDir,
			agentDir,
			model: "test/one",
			kernelPool: { live: 1, maxLive: 16 },
		});
		expect(info.environment.ULTRON_TOOLS).toBe("rlm");
		expect(info.environment.ULTRON_HINDSIGHT_API_KEY).toBe("<redacted>");
		expect(info.environment.PI_AUTH_TOKEN).toBe("<redacted>");
		expect(JSON.stringify(info)).not.toContain("secret");
		expect(debugEnvironment({ HOME: "/home/x", ULTRON_X: "1" })).toEqual({ ULTRON_X: "1" });
	});
});

describe("native Session listing for /resume", () => {
	test("reads the name, first user message and message count from a Session file", () => {
		const text = [
			JSON.stringify({ v: 4, kind: "header", id: "s1", createdAt: 1, cwd: "/p" }),
			JSON.stringify([
				{
					kind: "entry",
					id: "u1",
					type: "message",
					message: { role: "user", content: [{ type: "text", text: "fix the parser" }] },
				},
				{ kind: "value", op: "set", namespace: "pi.session.name", key: "", value: "Parser" },
			]),
			JSON.stringify({ kind: "entry", id: "a1", type: "message", message: { role: "assistant", content: [] } }),
			'{"kind":"entry","id":"torn',
		].join("\n");
		expect(nativeSessionDetails(text)).toEqual({ name: "Parser", firstMessage: "fix the parser", messageCount: 2 });
		expect(
			nativeSessionDetails(
				`${text}\n${JSON.stringify({ kind: "value", op: "delete", namespace: "pi.session.name", key: "" })}`,
			).name,
		).toBeNull();
	});
});
