/**
 * Explicit, repeatable migration between Pi and Ultron.
 *
 * - `importPiSession` copies the active branch of a Pi session into a new native Session.
 * - `exportNativeSessionToPi` writes a native Session's main branch back as a Pi session (rollback).
 * - `backupProfile` / `restoreProfile` snapshot and restore the Ultron config profile (never sessions).
 *
 * Nothing here reads Pi's data directory implicitly: every source path is supplied by the caller.
 */
import { createHash } from "node:crypto";
import { chmod, lstat, mkdir, readdir, readFile, readlink, rename, rm, symlink, writeFile } from "node:fs/promises";
import { dirname, isAbsolute, join, relative, resolve, sep } from "node:path";
import { isDeepStrictEqual } from "node:util";
import {
	type AgentMessage,
	BACKGROUND_CONTEXT,
	branchTip,
	type Entry,
	insertEntry,
	type JsonlSessionMetadata,
	JsonlSessionRepo,
	type JsonValue,
	type NewEntry,
	type Session,
	setValue,
	type Write,
} from "@earendil-works/pi-agent-core";
import { NodeExecutionEnv } from "@earendil-works/pi-agent-core/node";
import { createCustomMessage } from "../core/messages.ts";
import {
	CURRENT_SESSION_VERSION,
	type FileEntry,
	loadEntriesFromFile,
	type SessionEntry,
	type SessionHeader,
	SessionManager,
	sessionEntryToContextMessages,
} from "../core/session-manager.ts";

// =============================================================================
// Pi -> native import
// =============================================================================

export interface ImportPiSessionOptions {
	/** Pi JSONL session file (format v1-v3; older formats are migrated in memory, the file is never modified). */
	piSessionPath: string;
	/** Native sessions root (the directory `JsonlSessionRepo` stores `<cwd-dir>/<file>.jsonl` under). */
	sessionsRoot: string;
}

export interface ImportPiSessionResult {
	sessionId: string;
	path: string;
	/** Number of native entries written. */
	imported: number;
	/** Active-branch Pi entry types that have no native equivalent, with counts. */
	skipped: Array<{ type: string; count: number }>;
}

/** The Pi session was already imported; re-import is refused so the native copy is never duplicated. */
export class PiSessionAlreadyImportedError extends Error {
	readonly sessionId: string;
	readonly path: string;

	constructor(piSessionId: string, sessionId: string, path: string) {
		super(`Pi session ${piSessionId} was already imported as native session ${sessionId}: ${path}`);
		this.name = "PiSessionAlreadyImportedError";
		this.sessionId = sessionId;
		this.path = path;
	}
}

/**
 * Deterministic native session id for an imported Pi session: a UUID-shaped (version 8)
 * digest of the Pi session id, so the same Pi file always maps to the same native session.
 */
export function nativeSessionIdForPiSession(piSessionId: string): string {
	const bytes = createHash("sha256").update(`ultron:pi-import:${piSessionId}`).digest().subarray(0, 16);
	bytes[6] = (bytes[6]! & 0x0f) | 0x80;
	bytes[8] = (bytes[8]! & 0x3f) | 0x80;
	const hex = bytes.toString("hex");
	return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
}

interface PiSessionSnapshot {
	header: SessionHeader;
	branch: SessionEntry[];
	name: string | undefined;
}

function readPiSession(piSessionPath: string): PiSessionSnapshot {
	const fileEntries: FileEntry[] = loadEntriesFromFile(piSessionPath);
	const header = fileEntries[0];
	if (header === undefined || header.type !== "session") {
		throw new Error(`Not a Pi session file (missing session header): ${piSessionPath}`);
	}
	// In-memory manager: applies Pi's format migrations without rewriting the source file.
	const manager = SessionManager.inMemory(header.cwd, undefined, fileEntries);
	return { header, branch: manager.getBranch(), name: manager.getSessionName() };
}

