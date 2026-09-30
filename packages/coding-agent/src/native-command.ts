import { open, readdir, stat } from "node:fs/promises";
import { isAbsolute, resolve } from "node:path";
import { createInterface } from "node:readline";
import { type Args, normalizeSessionName } from "./cli/args.ts";
import type { ClientCommand } from "./cli/experimental/commands/client.ts";
import { processFileArguments } from "./cli/file-processor.ts";
import { runClient } from "./experimental/client.ts";
import { runClientTui } from "./experimental/client-tui.ts";
import { describeLostServer } from "./experimental/lost-server.ts";
import { runNativeRpcMode } from "./experimental/rpc-native.ts";
import { type RunningServer, resolveSessionDirectory, startForegroundServer } from "./experimental/server.ts";
import { traceStartup } from "./experimental/startup-trace.ts";
import { isLocalPath, resolvePath } from "./utils/paths.ts";

interface NativeSessionHeader {
	readonly id: string;
	readonly createdAt: number;
	readonly modifiedAt: number;
	readonly cwd: string;
	readonly path: string;
}

/** Read a session file's header line without loading the whole transcript. */
async function readFirstLine(path: string): Promise<string> {
	const handle = await open(path, "r");
	try {
		const chunks: Buffer[] = [];
		const buffer = Buffer.alloc(16 * 1024);
		let position = 0;
		while (true) {
			const { bytesRead } = await handle.read(buffer, 0, buffer.length, position);
			if (bytesRead === 0) break;
			const chunk = buffer.subarray(0, bytesRead);
			const newline = chunk.indexOf(0x0a);
			if (newline !== -1) {
				chunks.push(Buffer.from(chunk.subarray(0, newline)));
				break;
			}
			chunks.push(Buffer.from(chunk));
			position += bytesRead;
		}
		return Buffer.concat(chunks).toString("utf8");
	} finally {
		await handle.close();
	}
}

async function importStatMtime(path: string): Promise<number> {
	return (await stat(path)).mtimeMs;
}

async function listNativeSessions(sessionDir: string, cwd: string): Promise<NativeSessionHeader[]> {
	const sessions: NativeSessionHeader[] = [];
	let directories: string[];
	try {
		directories = (await readdir(sessionDir, { withFileTypes: true }))
			.filter((entry) => entry.isDirectory())
			.map((entry) => resolve(sessionDir, entry.name));
	} catch (error) {
		if (error instanceof Error && "code" in error && error.code === "ENOENT") return [];
		throw error;
	}
	for (const directory of directories) {
		for (const entry of await readdir(directory, { withFileTypes: true })) {
			if (!entry.isFile() || !entry.name.endsWith(".jsonl")) continue;
			const path = resolve(directory, entry.name);
			try {
				const firstLine = await readFirstLine(path);
				const value: unknown = JSON.parse(firstLine ?? "");
				if (
					typeof value !== "object" ||
					value === null ||
					!("kind" in value) ||
					value.kind !== "header" ||
					!("id" in value) ||
					typeof value.id !== "string" ||
					!("createdAt" in value) ||
					typeof value.createdAt !== "number" ||
					!("cwd" in value) ||
					typeof value.cwd !== "string"
				)
					continue;
				sessions.push({
					id: value.id,
					createdAt: value.createdAt,
					modifiedAt: await importStatMtime(path),
					cwd: value.cwd,
					path,
				});
			} catch {
				// Ignore unrelated or incomplete files during discovery.
			}
		}
	}
	return sessions
		.filter((session) => session.cwd === cwd)
		.sort(
			(left, right) =>
				right.modifiedAt - left.modifiedAt || right.createdAt - left.createdAt || left.id.localeCompare(right.id),
		);
}

async function sessionIdFromSelector(selector: string, sessions: readonly NativeSessionHeader[]): Promise<string> {
	if (selector.includes("/") || selector.includes("\\") || selector.endsWith(".jsonl")) {
		const path = resolve(selector);
		const firstLine = await readFirstLine(path);
		const value: unknown = JSON.parse(firstLine ?? "");
		if (
			typeof value !== "object" ||
			value === null ||
			!("kind" in value) ||
			value.kind !== "header" ||
			!("id" in value) ||
			typeof value.id !== "string"
		) {
			throw new Error(`Session file is not a valid ultron session: ${path}`);
		}
		return value.id;
	}
	const exact = sessions.find((session) => session.id === selector);
	if (exact) return exact.id;
	const prefix = sessions.filter((session) => session.id.startsWith(selector));
	if (prefix.length === 1) return prefix[0]!.id;
	throw new Error(`No native session found matching '${selector}'`);
}

