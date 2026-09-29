/**
 * Claude Code as the root agent of Ultron: `ultron claude`, `ultron mcp`, `ultron hook`, `ultron watch` and
 * `ultron guide`. main.ts loads this module only for these commands.
 */
import { runtimeGuide } from "./guide.ts";
import { runHookCommand } from "./hooks.ts";
import { runClaudeLauncher } from "./launcher.ts";
import { runMcpServer } from "./mcp-server.ts";
import { runWatchCommand } from "./watch.ts";

export const CLAUDE_COMMANDS = ["claude", "mcp", "hook", "watch", "guide"] as const;

const GUIDE_HELP = `Usage: ultron guide [--for ultron|claude|claude-child] [--cwd <dir>]

Print the runtime guide: Ultron's own (default), or the system prompt \`ultron claude\` gives Claude Code
(--for claude; --for claude-child for a Claude Code subagent).`;

/** A usage error: printed without a stack, exit code 2. */
class UsageError extends Error {}

function runGuide(args: readonly string[]): void {
	let audience: "ultron" | "claude" | "claude-child" = "ultron";
	let cwd: string | undefined;
	for (let index = 0; index < args.length; index += 1) {
		const arg = args[index]!;
		if (arg === "--help" || arg === "-h") {
			process.stdout.write(`${GUIDE_HELP}\n`);
			return;
		}
		if (arg === "--for") {
			const value = args[++index];
			if (value !== "ultron" && value !== "claude" && value !== "claude-child")
				throw new UsageError("--for must be ultron, claude or claude-child");
			audience = value;
		} else if (arg === "--cwd") {
			cwd = args[++index];
			if (cwd === undefined) throw new UsageError("--cwd needs a directory");
		} else throw new UsageError(`unknown option for ultron guide: ${arg}`);
	}
	const text = runtimeGuide(
		audience,
		audience === "ultron"
			? {}
			: { cwd: cwd ?? process.cwd(), platform: process.platform, date: new Date().toISOString().slice(0, 10) },
	);
	process.stdout.write(`${text}\n`);
}

/** Run one of CLAUDE_COMMANDS; `args[0]` is the command. Sets process.exitCode on failure. */
export async function runClaudeCommand(args: readonly string[]): Promise<void> {
	const [command, ...rest] = args;
	try {
		if (command === "guide") runGuide(rest);
		else if (command === "hook") await runHookCommand(rest);
		else if (command === "watch") await runWatchCommand(rest);
		else if (command === "mcp") await runMcpServer(rest);
		else if (command === "claude") await runClaudeLauncher(rest);
		else throw new UsageError(`unknown command: ${command}`);
	} catch (error) {
		const message = error instanceof Error ? error.message : String(error);
		process.stderr.write(`Error: ${message}\n`);
		process.exitCode = error instanceof UsageError || /^unknown option|needs a value|must be /.test(message) ? 2 : 1;
	}
}