/** Messages Pi would keep in context for a compaction: [firstKeptEntryId, compaction) on the branch. */
function piRetainedTail(
	branch: readonly SessionEntry[],
	compactionIndex: number,
	firstKeptEntryId: string,
): AgentMessage[] {
	const start = branch.findIndex((entry, index) => index < compactionIndex && entry.id === firstKeptEntryId);
	if (start < 0) return [];
	const tail: AgentMessage[] = [];
	for (const entry of branch.slice(start, compactionIndex)) {
		// Pi drops system messages and older compaction summaries from the kept range.
		if (entry.type === "compaction") continue;
		if (entry.type === "message" && entry.message.role === "system") continue;
		tail.push(...sessionEntryToContextMessages(entry));
	}
	return tail;
}

type NativeEntryBody = NewEntry extends infer T ? (T extends unknown ? Omit<T, "id" | "parentId"> : never) : never;

function toNativeEntryBody(
	entry: SessionEntry,
	branch: readonly SessionEntry[],
	index: number,
): NativeEntryBody | undefined {
	switch (entry.type) {
		case "message":
			return { type: "message", message: entry.message };
		case "custom_message":
			return {
				type: "message",
				message: createCustomMessage(
					entry.customType,
					entry.content,
					entry.display,
					entry.details,
					entry.timestamp,
				),
			};
		case "compaction":
			return {
				type: "compaction",
				summary: entry.summary,
				retainedTail: piRetainedTail(branch, index, entry.firstKeptEntryId),
				tokensBefore: entry.tokensBefore,
				...(entry.details === undefined ? {} : { details: entry.details as JsonValue }),
				...(entry.usage === undefined ? {} : { usage: entry.usage }),
				fromHook: entry.fromHook ?? false,
			};
		case "branch_summary":
			return {
				type: "branch_summary",
				// Pi encodes a root source as the "root" sentinel.
				fromId: entry.fromId === "root" ? null : entry.fromId,
				summary: entry.summary,
				...(entry.details === undefined ? {} : { details: entry.details as JsonValue }),
				...(entry.usage === undefined ? {} : { usage: entry.usage }),
				fromHook: entry.fromHook ?? false,
			};
		case "custom":
			return {
				type: "custom",
				customType: entry.customType,
				...(entry.data === undefined ? {} : { data: entry.data as JsonValue }),
			};
		default:
			return undefined;
	}
}

interface RepoHandle {
	repo: JsonlSessionRepo;
	fileSystem: NodeExecutionEnv;
	close(): Promise<void>;
}

function openRepo(sessionsRoot: string, now?: () => number): RepoHandle {
	const fileSystem = new NodeExecutionEnv({ cwd: process.cwd() });
	const repo = new JsonlSessionRepo({ fileSystem, sessionsRoot, ...(now === undefined ? {} : { now }) });
	return {
		repo,
		fileSystem,
		async close() {
			await repo.close(BACKGROUND_CONTEXT);
			await fileSystem.cleanup(BACKGROUND_CONTEXT);
		},
	};
}

function parseTimestamp(value: string | undefined, fallback: number): number {
	const parsed = value === undefined ? Number.NaN : Date.parse(value);
	return Number.isFinite(parsed) ? parsed : fallback;
}

/**
 * Import the active branch (root -> leaf) of a Pi session as a new native Session with the same cwd.
 * Entry ids and timestamps are preserved; the native main branch tip is the last imported entry.
 * Throws `PiSessionAlreadyImportedError` when the Pi session was imported before.
 */
