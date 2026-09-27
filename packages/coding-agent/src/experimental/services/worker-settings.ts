/**
 * The Session worker's side of Pi's `/settings` and `/trust`: read the worker's `SettingsManager` (profile and
 * project settings, the source of truth), persist a change through it, and apply it to the running Session where
 * Pi applies it live (compaction, retry, queue modes, transport, HTTP idle timeout, per-model thinking levels).
 */

import type { AgentHarness, AgentLane, ThinkingLevel } from "@ultron/agent-core";
import type { Context, JsonValue } from "@ultron/chord";
import { isValidThinkingLevel } from "../../cli/args.ts";
import { DEFAULT_THINKING_LEVEL } from "../../core/defaults.ts";
import { configureHttpDispatcher } from "../../core/http-dispatcher.ts";
import {
	CACHE_WARMING_MODES,
	type DefaultProjectTrust,
	type Settings,
	SettingsManager,
} from "../../core/settings-manager.ts";
import { hasTrustRequiringProjectResources, ProjectTrustStore } from "../../core/trust-manager.ts";
import type { WorkerSettingResult, WorkerSettingsRead } from "./session-control.ts";

export interface WorkerSettingsTarget {
	readonly harness: AgentHarness;
	readonly lane: AgentLane;
	readonly settingsManager: SettingsManager;
	readonly cwd: string;
	readonly agentDir: string;
	/** Reconfigure the process's HTTP stack (Pi's `configureHttpDispatcher`); injectable for tests. */
	readonly configureHttpIdleTimeout?: (timeoutMs: number) => void;
}

/**
 * Whether a Session worker loads the project's settings and resources: the saved `/trust` decision for `cwd`,
 * else `defaultProjectTrust` ("never" distrusts; "ask" cannot prompt in a worker, so it trusts as before).
 * A project without trust-requiring resources is trusted, as in Pi.
 */
export function workerProjectTrusted(cwd: string, agentDir: string): boolean {
	if (!hasTrustRequiringProjectResources(cwd)) return true;
	const decision = new ProjectTrustStore(agentDir).get(cwd);
	if (decision !== null) return decision;
	const bootstrap = SettingsManager.create(cwd, agentDir, { projectTrusted: false });
	return bootstrap.getDefaultProjectTrust() !== "never";
}

export async function readWorkerSettings(target: WorkerSettingsTarget, context: Context): Promise<WorkerSettingsRead> {
	const { harness, settingsManager: settings } = target;
	const [compaction, retry, steeringMode, followUpMode] = await Promise.all([
		harness.getCompactionSettings(context),
		harness.getRetryPolicy(context),
		harness.getSteeringMode(context),
		harness.getFollowUpMode(context),
	]);
	const values: Record<string, JsonValue | undefined> = {
		"compaction.enabled": compaction.enabled,
		"retry.enabled": retry.enabled,
		steeringMode,
		followUpMode,
		transport: settings.getTransport(),
		httpIdleTimeoutMs: settings.getHttpIdleTimeoutMs(),
		cacheWarming: settings.getCacheWarmingMode(),
		modelThinkingLevels: settings.getAllModelThinkingLevels(),
		defaultThinkingLevel: settings.getDefaultThinkingLevel() ?? DEFAULT_THINKING_LEVEL,
		defaultProvider: settings.getDefaultProvider() ?? null,
		defaultModel: settings.getDefaultModel() ?? null,
		theme: settings.getThemeSetting() ?? null,
		hideThinkingBlock: settings.getHideThinkingBlock(),
		"terminal.showImages": settings.getShowImages(),
		"terminal.imageWidthCells": settings.getImageWidthCells(),
		"images.autoResize": settings.getImageAutoResize(),
		"images.blockImages": settings.getBlockImages(),
		enableSkillCommands: settings.getEnableSkillCommands(),
		"markdown.mermaid": settings.getMermaidRenderingMode(),
		showCacheMissNotices: settings.getShowCacheMissNotices(),
		collapseChangelog: settings.getCollapseChangelog(),
		enableInstallTelemetry: settings.getEnableInstallTelemetry(),
		quietStartup: settings.getQuietStartup(),
		rlmPaneAutoOpen: settings.getRlmPaneAutoOpen(),
		defaultProjectTrust: settings.getDefaultProjectTrust(),
		doubleEscapeAction: settings.getDoubleEscapeAction(),
		treeFilterMode: settings.getTreeFilterMode(),
		showHardwareCursor: settings.getShowHardwareCursor(),
		editorPaddingX: settings.getEditorPaddingX(),
		outputPad: settings.getOutputPad(),
		autocompleteMaxVisible: settings.getAutocompleteMaxVisible(),
		"terminal.clearOnShrink": settings.getClearOnShrink(),
		"terminal.showTerminalProgress": settings.getShowTerminalProgress(),
		tuiMode: settings.getTuiMode(),
		fullscreenExitOutput: settings.getFullscreenExitOutput(),
		fullscreenScrollbar: settings.getFullscreenScrollbar(),
		fullscreenCopyOnSelect: settings.getFullscreenCopyOnSelect(),
		warnings: { ...settings.getWarnings() } as JsonValue,
		enabledModels: settings.getEnabledModels() ?? null,
	};
	const saved = new ProjectTrustStore(target.agentDir).getEntry(target.cwd);
	return {
		values: Object.fromEntries(Object.entries(values).filter(([, value]) => value !== undefined)) as Record<
			string,
			JsonValue
		>,
		cwd: target.cwd,
		agentDir: target.agentDir,
		projectTrusted: settings.isProjectTrusted(),
		savedTrust: saved === null ? null : { path: saved.path, decision: saved.decision },
	};
}

