import { type Context, defineService, type ReplicatedState } from "@ultron/chord";
import type { ServerId } from "@ultron/protocol";

export interface SessionAddress {
	serverId: ServerId;
	sessionId: string;
}

export interface SessionSummary extends SessionAddress {
	createdAt: number;
	modifiedAt: number;
}

export interface SessionCreateOptions {
	id?: string;
	parentSessionId?: string;
	forkFromSessionId?: string;
	/**
	 * With `forkFromSessionId`: copy only the main lane's path from the root to `entryId` (default: the current tip),
	 * as Pi's fork does (`"before"` stops at the entry's parent, Pi's fork of a user message; `"at"` includes it,
	 * Pi's clone). Labels of copied entries are kept.
	 * Omitted, the whole tree is copied (Pi's `--fork`).
	 */
	forkPath?: SessionForkPath;
	name?: string;
}

export interface SessionForkPath {
	entryId?: string;
	position: "before" | "at";
}

export interface SessionDirectoryState {
	revision: number;
	sessions: SessionSummary[];
}

export interface SessionDirectory {
	readonly state: ReplicatedState<SessionDirectoryState>;
}

export const SessionDirectory = defineService<SessionDirectory>("pi.session-directory");

export interface SessionManagement {
	create(options: SessionCreateOptions, context: Context): Promise<SessionSummary>;
	remove(sessionId: string, context: Context): Promise<void>;
	rename(sessionId: string, name: string, context: Context): Promise<void>;
	attach(sessionId: string, context: Context): Promise<void>;
	detach(context: Context): Promise<void>;
}

export const SessionManagement = defineService<SessionManagement>("pi.session-management");
