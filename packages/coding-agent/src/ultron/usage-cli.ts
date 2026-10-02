/**
 * `ultron usage [session-id|path] [--last N] [--json]`: the session report (session-report.ts) of a session file,
 * or a table of the most recent sessions. It only reads session files: no model call, no server, and nothing is
 * written, so it is safe on a session another process is running.
 */
import { existsSync, readFileSync, statSync } from "node:fs";
import { readdir, stat } from "node:fs/promises";
import { join, resolve } from "node:path";
import { APP_NAME, ENV_SESSION_DIR, getAgentDir } from "../config.ts";
import { resolvePath } from "../utils/paths.ts";
import { readSessionHeader, readSessionLog, SessionLogError } from "./session-log.ts";
import { buildSessionReport, type SessionReport, sessionIsEmpty } from "./session-report.ts";
import { renderSessionReport, renderSessionTable } from "./session-report-text.ts";

export const SESSION_REPORT_LIST_SCHEMA = "ultron.session-report-list/1";

export interface UsageCommandIo {
	stdout(line: string): void;
	stderr(line: string): void;
}

export interface UsageCommandEnvironment {
	cwd?: string;
	env?: NodeJS.ProcessEnv;
	/** The profile directory (for the sessions root and Claude Code's session map); default `getAgentDir()`. */
	agentDir?: string;
	/** The home directory printed as `~`; default the user's. */
	home?: string;
}

const defaultIo: UsageCommandIo = {
	stdout: (line) => process.stdout.write(`${line}\n`),
	stderr: (line) => process.stderr.write(`${line}\n`),
};

class UsageError extends Error {}

function usage(): string {
	return [
		`Usage: ${APP_NAME} usage [session-id | session.jsonl] [options]`,
		"",
		"What a session did: turns, cells, depth (frames, sub-agents, workflows), tokens and cost per lane and model,",
		"guardrails and memory decisions. Reads session files only: no model call, no server, nothing written.",
		"",
		"With no session, reports the most recent session of the current directory.",
		"",
		"Options:",
		"  --last <N>              A table of the N most recent sessions, across all directories",
		"  --here                  With --last: only sessions of the current directory",
		"  --all                   With --last: include sessions in which nothing ran",
		"  --json                  The report as JSON (schema ultron.session-report/1; with --last, a list)",
		"  --utc                   Print times in UTC",
		"  --sessions-root <dir>   Where sessions are kept (default: the profile's sessions directory)",
	].join("\n");
}

type Options = {
	target?: string;
	last?: number;
	here: boolean;
	all: boolean;
	json: boolean;
	utc: boolean;
	sessionsRoot?: string;
};

function parse(args: readonly string[]): Options {
	const options: Options = { here: false, all: false, json: false, utc: false };
	for (let index = 0; index < args.length; index += 1) {
		const arg = args[index]!;
		const value = (name: string): string => {
			const inline = arg.includes("=") ? arg.slice(arg.indexOf("=") + 1) : args[++index];
			if (inline === undefined || inline === "") throw new UsageError(`Option ${name} requires a value`);
			return inline;
		};
		if (arg === "--json") options.json = true;
		else if (arg === "--here") options.here = true;
		else if (arg === "--all") options.all = true;
		else if (arg === "--utc") options.utc = true;
		else if (arg === "--last" || arg.startsWith("--last=")) {
			const count = Number(value("--last"));
			if (!Number.isSafeInteger(count) || count < 1 || count > 1000)
				throw new UsageError("--last needs a number between 1 and 1000");
			options.last = count;
		} else if (arg === "--sessions-root" || arg.startsWith("--sessions-root="))
			options.sessionsRoot = value("--sessions-root");
		else if (arg.startsWith("-")) throw new UsageError(`Unknown option ${arg}`);
		else if (options.target !== undefined) throw new UsageError(`Unexpected argument: ${arg}`);
		else options.target = arg;
	}
	if (options.target !== undefined && options.last !== undefined)
		throw new UsageError("Give a session or --last, not both");
	if ((options.here || options.all) && options.last === undefined)
		throw new UsageError("--here and --all go with --last");
	return options;
}

type SessionFile = { path: string; modifiedAt: number };

/** Every session file under the sessions root (one directory per working directory), newest first. */
async function listSessionFiles(root: string): Promise<SessionFile[]> {
	const files: SessionFile[] = [];
	let directories: string[];
	try {
		directories = (await readdir(root, { withFileTypes: true }))
			.filter((entry) => entry.isDirectory())
			.map((entry) => join(root, entry.name));
	} catch {
		return files;
	}
	for (const directory of directories) {
		const names = await readdir(directory).catch(() => [] as string[]);
		for (const name of names) {
			if (!name.endsWith(".jsonl")) continue;
			const path = join(directory, name);
			const info = await stat(path).catch(() => undefined);
			if (info?.isFile()) files.push({ path, modifiedAt: info.mtimeMs });
		}
	}
	return files.sort((left, right) => right.modifiedAt - left.modifiedAt || left.path.localeCompare(right.path));
}

/** The session id in a session file's name (`<created>_<id>.jsonl`). */
function fileSessionId(path: string): string {
	const name = path.slice(path.lastIndexOf("/") + 1).replace(/\.jsonl$/, "");
	const separator = name.indexOf("_");
	return separator === -1 ? name : name.slice(separator + 1);
}