/** Persist `key` through the worker's `SettingsManager`, then apply it to the running Session where Pi does. */
export async function applyWorkerSetting(
	target: WorkerSettingsTarget,
	key: string,
	value: JsonValue,
	context: Context,
): Promise<WorkerSettingResult> {
	const { harness, lane, settingsManager: settings } = target;
	let applied: WorkerSettingResult["applied"] = "saved";
	// Errors recorded before this change (a malformed file at startup) are not this change's failure.
	settings.drainErrors();
	switch (key) {
		case "compaction.enabled": {
			const enabled = bool(key, value);
			settings.setCompactionEnabled(enabled);
			await harness.setCompactionSettings({ ...(await harness.getCompactionSettings(context)), enabled }, context);
			applied = "live";
			break;
		}
		case "retry.enabled": {
			const enabled = bool(key, value);
			settings.setRetryEnabled(enabled);
			await harness.setRetryPolicy({ ...(await harness.getRetryPolicy(context)), enabled }, context);
			applied = "live";
			break;
		}
		case "steeringMode":
		case "followUpMode": {
			const mode = oneOf(key, value, ["all", "one-at-a-time"] as const);
			if (key === "steeringMode") {
				settings.setSteeringMode(mode);
				await harness.setSteeringMode(mode, context);
			} else {
				settings.setFollowUpMode(mode);
				await harness.setFollowUpMode(mode, context);
			}
			applied = "live";
			break;
		}
		case "transport": {
			const transport = oneOf(key, value, ["sse", "websocket", "websocket-cached", "auto"] as const);
			settings.setTransport(transport);
			await harness.setStreamOptions({ ...(await harness.getStreamOptions(context)), transport }, context);
			applied = "live";
			break;
		}
		case "httpIdleTimeoutMs": {
			const timeoutMs = int(key, value, 0);
			settings.setHttpIdleTimeoutMs(timeoutMs);
			(target.configureHttpIdleTimeout ?? configureHttpDispatcher)(timeoutMs);
			// Model requests carry the idle timeout unless `retry.provider.timeoutMs` pins one (providerRequestOptions).
			if (settings.getProviderRetrySettings().timeoutMs === undefined) {
				await harness.setStreamOptions(
					{
						...(await harness.getStreamOptions(context)),
						timeoutMs: timeoutMs === 0 ? 2147483647 : timeoutMs,
					},
					context,
				);
			}
			applied = "live";
			break;
		}
		case "cacheWarming":
			// Saved for Pi; the Ultron worker does not warm provider caches.
			settings.setCacheWarmingMode(oneOf(key, value, CACHE_WARMING_MODES));
			break;
		case "modelThinkingLevel": {
			const target = record(key, value);
			const provider = string(key, target.provider);
			const modelId = string(key, target.modelId);
			const level = target.level === null ? null : thinking(key, target.level);
			if (level === null) settings.removeModelThinkingLevel(provider, modelId);
			else settings.setModelThinkingLevel(provider, modelId, level);
			// As in Pi, an override for the current model applies to the Session now.
			const current = await lane.getModel(context);
			if (current?.provider === provider && current.id === modelId) {
				await lane.setThinkingLevel(level ?? settings.getDefaultThinkingLevel() ?? DEFAULT_THINKING_LEVEL, context);
				applied = "live";
			}
			break;
		}
		case "defaultThinkingLevel":
			settings.setDefaultThinkingLevel(thinking(key, value));
			applied = "restart";
			break;
		case "theme":
			settings.setTheme(string(key, value));
			break;
		case "hideThinkingBlock":
			settings.setHideThinkingBlock(bool(key, value));
			break;
		case "terminal.showImages":
			settings.setShowImages(bool(key, value));
			break;
		case "terminal.imageWidthCells":
			settings.setImageWidthCells(int(key, value, 1));
			break;
		case "images.autoResize":
			settings.setImageAutoResize(bool(key, value));
			break;
		case "images.blockImages":
			settings.setBlockImages(bool(key, value));
			break;
		case "enableSkillCommands":
			settings.setEnableSkillCommands(bool(key, value));
			break;
		case "markdown.mermaid":
			settings.setMermaidRenderingMode(oneOf(key, value, ["off", "final", "streaming"] as const));
			break;
		case "showCacheMissNotices":
			settings.setShowCacheMissNotices(bool(key, value));
			break;
		case "collapseChangelog":
			settings.setCollapseChangelog(bool(key, value));
			break;
		case "enableInstallTelemetry":
			settings.setEnableInstallTelemetry(bool(key, value));
			break;
		case "quietStartup":
			settings.setQuietStartup(bool(key, value));
			break;
		case "rlmPaneAutoOpen":
			settings.setRlmPaneAutoOpen(bool(key, value));
			break;
		case "defaultProjectTrust":
			settings.setDefaultProjectTrust(
				oneOf(key, value, ["ask", "always", "never"] as const satisfies readonly DefaultProjectTrust[]),
			);
			applied = "restart";
			break;
		case "doubleEscapeAction":
			settings.setDoubleEscapeAction(oneOf(key, value, ["fork", "tree", "none"] as const));
			break;
		case "treeFilterMode":
			settings.setTreeFilterMode(
				oneOf(key, value, ["default", "no-tools", "user-only", "labeled-only", "all"] as const),
			);
			break;
		case "showHardwareCursor":
			settings.setShowHardwareCursor(bool(key, value));
			break;
		case "editorPaddingX":
			settings.setEditorPaddingX(int(key, value, 0));
			break;
		case "outputPad":
			settings.setOutputPad(int(key, value, 0) === 0 ? 0 : 1);
			break;
		case "autocompleteMaxVisible":
			settings.setAutocompleteMaxVisible(int(key, value, 1));
			break;
		case "terminal.clearOnShrink":
			settings.setClearOnShrink(bool(key, value));
			break;
		case "terminal.showTerminalProgress":
			settings.setShowTerminalProgress(bool(key, value));
			break;
		case "tuiMode":
			settings.setTuiMode(oneOf(key, value, ["regular", "fullscreen"] as const));
			break;
		case "fullscreenExitOutput":
			settings.setFullscreenExitOutput(oneOf(key, value, ["transcript", "resume-hint"] as const));
			break;
		case "fullscreenScrollbar":
			settings.setFullscreenScrollbar(oneOf(key, value, ["hidden", "auto", "always"] as const));
			break;
		case "fullscreenCopyOnSelect":
			settings.setFullscreenCopyOnSelect(bool(key, value));
			break;
		case "warnings": {
			const warnings = record(key, value);
			settings.setWarnings(
				warnings.anthropicExtraUsage === undefined
					? {}
					: { anthropicExtraUsage: bool(key, warnings.anthropicExtraUsage) },
			);
			break;
		}
		case "enabledModels":
			settings.setEnabledModels(
				value === null ? undefined : array(key, value).map((pattern) => string(key, pattern)),
			);
			break;
		default:
			throw new Error(`Unknown setting: ${key}`);
	}
	await settings.flush();
	const errors = settings.drainErrors();
	if (errors.length > 0) throw new Error(`Could not save ${key}: ${errors.map((e) => e.error.message).join("; ")}`);
	return { applied };
}