export async function importPiSession(options: ImportPiSessionOptions): Promise<ImportPiSessionResult> {
	const piPath = resolve(options.piSessionPath);
	const pi = readPiSession(piPath);
	const sessionId = nativeSessionIdForPiSession(pi.header.id);

	// Storage stamps every commit with the repo clock; drive it from the Pi timestamps.
	let clock = parseTimestamp(pi.header.timestamp, Date.now());
	const handle = openRepo(options.sessionsRoot, () => clock);
	let session: Session<JsonlSessionMetadata> | undefined;
	let created = false;
	try {
		const existing = (await handle.repo.list(undefined, BACKGROUND_CONTEXT)).find((m) => m.id === sessionId);
		if (existing !== undefined) throw new PiSessionAlreadyImportedError(pi.header.id, sessionId, existing.path);

		session = await handle.repo.create({ id: sessionId, cwd: pi.header.cwd }, BACKGROUND_CONTEXT);
		created = true;
		const skipped = new Map<string, number>();
		let parentId: string | null = null;
		let imported = 0;
		for (const [index, entry] of pi.branch.entries()) {
			const body = toNativeEntryBody(entry, pi.branch, index);
			if (body === undefined) {
				if (entry.type !== "session_info") skipped.set(entry.type, (skipped.get(entry.type) ?? 0) + 1);
				continue;
			}
			clock = parseTimestamp(entry.timestamp, clock);
			const writes: Write[] = [
				insertEntry({ ...body, id: entry.id, parentId } as NewEntry),
				setValue(branchTip("main"), entry.id),
			];
			await session.mutate(async (mutator) => {
				await mutator.commit(writes, BACKGROUND_CONTEXT);
			}, BACKGROUND_CONTEXT);
			parentId = entry.id;
			imported++;
		}
		if (imported === 0) await session.createBranch("main", null, BACKGROUND_CONTEXT);
		if (pi.name !== undefined) await session.setName(pi.name, BACKGROUND_CONTEXT);
		return {
			sessionId,
			path: session.metadata.path,
			imported,
			skipped: [...skipped].map(([type, count]) => ({ type, count })),
		};
	} catch (error) {
		if (created && session !== undefined) {
			// Never leave a partial import behind: it would block a retry.
			const path = session.metadata.path;
			await session.close(BACKGROUND_CONTEXT).catch(() => undefined);
			session = undefined;
			await rm(path, { force: true });
		}
		throw error;
	} finally {
		await session?.close(BACKGROUND_CONTEXT);
		await handle.close();
	}
}

// =============================================================================
// native -> Pi export (rollback)
// =============================================================================

export interface ExportNativeSessionToPiOptions {
	/** Native session file, located at `<sessionsRoot>/<cwd-dir>/<file>.jsonl`. */
	sessionPath: string;
	/** Destination Pi JSONL file. Must not exist. */
	outputPath: string;
}

export interface ExportNativeSessionToPiResult {
	sessionId: string;
	path: string;
	/** Number of Pi entries written (excluding the header). */
	entries: number;
}

function iso(timestamp: number): string {
	return new Date(timestamp).toISOString();
}

/**
 * Pi keeps compaction tails by reference (`firstKeptEntryId`). Find the earliest preceding entry whose
 * projected context equals the native retained tail.
 */
function findFirstKeptEntryId(previous: readonly SessionEntry[], retainedTail: readonly AgentMessage[]): string | null {
	if (retainedTail.length === 0) return null;
	const collected: AgentMessage[] = [];
	for (let index = previous.length - 1; index >= 0; index--) {
		const entry = previous[index]!;
		if (entry.type === "compaction") continue;
		if (entry.type === "message" && entry.message.role === "system") continue;
		collected.unshift(...sessionEntryToContextMessages(entry));
		if (collected.length === retainedTail.length) {
			return isDeepStrictEqual(collected, retainedTail) ? entry.id : null;
		}
		if (collected.length > retainedTail.length) return null;
	}
	return null;
}

