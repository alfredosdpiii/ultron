import { type Context, defineService, type JsonValue } from "@ultron/chord";
import type { CommandSourceInfo } from "./legacy-extensions.ts";

export type SessionQueueMode = "all" | "one-at-a-time";

export interface SessionControlSettings {
	name: string | null;
	steeringMode: SessionQueueMode;
	followUpMode: SessionQueueMode;
	autoCompaction: boolean;
	autoRetry: boolean;
}

export interface SessionCommandInfo {
	name: string;
	description: string | null;
	source: "extension" | "prompt" | "skill";
	/** Pi's `SourceInfo` for the owning resource, when the worker knows it. */
	sourceInfo: CommandSourceInfo | null;
}

export interface SessionBashResult {
	output: string;
	exitCode: number | null;
	cancelled: boolean;
	truncated: boolean;
	fullOutputPath: string | null;
}

/** The whole Session tree (every branch) in Pi's entry format, read from the worker's storage. */
export interface SessionTreeRead {
	/** Pi `SessionEntry` objects (typed as JSON for the service contract) in storage order, ids unchanged. */
	entries: JsonValue[];
	/** The entry the main lane's tip maps to. */
	leafId: string | null;
	/** Resolved labels by target entry id. */
	labels: Record<string, string>;
	/** The native Session file. */
	sessionFile: string | null;
}

/**
 * The worker's profile and project settings as Pi's settings selector shows them (`SettingsManager` getters, plus
 * the Session's live compaction and queue modes). Plain JSON; the worker's settings are the source of truth.
 */
export interface WorkerSettingsRead {
	/** Effective values by setting key (see `WORKER_SETTING_KEYS`), as the worker's `SettingsManager` reports them. */
	values: Record<string, JsonValue>;
	/** The Session cwd whose project settings apply. */
	cwd: string;
	/** The profile directory holding `settings.json`, `auth.json` and `trust.json`. */
	agentDir: string;
	/** Whether this worker loaded the project's settings and resources. */
	projectTrusted: boolean;
	/** The saved trust decision that applies to `cwd` (it may be inherited from a parent directory). */
	savedTrust: { path: string; decision: boolean } | null;
}

/** How the worker applied a setting change. */
export interface WorkerSettingResult {
	/**
	 * `live`: saved and applied to the running Session; `restart`: saved, applies when the Session worker restarts;
	 * `saved`: saved only (a presentation setting the client applies, or one the worker does not use).
	 */
	applied: "live" | "restart" | "saved";
}

/** Non-secret runtime facts about the Session worker for `/debug`. */
export interface SessionDebugInfo {
	pid: number;
	parentPid: number;
	nodeVersion: string;
	version: string;
	platform: string;
	cwd: string;
	agentDir: string;
	sessionFile: string | null;
	uptimeMs: number;
	rssBytes: number;
	model: string | null;
	thinkingLevel: string;
	/** `rlm.pool` inspection (live kernels, pins, evictions), or the error reading it. */
	kernelPool: JsonValue;
	/** `ULTRON_*`/`PI_*` environment names; values of secret-looking names are redacted. */
	environment: Record<string, string>;
}

