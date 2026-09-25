/**
 * `ultron migrate ...`: explicit Pi <-> Ultron session migration and profile backup/restore.
 * Output names paths and counts only; credential file contents are never printed.
 */
import { dirname, join } from "node:path";
import { APP_NAME, ENV_SESSION_DIR, getAgentDir } from "../config.ts";
import { resolvePath } from "../utils/paths.ts";
import { backupProfile, exportNativeSessionToPi, importPiSession, restoreProfile } from "./migration.ts";

export interface MigrationCommandIo {
	stdout(line: string): void;
	stderr(line: string): void;
}

const defaultIo: MigrationCommandIo = {
	stdout: (line) => process.stdout.write(`${line}\n`),
	stderr: (line) => process.stderr.write(`${line}\n`),
};

function usage(): string {
	return [
		`Usage: ${APP_NAME} migrate <command>`,
		"",
		"Commands:",
		"  import-pi <pi-session.jsonl> [--sessions-root <dir>]   Import a Pi session's active branch as a native session",
		"  export-pi <native-session.jsonl> <out.jsonl>          Write a native session's main branch as a Pi session",
		"  backup [dir] [--agent-dir <dir>]                      Back up the config profile (not sessions)",
		"  restore <backup-dir> [--agent-dir <dir>]              Verify and restore a profile backup",
	].join("\n");
}

class UsageError extends Error {}

/** Same resolution as the native server's `resolveSessionDirectory` (kept local to avoid loading the server). */
function resolveNativeSessionsRoot(sessionDir: string | undefined): string {
	return resolvePath(sessionDir ?? process.env[ENV_SESSION_DIR] ?? join(getAgentDir(), "experimental", "sessions"));
}

function parseOptions(
	args: readonly string[],
	flags: readonly string[],
): { positional: string[]; options: Map<string, string> } {
	const positional: string[] = [];
	const options = new Map<string, string>();
	for (let index = 0; index < args.length; index++) {
		const arg = args[index]!;
		if (arg.startsWith("--")) {
			const [name, inline] = arg.slice(2).split("=", 2) as [string, string | undefined];
			if (!flags.includes(name)) throw new UsageError(`Unknown option --${name}`);
			const value = inline ?? args[++index];
			if (value === undefined || value === "") throw new UsageError(`Option --${name} requires a value`);
			options.set(name, value);
		} else {
			positional.push(arg);
		}
	}
	return { positional, options };
}

function expectPositional(positional: readonly string[], min: number, max: number): void {
	if (positional.length < min) throw new UsageError("Missing argument");
	if (positional.length > max) throw new UsageError(`Unexpected argument: ${positional[max]}`);
}

/** Handle `migrate ...`. Returns false when args are not a migrate command. */
export async function runMigrationCommand(args: string[], io: MigrationCommandIo = defaultIo): Promise<boolean> {
	if (args[0] !== "migrate") return false;
	const [subcommand, ...rest] = args.slice(1);
	if (subcommand === undefined || subcommand === "--help" || subcommand === "-h" || subcommand === "help") {
		io.stdout(usage());
		if (subcommand === undefined) process.exitCode = 1;
		return true;
	}
	try {
		switch (subcommand) {
			case "import-pi": {
				const { positional, options } = parseOptions(rest, ["sessions-root"]);
				expectPositional(positional, 1, 1);
				const result = await importPiSession({
					piSessionPath: positional[0]!,
					sessionsRoot: resolveNativeSessionsRoot(options.get("sessions-root")),
				});
				io.stdout(`Imported ${result.imported} entries as native session ${result.sessionId}`);
				io.stdout(`  ${result.path}`);
				for (const { type, count } of result.skipped)
					io.stdout(`  skipped ${count} ${type} entr${count === 1 ? "y" : "ies"}`);
				return true;
			}
			case "export-pi": {
				const { positional } = parseOptions(rest, []);
				expectPositional(positional, 2, 2);
				const result = await exportNativeSessionToPi({ sessionPath: positional[0]!, outputPath: positional[1]! });
				io.stdout(`Exported native session ${result.sessionId} (${result.entries} entries) to ${result.path}`);
				return true;
			}
			case "backup": {
				const { positional, options } = parseOptions(rest, ["agent-dir"]);
				expectPositional(positional, 0, 1);
				const agentDir = options.get("agent-dir") ?? getAgentDir();
				const backupDir = positional[0] ?? join(dirname(agentDir), "backups");
				const result = await backupProfile({ agentDir, backupDir });
				io.stdout(`Backed up ${result.manifest.entries.length} profile entries to ${result.path}`);
				return true;
			}
			case "restore": {
				const { positional, options } = parseOptions(rest, ["agent-dir"]);
				expectPositional(positional, 1, 1);
				const agentDir = options.get("agent-dir") ?? getAgentDir();
				const result = await restoreProfile({ backupPath: positional[0]!, agentDir });
				io.stdout(`Restored ${result.restored} profile entries to ${agentDir}`);
				return true;
			}
			default:
				throw new UsageError(`Unknown migrate command: ${subcommand}`);
		}
	} catch (error) {
		// Errors carry paths and verification problems only, never profile file contents.
		io.stderr(`Error: ${error instanceof Error ? error.message : String(error)}`);
		if (error instanceof UsageError) io.stderr(usage());
		process.exitCode = 1;
		return true;
	}
}