async function selectInteractiveSession(sessions: readonly NativeSessionHeader[]): Promise<string | undefined> {
	if (sessions.length === 0) return undefined;
	if (sessions.length === 1) return sessions[0]!.id;
	process.stdout.write("Available sessions:\n");
	sessions.forEach((session, index) => {
		process.stdout.write(`  ${index + 1}. ${session.id}\n`);
	});
	return new Promise((resolveSelection) => {
		const readline = createInterface({ input: process.stdin, output: process.stdout });
		readline.question("Select a session number: ", (answer) => {
			readline.close();
			const index = Number.parseInt(answer.trim(), 10) - 1;
			resolveSelection(
				Number.isInteger(index) && index >= 0 && index < sessions.length ? sessions[index]!.id : undefined,
			);
		});
	});
}

async function selectedSessionId(parsed: Args, sessions: readonly NativeSessionHeader[]): Promise<string | undefined> {
	if (parsed.sessionId !== undefined) return parsed.sessionId;
	if (parsed.resume && process.stdin.isTTY && process.stdout.isTTY) return selectInteractiveSession(sessions);
	if (parsed.continue || parsed.resume) return sessions[0]?.id;
	if (parsed.session !== undefined) return sessionIdFromSelector(parsed.session, sessions);
	return undefined;
}

/** How long a signal waits for an aborted print-mode turn to end before the process exits anyway. */
const PROMPT_ABORT_WAIT_MS = 5_000;