function toPiEntries(entries: readonly Entry[]): SessionEntry[] {
	const out: SessionEntry[] = [];
	// Map native ids to the Pi id that represents them (synthetic tail entries shift the chain).
	let lastId: string | null = null;
	const parentOf = (entry: Entry): string | null => (entry.parentId === null ? null : lastId);
	for (const entry of entries) {
		const base = { id: entry.id, parentId: parentOf(entry), timestamp: iso(entry.timestamp) };
		switch (entry.type) {
			case "message":
				out.push({ ...base, type: "message", message: entry.message });
				break;
			case "compaction": {
				const firstKept = findFirstKeptEntryId(out, entry.retainedTail);
				out.push({
					...base,
					type: "compaction",
					summary: entry.summary,
					// No kept tail (or it could not be matched): nothing before the compaction is kept.
					firstKeptEntryId: firstKept ?? entry.id,
					tokensBefore: entry.tokensBefore,
					...(entry.details === undefined ? {} : { details: entry.details }),
					...(entry.usage === undefined ? {} : { usage: entry.usage }),
					fromHook: entry.fromHook,
				});
				if (firstKept === null && entry.retainedTail.length > 0) {
					// Unmatched tail: replay it after the summary so Pi's model context is unchanged.
					let parentId = entry.id;
					for (const [index, message] of entry.retainedTail.entries()) {
						const id = `${entry.id}-tail-${index}`;
						out.push({ id, parentId, timestamp: base.timestamp, type: "message", message });
						parentId = id;
					}
					lastId = parentId;
					continue;
				}
				break;
			}
			case "branch_summary":
				out.push({
					...base,
					type: "branch_summary",
					fromId: entry.fromId ?? "root",
					summary: entry.summary,
					...(entry.details === undefined ? {} : { details: entry.details }),
					...(entry.usage === undefined ? {} : { usage: entry.usage }),
					fromHook: entry.fromHook,
				});
				break;
			case "custom":
				out.push({
					...base,
					type: "custom",
					customType: entry.customType,
					...(entry.data === undefined ? {} : { data: entry.data }),
				});
				break;
		}
		lastId = entry.id;
	}
	return out;
}

/** Write the main branch of a native Session as a Pi-format JSONL session (for rollback to Pi). */
export async function exportNativeSessionToPi(
	options: ExportNativeSessionToPiOptions,
): Promise<ExportNativeSessionToPiResult> {
	const path = resolve(options.sessionPath);
	const handle = openRepo(dirname(dirname(path)));
	let session: Session<JsonlSessionMetadata> | undefined;
	try {
		const metadata = (await handle.repo.list(undefined, BACKGROUND_CONTEXT)).find((m) => m.path === path);
		if (metadata === undefined) throw new Error(`Not a native session in its session directory: ${path}`);
		session = await handle.repo.open(metadata, BACKGROUND_CONTEXT);
		const main = await session.branch("main", BACKGROUND_CONTEXT);
		const nativeEntries =
			main === undefined ? [] : await main.findEntries({ order: "oldestFirst" }, BACKGROUND_CONTEXT);
		const entries = toPiEntries(nativeEntries);
		const stamp = iso(nativeEntries.at(-1)?.timestamp ?? metadata.createdAt);
		const append = (entry: Record<string, unknown> & { type: string; id: string }): void => {
			entries.push({ ...entry, parentId: entries.at(-1)?.id ?? null, timestamp: stamp } as SessionEntry);
		};
		for (const entry of nativeEntries) {
			const label = await session.getLabel(entry.id, BACKGROUND_CONTEXT);
			if (label !== undefined) {
				append({ type: "label", id: `label-${entry.id}`, targetId: entry.id, label });
			}
		}
		const name = await session.getName(BACKGROUND_CONTEXT);
		if (name !== undefined) append({ type: "session_info", id: `name-${metadata.id}`, name });

		const header: SessionHeader = {
			type: "session",
			version: CURRENT_SESSION_VERSION,
			id: metadata.id,
			timestamp: iso(metadata.createdAt),
			cwd: metadata.cwd,
		};
		const output = resolve(options.outputPath);
		await mkdir(dirname(output), { recursive: true });
		const content = `${[header, ...entries].map((line) => JSON.stringify(line)).join("\n")}\n`;
		await writeFile(output, content, { encoding: "utf8", flag: "wx", mode: 0o600 });
		return { sessionId: metadata.id, path: output, entries: entries.length };
	} finally {
		await session?.close(BACKGROUND_CONTEXT);
		await handle.close();
	}
}

