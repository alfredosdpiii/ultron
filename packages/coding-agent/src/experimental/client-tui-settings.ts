/**
 * Pi's `/settings` in the native TUI. The values come from the Session worker's `SettingsManager` (profile and
 * project settings are the worker's), and every change is written back through `SessionControl.setSetting`, which
 * persists it there and applies it to the running Session where Pi applies it live. Presentation settings (theme,
 * thinking visibility, editor padding, cursor) are then applied by the client, as Pi's interactive mode does.
 */

import type { ThinkingLevel } from "@ultron/agent-core";
import type { JsonValue } from "@ultron/chord";
import { BACKGROUND_CONTEXT } from "@ultron/chord/context";
import { DEFAULT_THINKING_LEVEL, THINKING_LEVEL_OPTIONS } from "../core/defaults.ts";
import { formatHttpIdleTimeoutMs } from "../core/http-dispatcher.ts";
import type { Settings, SettingsManager } from "../core/settings-manager.ts";
import { type SettingsConfig, SettingsSelectorComponent } from "../modes/interactive/components/settings-selector.ts";
import { getAvailableThemes } from "../modes/interactive/theme/theme.ts";
import type { PiCommandHost } from "./client-tui-pi-commands.ts";
import type { WorkerSettingResult } from "./services/session-control.ts";
import { settingsFragment } from "./services/worker-settings.ts";

/** Settings the client applies to itself when they change (the rest of the presentation reads them on use). */
const CLIENT_LIVE = new Set([
	"hideThinkingBlock",
	"theme",
	"showHardwareCursor",
	"terminal.clearOnShrink",
	"editorPaddingX",
	"autocompleteMaxVisible",
	"doubleEscapeAction",
	"treeFilterMode",
	"enabledModels",
	"compaction.enabled",
]);

/** Pi settings that neither the native TUI nor the Session worker reads; they are saved for Pi. */
const UNUSED_BY_ULTRON = new Set([
	"terminal.showImages",
	"terminal.imageWidthCells",
	"images.autoResize",
	"images.blockImages",
	"enableSkillCommands",
	"markdown.mermaid",
	"showCacheMissNotices",
	"collapseChangelog",
	"enableInstallTelemetry",
	"quietStartup",
	"outputPad",
	"terminal.showTerminalProgress",
	"tuiMode",
	"fullscreenExitOutput",
	"fullscreenScrollbar",
	"fullscreenCopyOnSelect",
	"warnings",
	"cacheWarming",
]);

/**
 * The client's copy of the settings, kept in step with the worker's: after a change it re-reads its files (the same
 * profile for a local server) and overlays every value this client changed (a remote worker's profile differs).
 */
export class ClientSettingsMirror {
	readonly #settings: SettingsManager;
	#overlay: Partial<Settings> = {};

	constructor(settings: SettingsManager) {
		this.#settings = settings;
	}

	async apply(key: string, value: JsonValue): Promise<void> {
		const fragment = settingsFragment(key, value);
		if (fragment !== undefined) this.#overlay = mergeFragment(this.#overlay, fragment);
		await this.#settings.reload();
		this.#settings.applyOverrides(this.#overlay);
	}
}

function mergeFragment(base: Partial<Settings>, fragment: Partial<Settings>): Partial<Settings> {
	const merged: Record<string, unknown> = { ...base };
	for (const [key, value] of Object.entries(fragment)) {
		const previous = merged[key];
		merged[key] =
			typeof previous === "object" && previous !== null && !Array.isArray(previous) && typeof value === "object"
				? { ...previous, ...(value as object) }
				: value;
	}
	return merged as Partial<Settings>;
}

/** Persist one setting in the worker, mirror it locally and apply it to the presentation; reports the outcome. */
export async function changeSetting(
	host: PiCommandHost,
	key: string,
	value: JsonValue,
	options: { readonly quiet?: boolean } = {},
): Promise<WorkerSettingResult["applied"] | undefined> {
	const control = host.control();
	if (control === undefined) {
		host.showStatus("Error: No Session is attached");
		return undefined;
	}
	let result: WorkerSettingResult;
	try {
		result = await control.setSetting(key, value, BACKGROUND_CONTEXT);
		await host.settingsMirror.apply(key, value);
	} catch (error) {
		host.showStatus(`Error: ${error instanceof Error ? error.message : String(error)}`);
		return undefined;
	}
	if (CLIENT_LIVE.has(key)) host.applySettingLocally(key, value);
	if (options.quiet) return result.applied;
	if (result.applied === "restart") {
		host.showStatus(`Saved ${key}. It takes effect when the Session worker restarts.`);
	} else if (UNUSED_BY_ULTRON.has(key)) {
		host.showStatus(`Saved ${key} to settings. Ultron's native TUI and Session worker do not use it.`);
	} else if (key === "httpIdleTimeoutMs" && typeof value === "number") {
		host.showStatus(`HTTP idle timeout: ${formatHttpIdleTimeoutMs(value)}`);
	}
	return result.applied;
}