export async function runNativeUltronCommand(parsed: Args, stdinContent: string | undefined): Promise<void> {
	if (parsed.diagnostics.some((diagnostic) => diagnostic.type === "error")) {
		throw new Error(
			parsed.diagnostics
				.filter((diagnostic) => diagnostic.type === "error")
				.map((diagnostic) => diagnostic.message)
				.join("\n"),
		);
	}
	if (parsed.fork && (parsed.continue || parsed.resume || parsed.session || parsed.sessionId)) {
		throw new Error("--fork cannot be combined with another session selector");
	}
	if (parsed.sessionDir !== undefined && !isAbsolute(parsed.sessionDir)) {
		throw new Error("--session-dir must be an absolute path for the native ultron command");
	}
	const name = parsed.name === undefined ? undefined : normalizeSessionName(parsed.name);
	if (parsed.name !== undefined && name === undefined) {
		throw new Error("--name requires a non-empty value");
	}
	const sessionDir = resolveSessionDirectory(parsed.sessionDir);
	traceStartup("cli.list-sessions");
	// Only session selectors consult the existing sessions; a fresh (or --no-session) start skips the scan.
	const needsSessions = parsed.continue || parsed.resume || parsed.session !== undefined || parsed.fork !== undefined;
	const sessions = needsSessions ? await listNativeSessions(sessionDir, process.cwd()) : [];
	if (parsed.fork !== undefined && parsed.noSession) {
		throw new Error("--fork cannot be combined with --no-session");
	}
	const sessionId = parsed.fork === undefined ? await selectedSessionId(parsed, sessions) : undefined;
	const forkSourceId = parsed.fork === undefined ? undefined : await sessionIdFromSelector(parsed.fork, sessions);
	const files = parsed.fileArgs.length === 0 ? { text: "", images: [] } : await processFileArguments(parsed.fileArgs);
	const promptParts = [...(stdinContent === undefined ? [] : [stdinContent]), files.text, ...parsed.messages].filter(
		Boolean,
	);
	const command: ClientCommand = {
		command: "client",
		// The foreground server receives model selection. Repeating it on the client
		// would incorrectly reject an already-running server.
		...(sessionId === undefined ? {} : { sessionId }),
		...(name === undefined ? {} : { name }),
		...(parsed.continue ? { continue: true } : {}),
		...(parsed.resume ? { resume: true } : {}),
		...(promptParts.length === 0 ? {} : { prompt: promptParts.join("\n\n") }),
		...(files.images.length === 0 ? {} : { images: files.images }),
	};
	let server: RunningServer | undefined;
	// A prompted (print or json) run is Pi's print mode: a signal aborts its turn before the process exits.
	const interrupt = new AbortController();
	let promptRun: Promise<unknown> | undefined;
	// An interrupted, killed, or hung-up client still releases its server: idle workers exit now and busy workers keep
	// their detached work running instead of waiting for the coordinator's orphan grace. Leaving the TUI or RPC mode
	// is not an abort; only a prompted run is aborted.
	const signalExitCodes: Partial<Record<NodeJS.Signals, number>> = { SIGINT: 130, SIGTERM: 143, SIGHUP: 129 };
	const onSignal = (signal: NodeJS.Signals): void => {
		for (const name of Object.keys(signalExitCodes)) process.off(name, onSignal);
		const exitCode = signalExitCodes[signal] ?? 1;
		const aborted =
			promptRun === undefined
				? Promise.resolve()
				: (() => {
						interrupt.abort();
						// Wait for the aborted turn to end, but never hang the exit on it.
						return Promise.race([
							promptRun.catch(() => {}),
							new Promise((resolveWait) => setTimeout(resolveWait, PROMPT_ABORT_WAIT_MS).unref()),
						]);
					})();
		void aborted
			.then(() => server?.close())
			.catch(() => {})
			.finally(() => process.exit(exitCode));
	};
	for (const name of Object.keys(signalExitCodes)) process.on(name, onSignal);
	const runsTui =
		parsed.mode !== "rpc" &&
		command.prompt === undefined &&
		!parsed.print &&
		process.stdin.isTTY === true &&
		process.stdout.isTTY === true;
	try {
		server = await startForegroundServer({
			model: parsed.model,
			provider: parsed.provider,
			sessionDir,
			...(parsed.apiKey === undefined ? {} : { apiKey: parsed.apiKey }),
			...(parsed.thinking === undefined ? {} : { thinking: parsed.thinking }),
			...(parsed.systemPrompt === undefined ? {} : { systemPrompt: parsed.systemPrompt }),
			...(parsed.noTools
				? { noTools: "all" as const }
				: parsed.noBuiltinTools
					? { noTools: "builtin" as const }
					: {}),
			...(parsed.tools === undefined ? {} : { tools: parsed.tools }),
			...(parsed.excludeTools === undefined ? {} : { excludeTools: parsed.excludeTools }),
			// Pi's -e/--extension and --no-extensions, resolved against this client's cwd as Pi does.
			...(parsed.extensions === undefined
				? {}
				: { extensionPaths: parsed.extensions.map((path) => (isLocalPath(path) ? resolvePath(path) : path)) }),
			...(parsed.noExtensions ? { noExtensions: true } : {}),
			// Pi's --no-skills / --no-context-files for this client's Session workers.
			...(parsed.noSkills ? { noSkills: true } : {}),
			...(parsed.noContextFiles ? { noContextFiles: true } : {}),
			// Pi's extension mode for this client: `ctx.mode`, and `ctx.hasUI` from the Session's start.
			extensionMode: parsed.mode === "rpc" ? "rpc" : runsTui ? "tui" : parsed.mode === "json" ? "json" : "print",
		});
		traceStartup("cli.server-started");
		// This process's own server generation, reached directly: other `ultron` processes run their own beside it.
		const route = { serverId: server.serverId, path: server.endpointPath };
		if (parsed.mode === "rpc") {
			await runNativeRpcMode({
				sessionDir,
				route,
				...(sessionId === undefined ? {} : { sessionId }),
				...(parsed.continue || parsed.resume ? { continue: true } : {}),
				...(forkSourceId === undefined ? {} : { forkFromSessionId: forkSourceId }),
				...(parsed.noSession ? { noSession: true } : {}),
				...(name === undefined ? {} : { name }),
			});
			return;
		}
		if (runsTui) {
			await runClientTui(forkSourceId === undefined ? command : { ...command, fork: forkSourceId }, {
				sessionDir,
				route,
			});
			return;
		}
		const run = runClient(command, {
			sessionDir,
			route,
			forkFromSessionId: forkSourceId,
			noSession: parsed.noSession,
			...(command.prompt === undefined ? {} : { interrupt: interrupt.signal, followEvents: true }),
			onEvent:
				parsed.mode === "json" || parsed.print
					? undefined
					: (event) => {
							if (event.type === "message_update" && event.frame?.type === "text_delta") {
								process.stdout.write(event.frame.delta);
							}
						},
		});
		if (command.prompt !== undefined) promptRun = run;
		const result = await run;
		if (interrupt.signal.aborted) return;
		if (result.kind === "prompted") {
			if (parsed.mode !== "json") process.stdout.write(`${parsed.print ? result.text : ""}\n`);
			else process.stdout.write(`${JSON.stringify(result)}\n`);
		} else if (result.kind === "attached") {
			process.stdout.write(`${result.serverId}\t${result.sessionId}\tattached\n`);
		} else {
			for (const session of result.sessions) process.stdout.write(`${session.serverId}\t${session.sessionId}\n`);
		}
	} catch (error) {
		// A lost server or worker surfaces as a transport-level error; say what happened and how to resume.
		const lost = describeLostServer(error);
		if (lost !== undefined) throw new Error(lost, { cause: error });
		throw error;
	} finally {
		for (const name of Object.keys(signalExitCodes)) process.off(name, onSignal);
		await server?.close();
	}
}