// =============================================================================
// Profile backup / restore
// =============================================================================

export const PROFILE_BACKUP_FORMAT = "ultron-profile-backup";
export const PROFILE_BACKUP_VERSION = 1;
const MANIFEST_FILE = "manifest.json";
const FILES_DIR = "files";

/** Top-level profile entries that are data, caches, or regenerable installs rather than configuration. */
const EXCLUDED_TOP_LEVEL = new Set(["sessions", "traces", "bin", "npm", "node_modules", "backups"]);

export interface ProfileManifestFile {
	path: string;
	type: "file";
	size: number;
	sha256: string;
	mode: number;
}

export interface ProfileManifestSymlink {
	path: string;
	type: "symlink";
	target: string;
}

export interface ProfileManifest {
	format: typeof PROFILE_BACKUP_FORMAT;
	version: typeof PROFILE_BACKUP_VERSION;
	createdAt: string;
	sourceAgentDir: string;
	entries: Array<ProfileManifestFile | ProfileManifestSymlink>;
}

export interface BackupProfileOptions {
	agentDir: string;
	/** Parent directory; the backup is written to a new timestamped directory inside it. */
	backupDir: string;
}

export interface BackupProfileResult {
	path: string;
	manifest: ProfileManifest;
}

export interface RestoreProfileOptions {
	backupPath: string;
	agentDir: string;
}

export interface RestoreProfileResult {
	restored: number;
}

/** The backup failed verification; nothing was written to the profile. */
export class ProfileBackupVerificationError extends Error {
	readonly problems: string[];

	constructor(problems: string[]) {
		super(`Profile backup failed verification:\n${problems.map((problem) => `  - ${problem}`).join("\n")}`);
		this.name = "ProfileBackupVerificationError";
		this.problems = problems;
	}
}

function isExcluded(relativePath: string): boolean {
	const segments = relativePath.split("/");
	if (segments.length === 1 && EXCLUDED_TOP_LEVEL.has(segments[0]!)) return true;
	const name = segments.at(-1)!;
	// Session stores anywhere in the profile (sessions/, experimental/sessions/, *-sessions/).
	return name === "sessions" || name.endsWith("-sessions") || name.endsWith(".lock");
}

function isInside(parent: string, child: string): boolean {
	const rel = relative(parent, child);
	return rel === "" || (!rel.startsWith("..") && !isAbsolute(rel));
}

async function collectProfile(
	agentDir: string,
	skipDir: string | undefined,
): Promise<Array<{ path: string; kind: "file" | "symlink" }>> {
	const out: Array<{ path: string; kind: "file" | "symlink" }> = [];
	const walk = async (relativeDir: string): Promise<void> => {
		const absoluteDir = join(agentDir, relativeDir);
		const names = (await readdir(absoluteDir)).sort();
		for (const name of names) {
			const rel = relativeDir === "" ? name : `${relativeDir}/${name}`;
			const absolute = join(agentDir, rel);
			if (isExcluded(rel) || (skipDir !== undefined && isInside(skipDir, absolute))) continue;
			const stats = await lstat(absolute);
			if (stats.isSymbolicLink()) out.push({ path: rel, kind: "symlink" });
			else if (stats.isDirectory()) await walk(rel);
			else if (stats.isFile()) out.push({ path: rel, kind: "file" });
			// Sockets, FIFOs, and devices are not configuration.
		}
	};
	await walk("");
	return out;
}

function sha256(data: Buffer): string {
	return createHash("sha256").update(data).digest("hex");
}

async function mkdirPrivate(path: string): Promise<void> {
	await mkdir(path, { recursive: true, mode: 0o700 });
	await chmod(path, 0o700);
}

