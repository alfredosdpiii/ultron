/**
 * Pi's `/resume` and `/import` in the native TUI. `/resume` lists the server's native Sessions (this project's, or
 * all with Tab) in Pi's session selector and switches the TUI to the pick, as `/fork` and `/new` do. `/import`
 * copies a Pi session file into a new native Session with the migration's `importPiSession` (on the server) and
 * offers to switch to it; without a path it picks from this project's Pi sessions, read-only.
 */

import { readFile } from "node:fs/promises";
import { homedir } from "node:os";
import { join, resolve } from "node:path";
import { BACKGROUND_CONTEXT } from "@ultron/chord/context";
import { type SessionInfo, SessionManager } from "../core/session-manager.ts";
import { SessionSelectorComponent } from "../modes/interactive/components/session-selector.ts";
import { resolvePath } from "../utils/paths.ts";
import { pathArgument } from "./client-tui-commands.ts";
import type { PiCommandHost } from "./client-tui-pi-commands.ts";
import type { PiSessionImportResult, SessionListing } from "./services/sessions.ts";

/** Pi's profile directory (read-only here): `PI_CODING_AGENT_DIR`, else `~/.pi/agent`. */
export function piAgentDir(env: NodeJS.ProcessEnv = process.env): string {
	return resolvePath(env.PI_CODING_AGENT_DIR ?? join(homedir(), ".pi", "agent"));
}

/** Pi's per-project session directory under `<piAgentDir>/sessions`. */
export function piSessionDirForCwd(cwd: string, agentDir: string): string {
	const resolved = resolvePath(cwd);
	return join(agentDir, "sessions", `--${resolved.replace(/^[/\\]/u, "").replace(/[/\\:]/gu, "-")}--`);
}

/** A native Session as Pi's selector shows a session; `path` is the Session file (or its id without one). */
export function listingToSessionInfo(listing: SessionListing): SessionInfo {
	return {
		path: listing.sessionFile || listing.sessionId,
		id: listing.sessionId,
		cwd: listing.cwd,
		...(listing.name ? { name: listing.name } : {}),
		created: new Date(listing.createdAt),
		modified: new Date(listing.modifiedAt),
		messageCount: listing.messageCount,
		firstMessage: listing.firstMessage || "(no messages)",
		allMessagesText: listing.firstMessage,
	};
}

/** Pi's `/resume`: the session selector over the server's native Sessions. */
export async function showResumeSelector(host: PiCommandHost): Promise<void> {
	const byPath = new Map<string, string>();
	const load = (cwd: string | null) => async (): Promise<SessionInfo[]> => {
		const listings = await host.withManagement((management) => management.describe({ cwd }, BACKGROUND_CONTEXT));
		const infos = listings
			.map((listing) => {
				const info = listingToSessionInfo(listing);
				byPath.set(info.path, listing.sessionId);
				return info;
			})
			.sort((left, right) => right.modified.getTime() - left.modified.getTime());
		return infos;
	};
	// The first listing marks the current Session (Pi highlights it and refuses to delete it).
	let initial: SessionInfo[] | undefined;
	try {
		initial = await load(process.cwd())();
	} catch (error) {
		host.showStatus(`Error: ${error instanceof Error ? error.message : String(error)}`);
		return;
	}
	const currentFile = [...byPath].find(([, sessionId]) => sessionId === host.sessionId())?.[0];
	let close = (): void => {};
	const selector = new SessionSelectorComponent(
		async () => {
			const first = initial;
			initial = undefined;
			return first ?? load(process.cwd())();
		},
		load(null),
		(path) => {
			close();
			const sessionId = byPath.get(path);
			if (sessionId === undefined) return;
			if (sessionId === host.sessionId()) {
				host.showStatus("Already in this session");
				return;
			}
			void host
				.switchSession(sessionId)
				.then(() => host.showStatus("Resumed session"))
				.catch((error: unknown) => host.showStatus(`Error: ${error instanceof Error ? error.message : error}`));
		},
		() => {
			close();
			host.requestRender();
		},
		() => host.quit(),
		() => host.requestRender(),
		{
			renameSession: async (path, name) => {
				const next = (name ?? "").trim();
				const sessionId = byPath.get(path);
				if (!next || sessionId === undefined) return;
				if (sessionId === host.sessionId()) {
					await host.control()?.setName(next, BACKGROUND_CONTEXT);
					host.setSessionName(next);
					return;
				}
				await host.withManagement((management) => management.rename(sessionId, next, BACKGROUND_CONTEXT));
			},
			showRenameHint: true,
			keybindings: host.keybindings,
		},
		currentFile,
	);
	// Deleting goes through the server, which closes the Session's worker and its plugin profile first.
	const list = selector.getSessionList();
	const deleteFile = list.onDeleteSession;
	list.onDeleteSession = async (path) => {
		const sessionId = byPath.get(path);
		if (sessionId === host.sessionId()) {
			list.onError?.("Cannot delete the currently active session");
			return;
		}
		if (sessionId !== undefined) {
			try {
				await host.withManagement((management) => management.remove(sessionId, BACKGROUND_CONTEXT));
			} catch (error) {
				list.onError?.(`Failed to delete: ${error instanceof Error ? error.message : String(error)}`);
				return;
			}
		}
		// Pi's handler then drops it from the list (the file is already gone, which it treats as deleted).
		await deleteFile?.(path);
	};
	close = host.showComponent(selector, selector);
}