/** Session settings and user actions that act on the worker-owned harness directly. */
export interface SessionControl {
	getSettings(context: Context): Promise<SessionControlSettings>;
	setName(name: string, context: Context): Promise<void>;
	setSteeringMode(mode: SessionQueueMode, context: Context): Promise<void>;
	setFollowUpMode(mode: SessionQueueMode, context: Context): Promise<void>;
	setAutoCompaction(enabled: boolean, context: Context): Promise<void>;
	setAutoRetry(enabled: boolean, context: Context): Promise<void>;
	listCommands(context: Context): Promise<SessionCommandInfo[]>;
	/** Run a user shell command in the Session cwd and record it for the model unless excluded. */
	bash(command: string, excludeFromContext: boolean, context: Context): Promise<SessionBashResult>;
	abortBash(context: Context): Promise<void>;
	/** Read recorded runtime state (tasks, memory evidence, skills, experiments) without side effects. */
	inspect(request: string, payload: JsonValue, context: Context): Promise<JsonValue>;
	/** Read every entry of the Session (all branches) as Pi entries. Read-only. */
	readTree(context: Context): Promise<SessionTreeRead>;
	/** Set or clear (`null`) an entry's label, as Pi's tree selector does. */
	setLabel(entryId: string, label: string | null, context: Context): Promise<void>;
	/**
	 * Pi's `session_before_fork` in this Session's extensions, before a client forks or clones it. Extensions may
	 * cancel; the client then creates no fork.
	 */
	beforeFork(entryId: string, position: "before" | "at", context: Context): Promise<{ cancelled: boolean }>;
	/** The client moved on to `targetSessionFile`, a fork of this Session (Pi's `session_shutdown` reason "fork"). */
	forked(targetSessionFile: string | null, context: Context): Promise<void>;
	/** Pi's `/settings` values from the worker's `SettingsManager` (profile and project). */
	readSettings(context: Context): Promise<WorkerSettingsRead>;
	/**
	 * Persist one setting through the worker's `SettingsManager` and apply it to the running Session where Pi
	 * applies it live. `key` is one of `WORKER_SETTING_KEYS`.
	 */
	setSetting(key: string, value: JsonValue, context: Context): Promise<WorkerSettingResult>;
	/** Pi's `/trust`: save project trust decisions to the worker profile's `trust.json`. Applies on restart. */
	setProjectTrust(updates: { path: string; decision: boolean | null }[], context: Context): Promise<void>;
	/**
	 * Re-read `<agentDir>/auth.json` after a client `/login` or `/logout` and rebuild model availability, so the next
	 * request uses the new credentials. Returns how many models are available afterwards.
	 */
	reloadAuth(providerId: string | null, context: Context): Promise<{ availableModels: number }>;
	/** Worker facts for `/debug`; never includes credentials. */
	debugInfo(context: Context): Promise<SessionDebugInfo>;
}

/**
 * Settings `setSetting` accepts. Keys name the `Settings` field (dotted for nested ones); `modelThinkingLevel`
 * takes `{ provider, modelId, level | null }`.
 */
export const WORKER_SETTING_KEYS: readonly string[] = [
	"compaction.enabled",
	"retry.enabled",
	"steeringMode",
	"followUpMode",
	"transport",
	"httpIdleTimeoutMs",
	"cacheWarming",
	"modelThinkingLevel",
	"defaultThinkingLevel",
	"theme",
	"hideThinkingBlock",
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
	"defaultProjectTrust",
	"doubleEscapeAction",
	"treeFilterMode",
	"showHardwareCursor",
	"editorPaddingX",
	"outputPad",
	"autocompleteMaxVisible",
	"terminal.clearOnShrink",
	"terminal.showTerminalProgress",
	"tuiMode",
	"fullscreenExitOutput",
	"fullscreenScrollbar",
	"fullscreenCopyOnSelect",
	"warnings",
	"enabledModels",
];

/** Read-only host requests the inspector may issue. None of them runs a search or starts work. */
export const INSPECTION_REQUESTS: readonly string[] = [
	"agents.status",
	"agents.list",
	"agents.inspect",
	"memory.why",
	"memory.list",
	"skills.list",
	"skills.why",
	"experiments.list",
	"refinements.list",
	"progress.history",
	"progress.assess",
	"schedules.list",
	"goals.list",
	"goals.get",
	"instances.list",
	"instances.get",
	"gates.list",
	"gates.history",
	"grants.list",
	"rlm.pool",
	"rlm.frames",
	"jev.decisions",
	"ctx.state",
	"async.pending",
];

export const SessionControl = defineService<SessionControl>("ultron.session-control");