/** Pi's `/settings` selector over the worker's settings. */
export async function showSettingsSelector(host: PiCommandHost): Promise<void> {
	const control = host.control();
	if (control === undefined) {
		host.showStatus("Error: No Session is attached");
		return;
	}
	let read: Awaited<ReturnType<typeof control.readSettings>>;
	try {
		read = await control.readSettings(BACKGROUND_CONTEXT);
	} catch (error) {
		host.showStatus(`Error: ${error instanceof Error ? error.message : String(error)}`);
		return;
	}
	const values = read.values;
	const bool = (key: string, fallback: boolean): boolean =>
		typeof values[key] === "boolean" ? (values[key] as boolean) : fallback;
	const num = (key: string, fallback: number): number =>
		typeof values[key] === "number" ? (values[key] as number) : fallback;
	const str = <T extends string>(key: string, fallback: T): T =>
		typeof values[key] === "string" ? (values[key] as T) : fallback;
	const records = (host.models()?.state.value?.catalog.availableModels ?? []).flatMap((model) =>
		model.model === undefined ? [] : [model.model],
	);
	const defaultProvider = values.defaultProvider;
	const defaultModelId = values.defaultModel;
	const defaultThinking = str<ThinkingLevel>("defaultThinkingLevel", DEFAULT_THINKING_LEVEL);
	const change = (key: string, value: JsonValue): void => {
		void changeSetting(host, key, value);
	};
	/** A per-model thinking override for the current model applies to the Session now (Pi does the same). */
	const changeModelThinking = (provider: string, modelId: string, level: ThinkingLevel | null): void => {
		void (async () => {
			const applied = await changeSetting(host, "modelThinkingLevel", { provider, modelId, level });
			const models = host.models();
			if (applied !== "live" || models === undefined) return;
			// Keep the replicated model state (footer, editor border) in step with the worker's lane.
			await models.selectThinking(level ?? defaultThinking, BACKGROUND_CONTEXT).catch(() => {});
		})();
	};
	const config: SettingsConfig = {
		autoCompact: bool("compaction.enabled", true),
		defaultModel:
			typeof defaultProvider === "string" && typeof defaultModelId === "string"
				? `${defaultProvider}/${defaultModelId}`
				: "not set",
		...(host.currentModel()?.model === undefined ? {} : { currentModel: host.currentModel()!.model! }),
		availableDefaultModels: records,
		showImages: bool("terminal.showImages", true),
		imageWidthCells: num("terminal.imageWidthCells", 60),
		autoResizeImages: bool("images.autoResize", true),
		blockImages: bool("images.blockImages", false),
		enableSkillCommands: bool("enableSkillCommands", true),
		steeringMode: str("steeringMode", "all"),
		followUpMode: str("followUpMode", "all"),
		transport: str("transport", "auto"),
		httpIdleTimeoutMs: num("httpIdleTimeoutMs", 300_000),
		cacheWarmingMode: str("cacheWarming", "streaming"),
		thinkingLevel: defaultThinking,
		availableThinkingLevels: [...THINKING_LEVEL_OPTIONS],
		modelThinkingLevels: (values.modelThinkingLevels ?? {}) as Record<string, ThinkingLevel>,
		currentTheme: host.theme?.getThemeSelection() ?? str("theme", "dark"),
		terminalTheme: host.theme?.getTerminalTheme() ?? "dark",
		availableThemes: getAvailableThemes(),
		hideThinkingBlock: bool("hideThinkingBlock", false),
		mermaidRenderingMode: str("markdown.mermaid", "streaming"),
		showCacheMissNotices: bool("showCacheMissNotices", false),
		collapseChangelog: bool("collapseChangelog", false),
		enableInstallTelemetry: bool("enableInstallTelemetry", true),
		doubleEscapeAction: str("doubleEscapeAction", "tree"),
		treeFilterMode: str("treeFilterMode", "default"),
		showHardwareCursor: bool("showHardwareCursor", false),
		editorPaddingX: num("editorPaddingX", 0),
		outputPad: num("outputPad", 1) === 0 ? 0 : 1,
		autocompleteMaxVisible: num("autocompleteMaxVisible", 5),
		quietStartup: bool("quietStartup", false),
		rlmPaneAutoOpen: bool("rlmPaneAutoOpen", true),
		defaultProjectTrust: str("defaultProjectTrust", "ask"),
		clearOnShrink: bool("terminal.clearOnShrink", false),
		showTerminalProgress: bool("terminal.showTerminalProgress", false),
		// The native TUI always runs fullscreen.
		tuiMode: "fullscreen",
		fullscreenExitOutput: str("fullscreenExitOutput", "transcript"),
		fullscreenScrollbar: str("fullscreenScrollbar", "auto"),
		fullscreenCopyOnSelect: bool("fullscreenCopyOnSelect", true),
		warnings: (values.warnings ?? {}) as SettingsConfig["warnings"],
	};
	let close = (): void => {};
	const selector = new SettingsSelectorComponent(config, {
		onAutoCompactChange: (enabled) => change("compaction.enabled", enabled),
		onShowImagesChange: (enabled) => change("terminal.showImages", enabled),
		onImageWidthCellsChange: (width) => change("terminal.imageWidthCells", width),
		onAutoResizeImagesChange: (enabled) => change("images.autoResize", enabled),
		onBlockImagesChange: (blocked) => change("images.blockImages", blocked),
		onEnableSkillCommandsChange: (enabled) => change("enableSkillCommands", enabled),
		onSteeringModeChange: (mode) => change("steeringMode", mode),
		onFollowUpModeChange: (mode) => change("followUpMode", mode),
		onTransportChange: (transport) => change("transport", transport),
		onHttpIdleTimeoutMsChange: (timeoutMs) => change("httpIdleTimeoutMs", timeoutMs),
		onCacheWarmingModeChange: (mode) => change("cacheWarming", mode),
		onModelThinkingLevelChange: (provider, modelId, level) => changeModelThinking(provider, modelId, level),
		onModelThinkingLevelRemove: (provider, modelId) => changeModelThinking(provider, modelId, null),
		onThemeChange: (theme) => change("theme", theme),
		onThemePreview: (theme) => host.theme?.preview(theme),
		onHideThinkingBlockChange: (hidden) => change("hideThinkingBlock", hidden),
		onMermaidRenderingModeChange: (mode) => change("markdown.mermaid", mode),
		onShowCacheMissNoticesChange: (shown) => change("showCacheMissNotices", shown),
		onCollapseChangelogChange: (collapsed) => change("collapseChangelog", collapsed),
		onEnableInstallTelemetryChange: (enabled) => change("enableInstallTelemetry", enabled),
		onDoubleEscapeActionChange: (action) => change("doubleEscapeAction", action),
		onTreeFilterModeChange: (mode) => change("treeFilterMode", mode),
		onShowHardwareCursorChange: (enabled) => change("showHardwareCursor", enabled),
		onEditorPaddingXChange: (padding) => change("editorPaddingX", padding),
		onOutputPadChange: (padding) => change("outputPad", padding),
		onAutocompleteMaxVisibleChange: (maxVisible) => change("autocompleteMaxVisible", maxVisible),
		onQuietStartupChange: (enabled) => change("quietStartup", enabled),
		onRlmPaneAutoOpenChange: (enabled) => change("rlmPaneAutoOpen", enabled),
		onDefaultProjectTrustChange: (trust) => change("defaultProjectTrust", trust),
		onClearOnShrinkChange: (enabled) => change("terminal.clearOnShrink", enabled),
		onShowTerminalProgressChange: (enabled) => change("terminal.showTerminalProgress", enabled),
		onTuiModeChange: (mode) => {
			void changeSetting(host, "tuiMode", mode, { quiet: true }).then((applied) => {
				if (applied === undefined) return;
				selector.getSettingsList().updateValue("tui-mode", "fullscreen");
				host.showStatus(`Saved tuiMode: ${mode} for Pi. Ultron's native TUI always runs fullscreen.`);
			});
		},
		onFullscreenExitOutputChange: (output) => change("fullscreenExitOutput", output),
		onFullscreenScrollbarChange: (mode) => change("fullscreenScrollbar", mode),
		onFullscreenCopyOnSelectChange: (enabled) => change("fullscreenCopyOnSelect", enabled),
		onWarningsChange: (warnings) => change("warnings", { ...warnings } as JsonValue),
		onCancel: () => {
			close();
			host.requestRender();
		},
	});
	close = host.showComponent(selector, selector.getSettingsList());
}