async function createUniqueDirectory(parent: string, base: string): Promise<string> {
	for (let attempt = 0; attempt < 100; attempt++) {
		const candidate = join(parent, attempt === 0 ? base : `${base}-${attempt}`);
		try {
			await mkdir(candidate, { mode: 0o700 });
			await chmod(candidate, 0o700);
			return candidate;
		} catch (error) {
			if ((error as NodeJS.ErrnoException).code !== "EEXIST") throw error;
		}
	}
	throw new Error(`Could not create a unique backup directory under ${parent}`);
}

/**
 * Copy the Ultron config profile (everything under agentDir except sessions, traces, caches of
 * installed binaries/packages, and lock files) into `<backupDir>/ultron-profile-<timestamp>/`.
 * Directories are 0700 and files 0600; `manifest.json` records a sha256 per file.
 */
export async function backupProfile(options: BackupProfileOptions): Promise<BackupProfileResult> {
	const agentDir = resolve(options.agentDir);
	const backupDir = resolve(options.backupDir);
	if ((await lstat(agentDir).catch(() => undefined))?.isDirectory() !== true) {
		throw new Error(`Profile directory does not exist: ${agentDir}`);
	}
	// Never tighten a caller-supplied parent (it may be $HOME); the backup directory itself is 0700.
	await mkdir(backupDir, { recursive: true, mode: 0o700 });
	const createdAt = new Date();
	const target = await createUniqueDirectory(
		backupDir,
		`ultron-profile-${createdAt.toISOString().replace(/[:.]/g, "-")}`,
	);
	try {
		const filesRoot = join(target, FILES_DIR);
		await mkdirPrivate(filesRoot);
		const manifest: ProfileManifest = {
			format: PROFILE_BACKUP_FORMAT,
			version: PROFILE_BACKUP_VERSION,
			createdAt: createdAt.toISOString(),
			sourceAgentDir: agentDir,
			entries: [],
		};
		for (const item of await collectProfile(agentDir, isInside(agentDir, backupDir) ? backupDir : undefined)) {
			const source = join(agentDir, item.path);
			const destination = join(filesRoot, item.path);
			await mkdirPrivateTree(filesRoot, dirname(destination));
			if (item.kind === "symlink") {
				const linkTarget = await readlink(source);
				await symlink(linkTarget, destination);
				manifest.entries.push({ path: item.path, type: "symlink", target: linkTarget });
				continue;
			}
			const data = await readFile(source);
			const mode = (await lstat(source)).mode & 0o777;
			await writeFile(destination, data, { mode: 0o600, flag: "wx" });
			await chmod(destination, 0o600);
			manifest.entries.push({ path: item.path, type: "file", size: data.length, sha256: sha256(data), mode });
		}
		await writeFile(join(target, MANIFEST_FILE), `${JSON.stringify(manifest, null, 2)}\n`, {
			mode: 0o600,
			flag: "wx",
		});
		return { path: target, manifest };
	} catch (error) {
		await rm(target, { recursive: true, force: true });
		throw error;
	}
}

async function mkdirPrivateTree(root: string, directory: string): Promise<void> {
	const rel = relative(root, directory);
	if (rel === "") return;
	let current = root;
	for (const segment of rel.split(sep)) {
		current = join(current, segment);
		await mkdir(current, { mode: 0o700 }).catch((error: NodeJS.ErrnoException) => {
			if (error.code !== "EEXIST") throw error;
		});
		await chmod(current, 0o700);
	}
}

function isSafeRelativePath(path: unknown): path is string {
	if (typeof path !== "string" || path.length === 0 || path.includes("\\") || path.includes("\0")) return false;
	if (isAbsolute(path)) return false;
	return path.split("/").every((segment) => segment !== "" && segment !== "." && segment !== "..");
}

async function listBackupFiles(root: string, relativeDir = ""): Promise<string[]> {
	const out: string[] = [];
	for (const entry of await readdir(join(root, relativeDir), { withFileTypes: true })) {
		const rel = relativeDir === "" ? entry.name : `${relativeDir}/${entry.name}`;
		if (entry.isDirectory()) out.push(...(await listBackupFiles(root, rel)));
		else out.push(rel);
	}
	return out;
}

