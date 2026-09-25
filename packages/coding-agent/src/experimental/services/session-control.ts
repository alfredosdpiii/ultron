import { type Context, defineService } from "@earendil-works/chord";

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
}

export interface SessionBashResult {
	output: string;
	exitCode: number | null;
	cancelled: boolean;
	truncated: boolean;
	fullOutputPath: string | null;
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
}

export const SessionControl = defineService<SessionControl>("ultron.session-control");
