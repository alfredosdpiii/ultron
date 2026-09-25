import { type Context, defineService, type JsonValue } from "@earendil-works/chord";
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
}

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
	"jev.decisions",
];

export const SessionControl = defineService<SessionControl>("ultron.session-control");
