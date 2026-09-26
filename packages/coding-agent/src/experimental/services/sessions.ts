import { type Context, defineService, type ReplicatedState } from "@ultron/chord";
import type { ServerId } from "@ultron/protocol";

export interface SessionAddress {
	serverId: ServerId;
	sessionId: string;
}

export interface SessionSummary extends SessionAddress {
	createdAt: number;
	modifiedAt: number;
	/** The Session file; reported by `SessionManagement.create` only. */
	sessionFile?: string;
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

/** A stored Session with what Pi's session selector shows: name, times, first message. */
export interface SessionListing extends SessionAddress {
	createdAt: number;
	modifiedAt: number;
	cwd: string;
	sessionFile: string;
	name: string | null;
	/** The first user message's text (empty when the Session has none). */
	firstMessage: string;
	messageCount: number;
	/** The Session this one was forked from, when recorded. */
	parentSessionId: string | null;
}

/** A Pi session file's contents, sent by a client for `importPi`. */
export interface PiSessionImport {
	/** Where the client read it (reported back; the server never opens it). */
	sourcePath: string;
	/** The Pi JSONL file's text. */
	content: string;
}

export interface PiSessionImportResult {
	session: SessionSummary;
	/** The Pi session was imported before; `session` is that native copy and nothing was written. */
	alreadyImported: boolean;
	imported: number;
}

export interface SessionManagement {
	/** Pi's session selector listing: this server's Sessions, of one cwd or (null) all, with names and first messages. */
	describe(options: { cwd: string | null }, context: Context): Promise<SessionListing[]>;
	/** Import a Pi session into a new native Session (the migration's `importPiSession`). */
	importPi(source: PiSessionImport, context: Context): Promise<PiSessionImportResult>;
	create(options: SessionCreateOptions, context: Context): Promise<SessionSummary>;
	remove(sessionId: string, context: Context): Promise<void>;
	rename(sessionId: string, name: string, context: Context): Promise<void>;
	attach(sessionId: string, context: Context): Promise<void>;
	detach(context: Context): Promise<void>;
}

export const SessionManagement = defineService<SessionManagement>("pi.session-management");
