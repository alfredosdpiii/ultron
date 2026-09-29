/**
 * `/settings → Models`: the global-only model settings (`rlm.frameModel`, `rlm.childModel`, `rlm.frameThinking`,
 * `review.model`, `claudeCode.*`), their precedence against the environment, the worker's setting keys (a
 * claude-code sub-agent model is refused), the Models rows and pickers, and a running inference runtime that picks
 * up a change between two frames.
 */
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { AgentHarness, AgentLane, Context } from "@ultron/agent-core";
import { BACKGROUND_CONTEXT } from "@ultron/chord/context";
import { setKeybindings } from "@ultron/tui";
import { afterEach, beforeAll, describe, expect, test, vi } from "vitest";
import { CONFIG_DIR_NAME } from "../src/config.ts";
import { KeybindingsManager } from "../src/core/keybindings.ts";
import { SettingsManager } from "../src/core/settings-manager.ts";
import { displayAfterPick, modelSettingRows } from "../src/experimental/client-tui-models-settings.ts";
import type { ModelSummary } from "../src/experimental/services/models.ts";
import { createSessionControl } from "../src/experimental/services/session-control-provider.ts";
import { modelEnvOverrides } from "../src/experimental/services/worker-settings.ts";
import { ModelsSettingsSubmenu, modelPickerItems } from "../src/modes/interactive/components/models-settings.ts";
import { initTheme } from "../src/modes/interactive/theme/theme.ts";
import { createInferenceRuntime, createMemoryFrameStore } from "../src/ultron/rlm/inference.ts";
import {
	effectiveChildModel,
	effectiveFrameModel,
	effectiveReviewModel,
	FRAME_MODEL_ENV,
	REVIEW_MODEL_ENV,
} from "../src/ultron/rlm/model-settings.ts";
import { NativeRlmHost } from "../src/ultron/rlm/native-host.ts";
import { NativeUsageLedger } from "../src/ultron/usage.ts";
import { hostFixture, memoryDefinitionStore, memoryStore } from "./ultron-host-fixtures.ts";

const dirs: string[] = [];
function tempDir(prefix: string): string {
	const dir = mkdtempSync(join(tmpdir(), prefix));
	dirs.push(dir);
	return dir;
}

afterEach(() => {
	for (const dir of dirs.splice(0)) rmSync(dir, { recursive: true, force: true });
	vi.unstubAllEnvs();
});