/** Pi's `/import [path]`. */
export async function handleImport(host: PiCommandHost, args: string): Promise<void> {
	const path = pathArgument(args);
	if (path !== undefined) {
		await importPiSessionFile(host, resolve(process.cwd(), path));
		return;
	}
	showPiSessionPicker(host);
}

/** A picker of Pi's sessions for this project (Tab: all projects). Pi's files are only read. */
function showPiSessionPicker(host: PiCommandHost): void {
	const agentDir = host.piAgentDir;
	let close = (): void => {};
	const selector = new SessionSelectorComponent(
		(onProgress, signal) =>
			SessionManager.list(process.cwd(), piSessionDirForCwd(process.cwd(), agentDir), onProgress, signal),
		(onProgress, signal) => SessionManager.listAll(join(agentDir, "sessions"), onProgress, signal),
		(path) => {
			close();
			void importPiSessionFile(host, path);
		},
		() => {
			close();
			host.requestRender();
		},
		() => host.quit(),
		() => host.requestRender(),
		{ showRenameHint: false, keybindings: host.keybindings },
	);
	const list = selector.getSessionList();
	list.onDeleteSession = async () => {
		list.onError?.("Pi sessions are read-only here");
	};
	close = host.showComponent(selector, selector);
}

async function importPiSessionFile(host: PiCommandHost, path: string): Promise<void> {
	let content: string;
	try {
		content = await readFile(path, "utf8");
	} catch (error) {
		host.showStatus(`Error: Failed to import session: ${error instanceof Error ? error.message : String(error)}`);
		return;
	}
	host.showStatus(`Importing ${path}…`);
	let result: PiSessionImportResult;
	try {
		result = await host.withManagement((management) =>
			management.importPi({ sourcePath: path, content }, BACKGROUND_CONTEXT),
		);
	} catch (error) {
		host.showStatus(`Error: Failed to import session: ${error instanceof Error ? error.message : String(error)}`);
		return;
	}
	const sessionId = result.session.sessionId;
	const imported = result.alreadyImported
		? `Already imported as session ${sessionId}`
		: `Imported ${result.imported} entries from ${path} as session ${sessionId}`;
	if (sessionId === host.sessionId()) {
		host.showStatus(`${imported} (the current session)`);
		return;
	}
	const choice = await host.select(`${imported}. Switch to it?`, ["Switch to the imported session", "Stay here"]);
	if (choice !== "Switch to the imported session") {
		host.showStatus(`${imported}. Use /resume to open it.`);
		return;
	}
	try {
		await host.switchSession(sessionId);
		host.showStatus(`Session imported from: ${path}`);
	} catch (error) {
		host.showStatus(`Error: ${error instanceof Error ? error.message : String(error)}`);
	}
}