/** The `Settings` fragment a `setSetting` key and value correspond to (what the client overlays locally). */
export function settingsFragment(key: string, value: JsonValue): Partial<Settings> | undefined {
	if (key === "modelThinkingLevel" || (key === "enabledModels" && value === null)) return undefined;
	const path = key.split(".");
	if (path.length === 1) return { [key]: value } as Partial<Settings>;
	return { [path[0]!]: { [path[1]!]: value } } as Partial<Settings>;
}

function bool(key: string, value: JsonValue): boolean {
	if (typeof value !== "boolean") throw new Error(`${key} must be true or false`);
	return value;
}

function int(key: string, value: JsonValue, min: number): number {
	if (typeof value !== "number" || !Number.isFinite(value) || value < min) {
		throw new Error(`${key} must be a number of at least ${min}`);
	}
	return Math.floor(value);
}

function string(key: string, value: JsonValue | undefined): string {
	if (typeof value !== "string" || value.length === 0) throw new Error(`${key} must be a non-empty string`);
	return value;
}

function thinking(key: string, value: JsonValue | undefined): ThinkingLevel {
	const level = string(key, value);
	if (!isValidThinkingLevel(level)) throw new Error(`${key}: invalid thinking level ${level}`);
	return level;
}

function oneOf<const T extends string>(key: string, value: JsonValue, allowed: readonly T[]): T {
	if (typeof value !== "string" || !allowed.includes(value as T)) {
		throw new Error(`${key} must be one of: ${allowed.join(", ")}`);
	}
	return value as T;
}

function record(key: string, value: JsonValue): Record<string, JsonValue> {
	if (typeof value !== "object" || value === null || Array.isArray(value)) throw new Error(`${key} must be an object`);
	return value;
}

function array(key: string, value: JsonValue): JsonValue[] {
	if (!Array.isArray(value)) throw new Error(`${key} must be a list`);
	return value;
}
