import { readdir, readFile, stat } from "node:fs/promises";
import { isAbsolute, resolve } from "node:path";
import { createInterface } from "node:readline";
import { type Args, normalizeSessionName } from "./cli/args.ts";
import type { ClientCommand } from "./cli/experimental/commands/client.ts";
import { processFileArguments } from "./cli/file-processor.ts";
import { runClient } from "./experimental/client.ts";
import { runClientTui } from "./experimental/client-tui.ts";
import { runNativeRpcMode } from "./experimental/rpc-native.ts";
import { type RunningServer, resolveSessionDirectory, startForegroundServer } from "./experimental/server.ts";

interface NativeSessionHeader {
	readonly id: string;
	readonly createdAt: number;
	readonly modifiedAt: number;
	readonly cwd: string;
	readonly path: string;
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
				const firstLine = (await readFile(path, "utf8")).split("\n", 1)[0];
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
		const firstLine = (await readFile(path, "utf8")).split("\n", 1)[0];
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
	const sessions = await listNativeSessions(sessionDir, process.cwd());
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
		});
		if (parsed.mode === "rpc") {
			await runNativeRpcMode({
				sessionDir,
				...(sessionId === undefined ? {} : { sessionId }),
				...(parsed.continue || parsed.resume ? { continue: true } : {}),
				...(forkSourceId === undefined ? {} : { forkFromSessionId: forkSourceId }),
				...(parsed.noSession ? { noSession: true } : {}),
				...(name === undefined ? {} : { name }),
			});
			return;
		}
		if (command.prompt === undefined && !parsed.print && process.stdin.isTTY && process.stdout.isTTY) {
			await runClientTui(command, { sessionDir });
			return;
		}
		const result = await runClient(command, {
			sessionDir,
			forkFromSessionId: forkSourceId,
			noSession: parsed.noSession,
			onEvent:
				parsed.mode === "json" || parsed.print
					? undefined
					: (event) => {
							if (event.type === "message_update" && event.frame?.type === "text_delta") {
								process.stdout.write(event.frame.delta);
							}
						},
		});
		if (result.kind === "prompted") {
			if (parsed.mode !== "json") process.stdout.write(`${parsed.print ? result.text : ""}\n`);
			else process.stdout.write(`${JSON.stringify(result)}\n`);
		} else if (result.kind === "attached") {
			process.stdout.write(`${result.serverId}\t${result.sessionId}\tattached\n`);
		} else {
			for (const session of result.sessions) process.stdout.write(`${session.serverId}\t${session.sessionId}\n`);
		}
	} finally {
		await server?.close();
	}
}