/** Verify a backup against its manifest without touching any profile. Returns the manifest. */
export async function verifyProfileBackup(backupPath: string): Promise<ProfileManifest> {
	const root = resolve(backupPath);
	let manifest: ProfileManifest;
	try {
		manifest = JSON.parse(await readFile(join(root, MANIFEST_FILE), "utf8")) as ProfileManifest;
	} catch (error) {
		throw new ProfileBackupVerificationError([`unreadable manifest: ${(error as Error).message}`]);
	}
	if (manifest?.format !== PROFILE_BACKUP_FORMAT || manifest.version !== PROFILE_BACKUP_VERSION) {
		throw new ProfileBackupVerificationError(["unsupported manifest format or version"]);
	}
	if (!Array.isArray(manifest.entries)) throw new ProfileBackupVerificationError(["manifest has no entries list"]);
	const problems: string[] = [];
	const filesRoot = join(root, FILES_DIR);
	const expected = new Set<string>();
	for (const entry of manifest.entries) {
		if (!isSafeRelativePath(entry?.path)) {
			problems.push(`unsafe path in manifest: ${JSON.stringify(entry?.path)}`);
			continue;
		}
		expected.add(entry.path);
		const absolute = join(filesRoot, entry.path);
		const stats = await lstat(absolute).catch(() => undefined);
		if (stats === undefined) {
			problems.push(`missing: ${entry.path}`);
		} else if (entry.type === "symlink") {
			if (!stats.isSymbolicLink() || (await readlink(absolute)) !== entry.target) {
				problems.push(`symlink changed: ${entry.path}`);
			}
		} else if (entry.type === "file") {
			if (!stats.isFile()) problems.push(`not a regular file: ${entry.path}`);
			else if (sha256(await readFile(absolute)) !== entry.sha256 || stats.size !== entry.size) {
				problems.push(`hash mismatch: ${entry.path}`);
			}
		} else {
			problems.push(`unknown entry type for ${(entry as { path: string }).path}`);
		}
	}
	const present = await listBackupFiles(filesRoot).catch(() => [] as string[]);
	for (const path of present) if (!expected.has(path)) problems.push(`not in manifest: ${path}`);
	if (problems.length > 0) throw new ProfileBackupVerificationError(problems);
	return manifest;
}

/**
 * Restore a profile backup into agentDir. Every file is verified against the manifest before
 * anything is written; a mismatch refuses the whole restore. Files outside the backup are left alone.
 */
export async function restoreProfile(options: RestoreProfileOptions): Promise<RestoreProfileResult> {
	const root = resolve(options.backupPath);
	const agentDir = resolve(options.agentDir);
	const manifest = await verifyProfileBackup(root);
	const filesRoot = join(root, FILES_DIR);
	await mkdir(agentDir, { recursive: true, mode: 0o700 });
	let restored = 0;
	for (const entry of manifest.entries) {
		const destination = join(agentDir, entry.path);
		await mkdir(dirname(destination), { recursive: true, mode: 0o700 });
		const temporary = `${destination}.ultron-restore-${process.pid}`;
		await rm(temporary, { force: true });
		if (entry.type === "symlink") {
			await symlink(entry.target, temporary);
		} else {
			const data = await readFile(join(filesRoot, entry.path));
			// Re-check at write time so a file swapped after verification is still refused.
			if (sha256(data) !== entry.sha256) {
				throw new ProfileBackupVerificationError([`hash mismatch: ${entry.path}`]);
			}
			await writeFile(temporary, data, { mode: 0o600, flag: "wx" });
			await chmod(temporary, entry.mode & 0o777);
		}
		const existing = await lstat(destination).catch(() => undefined);
		if (existing?.isDirectory()) {
			await rm(temporary, { force: true });
			throw new Error(`Cannot restore ${entry.path}: a directory is in the way`);
		}
		await rename(temporary, destination);
		restored++;
	}
	return { restored };
}