const plain = (text: string) => text.replace(/\u001b\[[0-9;]*m/g, "");

describe("model settings in the settings manager", () => {
	test("rlm.* and review.model are global only, persisted, and a claude-code child model is dropped", async () => {
		const cwd = tempDir("ultron-models-cwd-");
		const agentDir = tempDir("ultron-models-agent-");
		mkdirSync(join(cwd, CONFIG_DIR_NAME), { recursive: true });
		// A project cannot route frames or sub-agents elsewhere.
		writeFileSync(
			join(cwd, CONFIG_DIR_NAME, "settings.json"),
			JSON.stringify({ rlm: { frameModel: "evil/model", childModel: "evil/model" }, review: { model: "evil/m" } }),
		);
		const settings = SettingsManager.create(cwd, agentDir, { projectTrusted: true });
		expect(settings.getRlmModelSettings()).toEqual({});
		expect(settings.getReviewModel()).toBeUndefined();

		settings.setRlmModelSetting("frameModel", "claude-code/haiku");
		settings.setRlmModelSetting("frameThinking", "low");
		settings.setReviewModel("claude-code/sonnet");
		settings.setClaudeCodeSetting("model", "sonnet");
		await settings.flush();
		const saved = JSON.parse(readFileSync(join(agentDir, "settings.json"), "utf8"));
		expect(saved).toMatchObject({
			rlm: { frameModel: "claude-code/haiku", frameThinking: "low" },
			review: { model: "claude-code/sonnet" },
			claudeCode: { model: "sonnet" },
		});
		expect(settings.getRlmModelSettings()).toEqual({ frameModel: "claude-code/haiku", frameThinking: "low" });
		expect(settings.getClaudeCodeSettings().model).toBe("sonnet");

		// Clearing removes the key; a hand-edited claude-code child model is ignored (sub-agents need tools).
		settings.setRlmModelSetting("frameModel", undefined);
		await settings.flush();
		expect(JSON.parse(readFileSync(join(agentDir, "settings.json"), "utf8")).rlm).toEqual({ frameThinking: "low" });
		const edited = SettingsManager.inMemory({ rlm: { childModel: "claude-code/haiku", frameModel: "haiku" } });
		expect(edited.getRlmModelSettings()).toEqual({});
	});

	test("precedence: env over setting over the session model; review.model over the frame model", () => {
		const settings = { frameModel: "claude-code/haiku", childModel: "openai/gpt-5" };
		expect(effectiveFrameModel({}, {})).toEqual({ source: "session" });
		expect(effectiveFrameModel({}, settings)).toEqual({ model: "claude-code/haiku", source: "setting" });
		expect(effectiveFrameModel({ [FRAME_MODEL_ENV]: "claude-code/opus" }, settings)).toEqual({
			model: "claude-code/opus",
			source: "env",
			env: FRAME_MODEL_ENV,
		});
		// A malformed variable does not count.
		expect(effectiveFrameModel({ [FRAME_MODEL_ENV]: "haiku" }, settings).source).toBe("setting");
		expect(effectiveReviewModel({}, undefined, settings)).toEqual({ model: "claude-code/haiku", source: "frame" });
		expect(effectiveReviewModel({}, "claude-code/sonnet", settings)).toEqual({
			model: "claude-code/sonnet",
			source: "setting",
		});
		expect(effectiveReviewModel({ [REVIEW_MODEL_ENV]: "x/y" }, "claude-code/sonnet", settings).source).toBe("env");
		expect(effectiveReviewModel({}, undefined, {})).toEqual({ source: "session" });
		expect(effectiveChildModel(settings)).toEqual({ model: "openai/gpt-5", source: "setting" });
		expect(effectiveChildModel({})).toEqual({ source: "session" });
	});
});

function fakeWorker(settingsManager: SettingsManager) {
	const lane = { getModel: async () => ({ provider: "test", id: "one" }), setThinkingLevel: vi.fn() };
	return createSessionControl({
		harness: {
			getCompactionSettings: async () => ({ enabled: true }),
			getRetryPolicy: async () => ({ enabled: true }),
			getSteeringMode: async () => "all",
			getFollowUpMode: async () => "all",
		} as unknown as AgentHarness,
		lane: lane as unknown as AgentLane,
		cwd: tempDir("ultron-models-worker-cwd-"),
		agentDir: tempDir("ultron-models-worker-agent-"),
		settingsManager,
		inspect: async () => null,
	});
}

describe("the worker's model setting keys", () => {
	test("frame, review, child and thinking apply live; a claude-code sub-agent model is refused", async () => {
		const settings = SettingsManager.inMemory();
		const control = fakeWorker(settings);
		await expect(control.setSetting("rlm.frameModel", "claude-code/haiku", BACKGROUND_CONTEXT)).resolves.toEqual({
			applied: "live",
		});
		await expect(control.setSetting("review.model", "claude-code/sonnet", BACKGROUND_CONTEXT)).resolves.toEqual({
			applied: "live",
		});
		await expect(control.setSetting("rlm.frameThinking", "high", BACKGROUND_CONTEXT)).resolves.toEqual({
			applied: "live",
		});
		await expect(control.setSetting("rlm.childModel", "claude-code/haiku", BACKGROUND_CONTEXT)).rejects.toThrow(
			/rlm.childModel must be a tool-capable model: claude-code\/haiku runs on the Claude Code CLI, which has no tool calling/,
		);
		await expect(
			control.setSetting("claudeCode.ultronChildModel", "claude-code/opus", BACKGROUND_CONTEXT),
		).rejects.toThrow(/tool-capable/);
		await expect(control.setSetting("rlm.frameModel", "haiku", BACKGROUND_CONTEXT)).rejects.toThrow(
			/provider\/model/,
		);
		await control.setSetting("rlm.childModel", "openai/gpt-5", BACKGROUND_CONTEXT);
		// Claude Code mode: a picked claude-code model is saved as the CLI alias; it applies at the next launch.
		await expect(control.setSetting("claudeCode.model", "claude-code/sonnet", BACKGROUND_CONTEXT)).resolves.toEqual({
			applied: "saved",
		});
		expect(settings.getRlmModelSettings()).toEqual({
			frameModel: "claude-code/haiku",
			childModel: "openai/gpt-5",
			frameThinking: "high",
		});
		expect(settings.getReviewModel()).toBe("claude-code/sonnet");
		expect(settings.getClaudeCodeSettings().model).toBe("sonnet");
		const read = await control.readSettings(BACKGROUND_CONTEXT);
		expect(read.values).toMatchObject({
			"rlm.frameModel": "claude-code/haiku",
			"rlm.childModel": "openai/gpt-5",
			"rlm.frameThinking": "high",
			"review.model": "claude-code/sonnet",
			"claudeCode.model": "sonnet",
			"claudeCode.frameModel": null,
		});
		// null clears a setting.
		await control.setSetting("rlm.frameModel", null, BACKGROUND_CONTEXT);
		expect(settings.getRlmModelSettings().frameModel).toBeUndefined();
	});

	test("environment overrides are reported for the locked rows", async () => {
		vi.stubEnv(FRAME_MODEL_ENV, "claude-code/opus");
		vi.stubEnv("ULTRON_CLAUDE_MODEL", "haiku");
		vi.stubEnv(REVIEW_MODEL_ENV, "not-a-ref");
		const read = await fakeWorker(SettingsManager.inMemory()).readSettings(BACKGROUND_CONTEXT);
		expect(read.envOverrides).toEqual({
			"rlm.frameModel": { name: FRAME_MODEL_ENV, value: "claude-code/opus" },
			"claudeCode.model": { name: "ULTRON_CLAUDE_MODEL", value: "haiku" },
		});
		expect(modelEnvOverrides({})).toEqual({});
	});
});

const summary = (provider: string, modelId: string, api = "openai-completions"): ModelSummary => ({
	provider,
	modelId,
	name: modelId,
	reasoning: true,
	model: { provider, id: modelId, api } as never,
});
const CATALOG = [
	summary("openai", "gpt-5"),
	summary("cliproxyapi", "glm-5.3-flash"),
	summary("claude-code", "haiku", "claude-code-cli"),
	summary("claude-code", "claude-opus-5-5", "claude-code-cli"),
];

describe("the Models rows and pickers", () => {
	beforeAll(() => {
		initTheme("dark");
		setKeybindings(new KeybindingsManager());
	});

	test("rows show the effective value and its source; env overrides are locked; child rows offer tool models only", () => {
		const rows = modelSettingRows({
			values: {
				"rlm.frameModel": "claude-code/haiku",
				"claudeCode.model": "sonnet",
				defaultProvider: "openai",
				defaultModel: "gpt-5",
			},
			envOverrides: { "review.model": { name: REVIEW_MODEL_ENV, value: "openai/gpt-5" } },
			available: CATALOG,
			session: { provider: "openai", modelId: "gpt-5" },
		});
		const byId = new Map(rows.map((row) => [row.id, row]));
		expect(byId.get("session")?.display).toBe("openai/gpt-5 (session)");
		expect(byId.get("rlm.frameModel")?.display).toBe("claude-code/haiku (setting)");
		expect(byId.get("rlm.childModel")?.display).toBe("same as session (openai/gpt-5)");
		expect(byId.get("rlm.frameThinking")?.display).toBe("same as session");
		expect(byId.get("claudeCode.model")?.display).toBe("sonnet (setting)");
		expect(byId.get("claudeCode.frameModel")?.display).toBe("claude-code/claude-opus-5-5 (default)");
		const review = byId.get("review.model")!;
		expect(review.display).toBe(`openai/gpt-5 (set by ${REVIEW_MODEL_ENV})`);
		expect(review.locked).toMatch(/Set by ULTRON_REVIEW_MODEL/);

		const values = (id: string) => byId.get(id)!.choices!().map((choice) => choice.value);
		expect(values("rlm.frameModel")).toEqual(["", ...CATALOG.map((m) => `${m.provider}/${m.modelId}`)]);
		expect(values("rlm.childModel")).toEqual(["", "openai/gpt-5", "cliproxyapi/glm-5.3-flash"]);
		expect(values("session")).toEqual(["openai/gpt-5", "cliproxyapi/glm-5.3-flash"]);
		expect(values("claudeCode.model")).toEqual(["", "claude-code/haiku", "claude-code/claude-opus-5-5"]);
		expect(values("rlm.frameThinking")).toEqual(["", "off", "low", "medium", "high"]);
		// The picker groups by provider and marks the current value.
		const items = modelPickerItems(byId.get("rlm.frameModel")!.choices!(), "claude-code/haiku").map((item) =>
			plain(item.label),
		);
		expect(items).toEqual([
			"  Same as session model",
			"── claude-code ──",
			"  claude-opus-5-5",
			"✓ haiku",
			"── cliproxyapi ──",
			"  glm-5.3-flash",
			"── openai ──",
			"  gpt-5",
		]);
		expect(displayAfterPick(byId.get("claudeCode.model")!, "claude-code/haiku", "openai/gpt-5")).toBe(
			"haiku (setting)",
		);
		expect(displayAfterPick(byId.get("rlm.frameModel")!, "", "openai/gpt-5")).toBe("same as session (openai/gpt-5)");
	});

	test("choosing a model in the picker saves it; a locked row opens nothing; headings are inert", () => {
		const rows = modelSettingRows({
			values: {},
			envOverrides: { "rlm.childModel": { name: "X", value: "a/b" } },
			available: CATALOG,
			session: { provider: "openai", modelId: "gpt-5" },
		});
		const picked: Array<[string, string]> = [];
		const submenu = new ModelsSettingsSubmenu(
			rows,
			(row, value) => {
				picked.push([row.id, value]);
				return `${value} (setting)`;
			},
			() => {},
		);
		const screen = () => plain(submenu.render(120).join("\n"));
		expect(screen()).toContain("Frame model");
		expect(screen()).toContain("Claude Code mode");
		// Down to "Frame model", open its picker, filter to haiku and choose it.
		submenu.handleInput("\u001b[B");
		submenu.handleInput("\r");
		expect(screen()).toContain("Same as session model");
		expect(screen()).toContain("── claude-code ──");
		for (const character of "haiku") submenu.handleInput(character);
		submenu.handleInput("\r");
		expect(picked).toEqual([["rlm.frameModel", "claude-code/haiku"]]);
		expect(screen()).toContain("claude-code/haiku (setting)");
		// Sub-agent model is locked (row 4): Enter keeps the list.
		submenu.handleInput("\u001b[B");
		submenu.handleInput("\u001b[B");
		submenu.handleInput("\r");
		expect(screen()).toContain("a/b (set by X) · locked");
		expect(screen()).not.toContain("Same as session model");
		expect(picked).toHaveLength(1);
	});
});

/** Lanes answering each frame prompt once, recording the model and thinking level each frame ran with. */
function frameLanes() {
	const runs: Array<{ lane: string; model?: string; thinking?: string }> = [];
	const harness = {
		hooks: { on: () => () => {} },
		lane: async (name: string) => {
			const run: { lane: string; model?: string; thinking?: string } = { lane: name };
			runs.push(run);
			const entries: Array<{ id: string; type: "message"; message: Record<string, unknown> }> = [];
			return {
				getActiveTools: async () => [],
				setActiveTools: async () => {},
				setModel: async (selected: { provider: string; modelId: string }) => {
					run.model = `${selected.provider}/${selected.modelId}`;
				},
				getModel: async () => ({ provider: "session", id: "model", reasoning: true }),
				setThinkingLevel: async (level: string) => {
					run.thinking = level;
				},
				steer: async () => ({ ok: true, value: {} }),
				abort: async () => ({ ok: true }),
				prompt: async () => {
					const id = `${name}#1`;
					entries.push({
						id,
						type: "message",
						message: { role: "assistant", content: [{ type: "text", text: "ok" }], stopReason: "stop" },
					});
					return { ok: true, value: { status: "completed", tipId: id, fromTipId: null } };
				},
				findEntries: async () => [...entries].reverse(),
			};
		},
	};
	return { harness, runs };
}

describe("the running Session picks up model changes", () => {
	test("a frame started after /settings changes rlm.frameModel runs on the new model, without a restart", async () => {
		const settings = SettingsManager.inMemory();
		const control = fakeWorker(settings);
		const { harness, runs } = frameLanes();
		const inference = createInferenceRuntime({
			contextDir: tempDir("ultron-models-frames-"),
			traces: createMemoryFrameStore(),
			modelSettings: () => ({ rlm: settings.getRlmModelSettings(), reviewModel: settings.getReviewModel() }),
			env: {},
		});
		inference.install(harness as never);
		const host = new NativeRlmHost(harness as never, {} as never, {
			store: memoryStore(),
			definitionStore: memoryDefinitionStore(),
			usage: new NativeUsageLedger(),
			frames: inference.executor,
			modules: [inference.module],
		});
		const infer = () => host.handle("rlm.infer", { task: "Say ok.", context: [] }, {} as Context, undefined);

		await infer();
		expect(runs[0]).toEqual({ lane: expect.any(String) });
		await control.setSetting("rlm.frameModel", "claude-code/haiku", BACKGROUND_CONTEXT);
		await control.setSetting("rlm.frameThinking", "low", BACKGROUND_CONTEXT);
		await infer();
		expect(runs[1]).toMatchObject({ model: "claude-code/haiku", thinking: "low" });
		// An explicit model= still wins, and a frame with depth keeps the session's tool-capable model.
		await host.handle("rlm.infer", { task: "x", context: [], model: "openai/gpt-5" }, {} as Context, undefined);
		expect(runs[2]?.model).toBe("openai/gpt-5");
		await control.setSetting("review.model", "claude-code/sonnet", BACKGROUND_CONTEXT);
		expect(await host.handle("rlm.models", {}, {} as Context, undefined)).toEqual({
			frame: { model: "claude-code/haiku", source: "setting" },
			review: { model: "claude-code/sonnet", source: "setting" },
			child: { source: "session" },
			frameThinking: "low",
		});
		await control.setSetting("rlm.frameModel", null, BACKGROUND_CONTEXT);
		await infer();
		expect(runs[3]?.model).toBeUndefined();
	});

	test("ULTRON_RLM_FRAME_MODEL wins over rlm.frameModel", async () => {
		const settings = SettingsManager.inMemory({ rlm: { frameModel: "claude-code/haiku" } });
		const { harness, runs } = frameLanes();
		const inference = createInferenceRuntime({
			contextDir: tempDir("ultron-models-frames-"),
			modelSettings: () => ({ rlm: settings.getRlmModelSettings() }),
			env: { [FRAME_MODEL_ENV]: "claude-code/opus" },
		});
		const host = new NativeRlmHost(harness as never, {} as never, {
			store: memoryStore(),
			definitionStore: memoryDefinitionStore(),
			frames: inference.executor,
			modules: [inference.module],
		});
		await host.handle("rlm.infer", { task: "x", context: [] }, {} as Context, undefined);
		expect(runs[0]?.model).toBe("claude-code/opus");
	});

	test("rlm.spawn reads rlm.childModel at each spawn", async () => {
		const settings = SettingsManager.inMemory();
		const fixture = hostFixture({ childModel: () => settings.getRlmModelSettings().childModel, script: () => "ok" });
		const first = await fixture.call<{ model?: string }>("rlm.spawn", { prompt: "x", kwargs: { name: "x" } });
		expect(first.model).not.toBe("cliproxyapi/glm-5.3-flash");
		settings.setRlmModelSetting("childModel", "cliproxyapi/glm-5.3-flash");
		const second = await fixture.call<{ model?: string }>("rlm.spawn", { prompt: "y", kwargs: { name: "y" } });
		expect(second.model).toBe("cliproxyapi/glm-5.3-flash");
		await fixture.host.close();
	});
});