/** The Ultron session of a Claude Code session (`ultron claude` remembers it by Claude Code's session id). */
function claudeSessionPath(agentDir: string, claudeSessionId: string): string | undefined {
	const path = join(agentDir, "claude-code", "sessions", `${claudeSessionId.replace(/[^A-Za-z0-9._-]/g, "_")}.json`);
	try {
		const saved = JSON.parse(readFileSync(path, "utf8")) as { path?: unknown };
		return typeof saved.path === "string" && existsSync(saved.path) ? saved.path : undefined;
	} catch {
		return undefined;
	}
}

async function resolveTarget(target: string, root: string, cwd: string, agentDir: string): Promise<string> {
	const asPath = resolve(cwd, target);
	if (existsSync(asPath) && statSync(asPath).isFile()) return asPath;
	const files = await listSessionFiles(root);
	const exact = files.filter((file) => fileSessionId(file.path) === target);
	const matches = exact.length > 0 ? exact : files.filter((file) => fileSessionId(file.path).startsWith(target));
	if (matches.length === 1) return matches[0]!.path;
	if (matches.length > 1)
		throw new Error(
			`${matches.length} sessions match "${target}": ${matches
				.slice(0, 5)
				.map((file) => fileSessionId(file.path))
				.join(", ")}${matches.length > 5 ? ", ..." : ""}`,
		);
	const claude = claudeSessionPath(agentDir, target);
	if (claude !== undefined) return claude;
	throw new Error(`No session file or session id "${target}" (looked in ${root})`);
}

async function reportOf(path: string): Promise<{ report: SessionReport; empty: boolean }> {
	const log = await readSessionLog(path);
	return { report: buildSessionReport(log), empty: sessionIsEmpty(log) };
}

/** Handle `usage ...`. Returns false when args are not a usage command. */
export async function runUsageCommand(
	args: readonly string[],
	io: UsageCommandIo = defaultIo,
	environment: UsageCommandEnvironment = {},
): Promise<boolean> {
	if (args[0] !== "usage") return false;
	const rest = args.slice(1);
	if (rest.includes("--help") || rest.includes("-h")) {
		io.stdout(usage());
		return true;
	}
	const cwd = environment.cwd ?? process.cwd();
	const env = environment.env ?? process.env;
	const agentDir = environment.agentDir ?? getAgentDir();
	try {
		const options = parse(rest);
		const root = resolvePath(
			options.sessionsRoot ?? env[ENV_SESSION_DIR] ?? join(agentDir, "experimental", "sessions"),
		);
		const text = { utc: options.utc, ...(environment.home === undefined ? {} : { home: environment.home }) };
		if (options.last !== undefined) {
			const reports: SessionReport[] = [];
			let skipped = 0;
			for (const file of await listSessionFiles(root)) {
				if (reports.length >= options.last) break;
				try {
					if (options.here && (await readSessionHeader(file.path)).cwd !== cwd) continue;
					const { report, empty } = await reportOf(file.path);
					if (empty && !options.all) skipped += 1;
					else reports.push(report);
				} catch (error) {
					// A file that is not a native session (a legacy Pi file left in the directory) is not listed.
					if (!(error instanceof SessionLogError)) throw error;
				}
			}
			if (options.json)
				io.stdout(JSON.stringify({ schema: SESSION_REPORT_LIST_SCHEMA, sessions: reports }, null, 2));
			else if (reports.length === 0)
				io.stdout(
					`No sessions${options.here ? ` of ${cwd}` : ""} under ${root}${skipped > 0 ? ` (${skipped} with nothing run; --all lists them)` : ""}`,
				);
			else {
				for (const line of renderSessionTable(reports, text)) io.stdout(line);
				if (skipped > 0)
					io.stdout(
						`(${skipped} newer session${skipped === 1 ? "" : "s"} in which nothing ran not listed; --all lists them)`,
					);
			}
			return true;
		}
		let path: string | undefined;
		if (options.target !== undefined) path = await resolveTarget(options.target, root, cwd, agentDir);
		else {
			// The most recent session of this directory in which something ran, else the most recent one.
			let fallback: string | undefined;
			for (const file of await listSessionFiles(root)) {
				const header = await readSessionHeader(file.path).catch(() => undefined);
				if (header?.cwd !== cwd) continue;
				fallback ??= file.path;
				if (!sessionIsEmpty(await readSessionLog(file.path))) {
					path = file.path;
					break;
				}
			}
			path ??= fallback;
			if (path === undefined)
				throw new Error(`No session of ${cwd} under ${root}; name one, or see \`${APP_NAME} usage --last 10\``);
		}
		const { report } = await reportOf(path);
		if (options.json) io.stdout(JSON.stringify(report, null, 2));
		else for (const line of renderSessionReport(report, text)) io.stdout(line);
	} catch (error) {
		const message = error instanceof Error ? error.message : String(error);
		io.stderr(`Error: ${message}`);
		if (error instanceof UsageError) io.stderr(`\n${usage()}`);
		process.exitCode = error instanceof UsageError ? 2 : 1;
	}
	return true;
}
