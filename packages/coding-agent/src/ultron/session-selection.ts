import { open } from "node:fs/promises";
import { isAbsolute, resolve as resolvePath } from "node:path";
import { createInterface } from "node:readline";
import {
	BACKGROUND_CONTEXT,
	type Context,
	type FileSystem,
	type JsonlSessionMetadata,
	JsonlSessionRepo,
} from "@earendil-works/pi-agent-core";
import { NodeExecutionEnv } from "@earendil-works/pi-agent-core/node";

const JSONL_FORMAT_VERSION = 4;
const DEFAULT_MAX_HEADER_BYTES = 64 * 1024;

type SessionMatchKind = "id" | "prefix" | "path" | "continue" | "interactive";

export interface NativeSessionMatch {
	readonly metadata: JsonlSessionMetadata;
	readonly matchedBy: SessionMatchKind;
}

export type NativeSessionSelectionErrorCode = "ambiguous" | "invalid_header" | "not_found";

export class NativeSessionSelectionError extends Error {
	readonly code: NativeSessionSelectionErrorCode;
	readonly selector: string;
	readonly matches: readonly JsonlSessionMetadata[];

	constructor(
		code: NativeSessionSelectionErrorCode,
		selector: string,
		message: string,
		matches: readonly JsonlSessionMetadata[] = [],
	) {
		super(message);
		this.name = "NativeSessionSelectionError";
		this.code = code;
		this.selector = selector;
		this.matches = matches;
	}
}

export interface NativeSessionSelectionOptions {
	/** Session root. Relative paths are resolved without resolving symlinks. */
	readonly sessionDir: string;
	/** Working directory used for cwd-scoped discovery. Defaults to process.cwd(). */
	readonly cwd?: string;
	/** Injectable filesystem for deterministic native filesystem tests. */
	readonly fileSystem?: FileSystem;
	/** Injectable repository. It must use the supplied fileSystem when both are provided. */
	readonly repo?: JsonlSessionRepo;
	readonly context?: Context;
	/** Maximum UTF-8 bytes read from one explicit session header line. */
	readonly maxHeaderBytes?: number;
	readonly now?: () => number;
}

export interface InteractiveResumeChooserOptions {
	readonly input?: NodeJS.ReadableStream;
	readonly output?: NodeJS.WritableStream;
	readonly prompt?: string;
	readonly write?: (text: string) => void;
	/** Return undefined to cancel. Supplying this avoids process I/O in tests and callers with their own UI. */
	readonly readAnswer?: () => Promise<string | undefined>;
}

export interface NativeSessionSelectRequest extends InteractiveResumeChooserOptions {
	readonly selector?: string;
	readonly continue?: boolean;
	readonly resume?: boolean;
	readonly interactive?: boolean;
}

interface V4SessionHeader {
	readonly v: typeof JSONL_FORMAT_VERSION;
	readonly kind: "header";
	readonly id: string;
	readonly storageVersion: number;
	readonly createdAt: number;
	readonly cwd: string;
	readonly parentSessionId?: string;
	readonly legacyParentSessionPath?: string;
}

interface LegacyV3SessionHeader {
	readonly type: "session";
	readonly version: 3;
	readonly id: string;
	readonly timestamp: string;
	readonly cwd: string;
	readonly parentSession?: string;
}

function isRecord(value: unknown): value is Record<string, unknown> {
	return typeof value === "object" && value !== null && !Array.isArray(value);
}

function isNonNegativeSafeInteger(value: unknown): value is number {
	return Number.isSafeInteger(value) && (value as number) >= 0;
}

function isPositiveSafeInteger(value: unknown): value is number {
	return Number.isSafeInteger(value) && (value as number) >= 1;
}

function isV4SessionHeader(value: unknown): value is V4SessionHeader {
	return (
		isRecord(value) &&
		value.v === JSONL_FORMAT_VERSION &&
		value.kind === "header" &&
		typeof value.id === "string" &&
		isPositiveSafeInteger(value.storageVersion) &&
		isNonNegativeSafeInteger(value.createdAt) &&
		typeof value.cwd === "string" &&
		(value.parentSessionId === undefined || typeof value.parentSessionId === "string") &&
		(value.legacyParentSessionPath === undefined || typeof value.legacyParentSessionPath === "string")
	);
}

function isLegacyV3SessionHeader(value: unknown): value is LegacyV3SessionHeader {
	return (
		isRecord(value) &&
		value.type === "session" &&
		value.version === 3 &&
		typeof value.id === "string" &&
		typeof value.timestamp === "string" &&
		Number.isFinite(Date.parse(value.timestamp)) &&
		typeof value.cwd === "string" &&
		(value.parentSession === undefined || typeof value.parentSession === "string")
	);
}

function parseHeader(line: string): V4SessionHeader | LegacyV3SessionHeader {
	let value: unknown;
	try {
		value = JSON.parse(line);
	} catch (error) {
		throw new Error("Session header is not valid JSON", { cause: error });
	}
	if (isV4SessionHeader(value) || isLegacyV3SessionHeader(value)) return value;
	throw new Error("Session file has no supported native header");
}

