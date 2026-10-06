/**
 * The installed Claude Code CLI, driven only through its documented command line: where it is, whether it is
 * logged in (`claude auth status --json`, which reports the login without exposing credentials), and which flags
 * this version has (read from `claude --help`). Ultron never reads, copies or writes Claude Code's credential
 * files or its settings.
 */
import { spawnSync } from "node:child_process";
import { accessSync, constants } from "node:fs";
import { delimiter, isAbsolute, join } from "node:path";

/** Claude Code model of the root, of claude subagents and (as claude-code/…) of frames: Opus 5.5. */
export const DEFAULT_CLAUDE_MODEL = "claude-opus-5-5";

/** `ULTRON_CLAUDE_BIN`, else `claude` on PATH. */
export function resolveClaudeBinary(env: NodeJS.ProcessEnv = process.env): string | undefined {
	const configured = env.ULTRON_CLAUDE_BIN?.trim();
	if (configured) return isAbsolute(configured) ? configured : (which(configured, env) ?? configured);
	return which("claude", env);
}

function which(command: string, env: NodeJS.ProcessEnv): string | undefined {
	const extensions = process.platform === "win32" ? (env.PATHEXT ?? ".EXE;.CMD").split(";") : [""];
	for (const dir of (env.PATH ?? "").split(delimiter)) {
		if (!dir) continue;
		for (const extension of extensions) {
			const candidate = join(dir, `${command}${extension}`);
			try {
				accessSync(candidate, constants.X_OK);
				return candidate;
			} catch {
				// Not here.
			}
		}
	}
	return undefined;
}

export interface ClaudeAuthStatus {
	readonly loggedIn: boolean;
	readonly authMethod?: string;
	readonly apiProvider?: string;
	readonly subscriptionType?: string;
}

/** `claude auth status --json`, reduced to the fields Ultron shows (never the account's identity). */
export function claudeAuthStatus(binary: string, env: NodeJS.ProcessEnv = process.env): ClaudeAuthStatus {
	const result = spawnSync(binary, ["auth", "status", "--json"], {
		encoding: "utf8",
		env,
		timeout: 20_000,
		stdio: ["ignore", "pipe", "pipe"],
	});
	if (result.error) throw new Error(`could not run ${binary}: ${result.error.message}`);
	let parsed: Record<string, unknown>;
	try {
		parsed = JSON.parse(result.stdout) as Record<string, unknown>;
	} catch {
		return { loggedIn: false };
	}
	const text = (key: string) => (typeof parsed[key] === "string" ? { [key]: parsed[key] as string } : {});
	return {
		loggedIn: parsed.loggedIn === true,
		...text("authMethod"),
		...text("apiProvider"),
		...text("subscriptionType"),
	};
}

/** The long flags `claude --help` lists (without the leading dashes). */
export function claudeFlags(binary: string, env: NodeJS.ProcessEnv = process.env): Set<string> {
	const result = spawnSync(binary, ["--help"], {
		encoding: "utf8",
		env,
		timeout: 20_000,
		stdio: ["ignore", "pipe", "pipe"],
	});
	return parseFlags(`${result.stdout ?? ""}\n${result.stderr ?? ""}`);
}

/**
 * `--dangerously-skip-permissions` for the Claude Code processes Ultron starts (`ultron claude`, its subagents,
 * `ultron --claude`): on by default. Left out with ULTRON_CLAUDE_SKIP_PERMISSIONS=off, and when running as root,
 * where Claude Code refuses the flag unless IS_SANDBOX=1 says the process is sandboxed (a container).
 */
export function skipPermissionsArgs(
	env: NodeJS.ProcessEnv = process.env,
	uid: number | undefined = process.getuid?.(),
): string[] {
	const raw = env.ULTRON_CLAUDE_SKIP_PERMISSIONS?.trim().toLowerCase();
	if (raw === "off" || raw === "0" || raw === "false" || raw === "no") return [];
	if (uid === 0 && env.IS_SANDBOX !== "1") return [];
	return ["--dangerously-skip-permissions"];
}

export function parseFlags(help: string): Set<string> {
	const flags = new Set<string>();
	for (const match of help.matchAll(/(?:^|[\s,])--([A-Za-z][A-Za-z0-9-]*)/g)) flags.add(match[1]!);
	return flags;
}

/** `claude --version`'s version number, if it prints one. */
export function claudeVersion(binary: string, env: NodeJS.ProcessEnv = process.env): string | undefined {
	const result = spawnSync(binary, ["--version"], {
		encoding: "utf8",
		env,
		timeout: 20_000,
		stdio: ["ignore", "pipe", "pipe"],
	});
	return result.stdout?.match(/\d+\.\d+\.\d+/)?.[0];
}

/** Flags `ultron claude` needs from Claude Code; a CLI without one of them is too old. */
export const REQUIRED_CLAUDE_FLAGS = [
	"mcp-config",
	"strict-mcp-config",
	"tools",
	"allowedTools",
	"settings",
	"setting-sources",
] as const;