async function readBoundedFirstLine(path: string, maxBytes: number): Promise<string> {
	const file = await open(path, "r");
	try {
		const pieces: Buffer[] = [];
		const chunk = Buffer.allocUnsafe(Math.min(8192, maxBytes + 1));
		let bytesRead = 0;
		while (bytesRead <= maxBytes) {
			const requested = Math.min(chunk.length, maxBytes + 1 - bytesRead);
			if (requested <= 0) break;
			const result = await file.read(chunk, 0, requested, bytesRead);
			if (result.bytesRead === 0) break;
			const part = chunk.subarray(0, result.bytesRead);
			const newline = part.indexOf(0x0a);
			if (newline !== -1) {
				if (bytesRead + newline > maxBytes) {
					throw new Error(`Session header exceeds ${maxBytes} bytes`);
				}
				pieces.push(Buffer.from(part.subarray(0, newline)));
				return Buffer.concat(pieces).toString("utf8");
			}
			pieces.push(Buffer.from(part));
			bytesRead += result.bytesRead;
		}
		throw new Error(`Session header exceeds ${maxBytes} bytes or is not newline-terminated`);
	} finally {
		await file.close();
	}
}

function metadataFromExplicitHeader(
	header: V4SessionHeader | LegacyV3SessionHeader,
	path: string,
	modifiedAt: number,
): JsonlSessionMetadata {
	if (isV4SessionHeader(header)) {
		return {
			id: header.id,
			createdAt: header.createdAt,
			storageVersion: header.storageVersion,
			cwd: header.cwd,
			path,
			modifiedAt,
			...(header.parentSessionId === undefined ? {} : { parentSessionId: header.parentSessionId }),
			...(header.legacyParentSessionPath === undefined
				? {}
				: { legacyParentSessionPath: header.legacyParentSessionPath }),
		};
	}
	return {
		id: header.id,
		createdAt: Date.parse(header.timestamp),
		storageVersion: 1,
		cwd: header.cwd,
		path,
		modifiedAt,
		...(header.parentSession === undefined ? {} : { legacyParentSessionPath: header.parentSession }),
	};
}

function isPathSelector(selector: string): boolean {
	return isAbsolute(selector) || selector.includes("/") || selector.includes("\\") || selector.endsWith(".jsonl");
}

function sortByModification(left: JsonlSessionMetadata, right: JsonlSessionMetadata): number {
	return (
		right.modifiedAt - left.modifiedAt ||
		right.createdAt - left.createdAt ||
		left.id.localeCompare(right.id) ||
		left.path.localeCompare(right.path)
	);
}

function sortById(left: JsonlSessionMetadata, right: JsonlSessionMetadata): number {
	return left.id.localeCompare(right.id) || left.path.localeCompare(right.path);
}

function fileSystemValue<T>(result: { ok: true; value: T } | { ok: false; error: Error }, action: string): T {
	if (!result.ok) throw new Error(`${action}: ${result.error.message}`, { cause: result.error });
	return result.value;
}

async function defaultReadAnswer(
	input: NodeJS.ReadableStream,
	output: NodeJS.WritableStream,
	prompt: string,
): Promise<string | undefined> {
	return await new Promise<string | undefined>((resolve) => {
		const readline = createInterface({ input, output });
		let settled = false;
		const finish = (answer: string | undefined): void => {
			if (settled) return;
			settled = true;
			readline.close();
			resolve(answer);
		};
		readline.once("close", () => finish(undefined));
		readline.once("SIGINT", () => finish(undefined));
		readline.question(prompt, (answer) => finish(answer));
	});
}

/**
 * Choose one already-discovered session. This function only returns a selection. It never creates or modifies a session.
 */
export async function chooseInteractiveResumeSession(
	sessions: readonly JsonlSessionMetadata[],
	options: InteractiveResumeChooserOptions = {},
): Promise<JsonlSessionMetadata | undefined> {
	if (sessions.length === 0) return undefined;
	if (sessions.length === 1) return sessions[0];

	const write =
		options.write ??
		((text: string) => {
			(options.output ?? process.stdout).write(text);
		});
	write("Available sessions:\n");
	for (const [index, session] of sessions.entries()) write(`  ${index + 1}. ${session.id}\n`);

	const answer = await (options.readAnswer?.() ??
		defaultReadAnswer(
			options.input ?? process.stdin,
			options.output ?? process.stdout,
			options.prompt ?? "Select a session number: ",
		));
	if (answer === undefined) return undefined;
	if (!/^\s*[0-9]+\s*$/.test(answer)) return undefined;
	const index = Number.parseInt(answer.trim(), 10) - 1;
	return Number.isSafeInteger(index) && index >= 0 && index < sessions.length ? sessions[index] : undefined;
}

/** Native session discovery and selection backed by the agent-core JSONL repository. */
export class NativeSessionSelector {
	readonly sessionDir: string;
	readonly cwd: string;
	readonly fileSystem: FileSystem;
	readonly repo: JsonlSessionRepo;
	readonly context: Context;
	readonly maxHeaderBytes: number;

	constructor(options: NativeSessionSelectionOptions) {
		const maxHeaderBytes = options.maxHeaderBytes ?? DEFAULT_MAX_HEADER_BYTES;
		if (!Number.isSafeInteger(maxHeaderBytes) || maxHeaderBytes <= 0) {
			throw new Error("maxHeaderBytes must be a positive safe integer");
		}
		this.sessionDir = resolvePath(options.sessionDir);
		this.cwd = resolvePath(options.cwd ?? process.cwd());
		this.fileSystem = options.fileSystem ?? new NodeExecutionEnv({ cwd: process.cwd() });
		if (options.repo !== undefined && options.fileSystem === undefined) {
			throw new Error("A custom repo requires its matching fileSystem");
		}
		this.repo =
			options.repo ??
			new JsonlSessionRepo({
				fileSystem: this.fileSystem,
				sessionsRoot: this.sessionDir,
				now: options.now,
			});
		this.context = options.context ?? BACKGROUND_CONTEXT;
		this.maxHeaderBytes = maxHeaderBytes;
	}

	/** List valid sessions whose stored cwd exactly matches this selector's cwd. */
	async discover(): Promise<JsonlSessionMetadata[]> {
		return await this.repo.list({ cwd: this.cwd }, this.context);
	}

	/** Resolve an exact id, a unique id prefix, or an explicit JSONL path. */
	async resolve(selector: string): Promise<NativeSessionMatch> {
		if (selector.length === 0) {
			throw new NativeSessionSelectionError("not_found", selector, "Session selector is empty");
		}
		if (isPathSelector(selector)) return { metadata: await this.readExplicitPath(selector), matchedBy: "path" };

		const sessions = await this.discover();
		const exact = sessions.filter((session) => session.id === selector).sort(sortById);
		if (exact.length === 1) return { metadata: exact[0]!, matchedBy: "id" };
		if (exact.length > 1) return this.ambiguous(selector, exact);

		const prefix = sessions.filter((session) => session.id.startsWith(selector)).sort(sortById);
		if (prefix.length === 1) return { metadata: prefix[0]!, matchedBy: "prefix" };
		if (prefix.length > 1) return this.ambiguous(selector, prefix);
		throw new NativeSessionSelectionError("not_found", selector, `No native session found matching '${selector}'`);
	}

	/** Return the cwd-scoped session whose file was modified most recently. */
	async continueSession(): Promise<NativeSessionMatch | undefined> {
		const sessions = [...(await this.discover())].sort(sortByModification);
		const metadata = sessions[0];
		return metadata === undefined ? undefined : { metadata, matchedBy: "continue" };
	}

	/** Resume the newest cwd-scoped session, optionally through the interactive chooser. */
	async resumeSession(
		options: InteractiveResumeChooserOptions & { interactive?: boolean } = {},
	): Promise<NativeSessionMatch | undefined> {
		const sessions = [...(await this.discover())].sort(sortByModification);
		if (!options.interactive) {
			const metadata = sessions[0];
			return metadata === undefined ? undefined : { metadata, matchedBy: "continue" };
		}
		const metadata = await chooseInteractiveResumeSession(sessions, options);
		return metadata === undefined ? undefined : { metadata, matchedBy: "interactive" };
	}

	/** Apply one native-command-style request without creating a session. */
	async select(request: NativeSessionSelectRequest): Promise<NativeSessionMatch | undefined> {
		if (request.selector !== undefined && (request.continue || request.resume)) {
			throw new Error("A session selector cannot be combined with continue or resume");
		}
		if (request.selector !== undefined) return await this.resolve(request.selector);
		if (request.continue) return await this.continueSession();
		if (request.resume) return await this.resumeSession(request);
		return undefined;
	}

	async close(): Promise<void> {
		await this.repo.close(this.context);
		await this.fileSystem.cleanup(this.context);
	}

	private ambiguous(selector: string, matches: readonly JsonlSessionMetadata[]): never {
		const ids = matches.map((session) => session.id).join(", ");
		throw new NativeSessionSelectionError(
			"ambiguous",
			selector,
			`Session selector '${selector}' is ambiguous; matches: ${ids}`,
			matches,
		);
	}

	private async readExplicitPath(selector: string): Promise<JsonlSessionMetadata> {
		const path = fileSystemValue(
			await this.fileSystem.absolutePath(selector, this.context),
			`Failed to resolve session path ${selector}`,
		);
		const info = await this.fileSystem.fileInfo(path, this.context);
		if (!info.ok) {
			throw new NativeSessionSelectionError("not_found", selector, `Session file does not exist: ${path}`);
		}
		try {
			const line = await readBoundedFirstLine(path, this.maxHeaderBytes);
			return metadataFromExplicitHeader(parseHeader(line), path, info.value.mtimeMs);
		} catch {
			throw new NativeSessionSelectionError(
				"invalid_header",
				selector,
				`Session file has no valid native header: ${path}`,
				[],
			);
		}
	}
}

export function createNativeSessionSelector(options: NativeSessionSelectionOptions): NativeSessionSelector {
	return new NativeSessionSelector(options);
}
