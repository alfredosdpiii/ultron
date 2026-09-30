/**
 * `ultron claude [options] [claude args...]`: Claude Code as the root agent of Ultron. It finds `claude`, checks the
 * login with `claude auth status --json` (it never touches credential files), checks the flags it needs, and runs
 * Claude Code with:
 *
 * - `--strict-mcp-config --mcp-config <ultron mcp>`: Ultron's REPL is the one MCP server (`--keep-mcp` keeps the
 *   user's own servers too; Ultron's own MCP servers are reachable from the REPL through `mcp.call` either way);
 * - `--tools "" --allowedTools mcp__ultron__rlm`: no built-in tools, and the REPL runs without permission prompts;
 * - `--system-prompt-file <guide>`: `ultron guide --for claude` replaces Claude Code's default system prompt;
 * - `--setting-sources ""`: the user's and project's Claude Code settings (their hooks, permissions, CLAUDE.md) are
 *   not loaded (`--keep-settings` loads them); the system prompt carries the context files (AGENTS.md, CLAUDE.md) and
 *   skills native Ultron loads for the directory instead (`resources.ts`; `--no-context-files`, `--no-skills`);
 * - `--settings <hooks>`: SessionStart, UserPromptSubmit and Stop hooks that call `ultron hook ...`.
 *
 * Other arguments go to `claude` unchanged (`-p`, `--model`, `--resume`, ...). The config files live in a private
 * temp directory removed when Claude Code exits. `--print-config` prints the command instead of running it, with
 * the configs inline so it works from other tools.
 */
import { spawn, spawnSync } from "node:child_process";
import { randomBytes } from "node:crypto";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { getAgentDir } from "../../config.ts";
import { SettingsManager } from "../../core/settings-manager.ts";
import {
	type ClaudeAuthStatus,
	claudeAuthStatus,
	claudeFlags,
	claudeVersion,
	DEFAULT_CLAUDE_MODEL,
	REQUIRED_CLAUDE_FLAGS,
	resolveClaudeBinary,
} from "./claude-cli.ts";
import { socketPathFor } from "./control-socket.ts";
import { CLAUDE_RLM_TOOL, claudeSystemPrompt } from "./guide.ts";
import {
	type ClaudePromptResources,
	describeClaudePromptResources,
	loadClaudePromptResources,
	NO_PROMPT_RESOURCES,
} from "./resources.ts";
import { type SelfCommand, selfCommand, shellCommand, shellQuote } from "./self.ts";

export interface LauncherOptions {
	printConfig: boolean;
	keepMcp: boolean;
	keepSettings: boolean;
	watch: boolean;
	hooks: boolean;
	/** `--no-context-files`: no AGENTS.md / CLAUDE.md in the system prompt (root and subagents). */
	noContextFiles?: boolean;
	/** `--no-skills`: no skills listed in the system prompt (root and subagents). */
	noSkills?: boolean;
	frameModel?: string;
	children?: "claude" | "ultron";
	childModel?: string;
	/** Claude Code model of the root when neither `--model` nor ULTRON_CLAUDE_MODEL names one (`claudeCode.model`). */
	rootModel?: string;
	/** Arguments for `claude`. */
	claudeArgs: string[];
}

const HELP = `Usage: ultron claude [options] [claude args...]

Run Claude Code as the root agent of Ultron: its only tool is Ultron's REPL (rlm).

Options (before any claude args, or anywhere before --):
  --print-config         Print the claude command and its config instead of running it
  --keep-mcp             Also load your own Claude Code MCP servers (default: only Ultron's)
  --keep-settings        Load your user/project Claude Code settings and CLAUDE.md (default: none)
  --no-hooks             No Ultron hooks (no automatic memory, events only in rlm results)
  --no-context-files, -nc  Leave AGENTS.md / CLAUDE.md out of the system prompt (included by default, as in ultron)
  --no-skills, -ns       Leave your skills out of the system prompt (listed by default, as in ultron)
  --watch                Open \`ultron watch\` in a tmux split (inside tmux)
  --frame-model <p/m>    Model of rlm.map/rlm.infer frames (default claude-code/claude-opus-5-5)
  --children <mode>      rlm.spawn subagents: claude (Claude Code processes, default) or ultron
  --child-model <alias>  Claude Code model of claude subagents (default claude-opus-5-5)
  --help                 This help

Claude Code itself runs claude-opus-5-5 unless you pass --model (or set ULTRON_CLAUDE_MODEL).
Everything else is passed to claude (for example -p "prompt", --model sonnet, --resume <id>).`;

/** Split `ultron claude` arguments into Ultron's options and claude's. */
export function parseLauncherArgs(args: readonly string[]): LauncherOptions | "help" {
	const options: LauncherOptions = {
		printConfig: false,
		keepMcp: false,
		keepSettings: false,
		watch: false,
		hooks: true,
		claudeArgs: [],
	};
	for (let index = 0; index < args.length; index += 1) {
		const arg = args[index]!;
		const value = (): string => {
			const next = args[index + 1];
			if (next === undefined) throw new Error(`${arg} needs a value`);
			index += 1;
			return next;
		};
		if (arg === "--") {
			options.claudeArgs.push(...args.slice(index + 1));
			break;
		}
		if (arg === "--help" || arg === "-h") return "help";
		if (arg === "--print-config") options.printConfig = true;
		else if (arg === "--keep-mcp") options.keepMcp = true;
		else if (arg === "--keep-settings") options.keepSettings = true;
		else if (arg === "--watch") options.watch = true;
		else if (arg === "--no-hooks") options.hooks = false;
		else if (arg === "--no-context-files" || arg === "-nc") options.noContextFiles = true;
		else if (arg === "--no-skills" || arg === "-ns") options.noSkills = true;
		else if (arg === "--frame-model") options.frameModel = value();
		else if (arg === "--child-model") options.childModel = value();
		else if (arg === "--children") {
			const mode = value();
			if (mode !== "claude" && mode !== "ultron") throw new Error("--children must be claude or ultron");
			options.children = mode;
		} else options.claudeArgs.push(arg);
	}
	return options;
}

export interface ClaudeLaunch {
	readonly claude: string;
	readonly args: string[];
	readonly env: Record<string, string>;
	/** Files the args refer to (written to the session's temp dir), by path. */
	readonly files: Record<string, string>;
	readonly socket: string;
	readonly watchCommand: string;
	/** The system prompt's length in characters. */
	readonly promptChars: number;
}

/** Everything `claude` is started with, for a session whose files go in `dir` (inline when `dir` is undefined). */
export function buildClaudeLaunch(input: {
	claude: string;
	flags: ReadonlySet<string>;
	self: SelfCommand;
	options: LauncherOptions;
	cwd: string;
	dir: string | undefined;
	socket: string;
	env: NodeJS.ProcessEnv;
	date?: string;
	/** Context files and skills for the system prompt (`loadClaudePromptResources`); none when absent. */
	resources?: ClaudePromptResources;
}): ClaudeLaunch {
	const { options, self, socket } = input;
	const files: Record<string, string> = {};
	const mcpArgs = [
		...self.args,
		"mcp",
		"--no-instructions",
		"--socket",
		socket,
		...(options.frameModel === undefined ? [] : ["--frame-model", options.frameModel]),
		...(options.children === undefined ? [] : ["--children", options.children]),
		...(options.childModel === undefined ? [] : ["--child-model", options.childModel]),
		// Subagents get the same context files and skills as the root.
		...(options.noContextFiles === true ? ["--no-context-files"] : []),
		...(options.noSkills === true ? ["--no-skills"] : []),
	];
	const mcpConfig = JSON.stringify({
		mcpServers: { ultron: { type: "stdio", command: self.command, args: mcpArgs } },
	});
	const hook = (event: string, timeout: number) => ({
		hooks: [{ type: "command", command: shellCommand(self, ["hook", event, "--socket", socket]), timeout }],
	});
	const settings = JSON.stringify({
		permissions: { allow: [CLAUDE_RLM_TOOL] },
		...(options.hooks
			? {
					hooks: {
						SessionStart: [hook("session-start", 30)],
						UserPromptSubmit: [hook("user-prompt", 60)],
						Stop: [hook("stop", 30)],
					},
				}
			: {}),
	});
	const prompt = claudeSystemPrompt("claude", {
		cwd: input.cwd,
		platform: process.platform,
		date: input.date ?? new Date().toISOString().slice(0, 10),
		resources: input.resources ?? NO_PROMPT_RESOURCES,
	});
	const file = (name: string, content: string): string | undefined => {
		if (input.dir === undefined) return undefined;
		const path = join(input.dir, name);
		files[path] = content;
		return path;
	};
	const mcpPath = file("mcp.json", mcpConfig);
	const settingsPath = file("settings.json", settings);
	const promptPath = input.flags.has("system-prompt-file") ? file("system-prompt.md", prompt) : undefined;
	const args = [
		...(options.keepMcp ? [] : ["--strict-mcp-config"]),
		"--mcp-config",
		mcpPath ?? mcpConfig,
		"--tools",
		"",
		"--allowedTools",
		CLAUDE_RLM_TOOL,
		...(promptPath === undefined ? ["--system-prompt", prompt] : ["--system-prompt-file", promptPath]),
		"--settings",
		settingsPath ?? settings,
		...(options.keepSettings ? [] : ["--setting-sources", ""]),
		...(options.claudeArgs.some((arg) => arg === "--model" || arg.startsWith("--model="))
			? []
			: ["--model", input.env.ULTRON_CLAUDE_MODEL?.trim() || options.rootModel || DEFAULT_CLAUDE_MODEL]),
		...options.claudeArgs,
	];
	const env: Record<string, string> = {};
	// A cell may wait on subagents for a long time; Claude Code's MCP tool timeout must not cut it off.
	if (!input.env.MCP_TOOL_TIMEOUT) env.MCP_TOOL_TIMEOUT = String(2 * 60 * 60 * 1000);
	return {
		claude: input.claude,
		args,
		env,
		files,
		socket,
		watchCommand: shellCommand(self, ["watch", "--socket", socket]),
		promptChars: prompt.length,
	};
}

function requireClaude(env: NodeJS.ProcessEnv): { claude: string; flags: Set<string>; version?: string } {
	const claude = resolveClaudeBinary(env);
	if (claude === undefined)
		throw new Error(
			"Claude Code (`claude`) is not on PATH. Install it (https://claude.com/claude-code) or set ULTRON_CLAUDE_BIN.",
		);
	const flags = claudeFlags(claude, env);
	const missing = REQUIRED_CLAUDE_FLAGS.filter((flag) => !flags.has(flag));
	const version = claudeVersion(claude, env);
	if (missing.length > 0)
		throw new Error(
			`${claude}${version ? ` ${version}` : ""} lacks ${missing.map((flag) => `--${flag}`).join(", ")}; update Claude Code (\`claude update\`).`,
		);
	return { claude, flags, ...(version === undefined ? {} : { version }) };
}

function describeAuth(status: ClaudeAuthStatus): string {
	return [status.authMethod, status.apiProvider, status.subscriptionType].filter(Boolean).join(", ");
}

/** `ultron claude ...`. */
export async function runClaudeLauncher(argv: readonly string[]): Promise<void> {
	const parsed = parseLauncherArgs(argv);
	if (parsed === "help") {
		process.stdout.write(`${HELP}\n`);
		return;
	}
	const env = process.env;
	// `claudeCode.model` (/settings → Models); the MCP server reads the frame and child models itself.
	parsed.rootModel ??= SettingsManager.create(process.cwd(), getAgentDir(), {
		projectTrusted: false,
	}).getClaudeCodeSettings().model;
	const { claude, flags } = requireClaude(env);
	const auth = claudeAuthStatus(claude, env);
	if (!auth.loggedIn && !parsed.printConfig)
		throw new Error("Claude Code is not logged in. Run `claude auth login` (or `claude`, then /login) first.");
	const self = selfCommand(env);
	const cwd = process.cwd();
	const launchId = `L-${Date.now().toString(36)}-${randomBytes(4).toString("hex")}`;
	const socket = socketPathFor(launchId);
	const resources = await loadClaudePromptResources(cwd, parsed);
	if (parsed.printConfig) {
		const launch = buildClaudeLaunch({
			claude,
			flags,
			self,
			options: parsed,
			cwd,
			dir: undefined,
			socket,
			env,
			resources,
		});
		const envPrefix = Object.entries(launch.env).map(([key, value]) => `${key}=${shellQuote(value)} `);
		process.stdout.write(
			`${JSON.stringify(
				{
					claude,
					auth: { loggedIn: auth.loggedIn, method: describeAuth(auth) },
					args: launch.args,
					env: launch.env,
					socket,
					watch: launch.watchCommand,
					// Paths and skill names only; the prompt itself is in args.
					systemPrompt: { chars: launch.promptChars, ...describeClaudePromptResources(resources) },
					command: `${envPrefix.join("")}${[claude, ...launch.args].map(shellQuote).join(" ")}`,
				},
				null,
				2,
			)}\n`,
		);
		return;
	}
	const dir = mkdtempSync(join(tmpdir(), "ultron-claude-"));
	const cleanup = (): void => rmSync(dir, { recursive: true, force: true });
	process.once("exit", cleanup);
	const launch = buildClaudeLaunch({ claude, flags, self, options: parsed, cwd, dir, socket, env, resources });
	for (const [path, content] of Object.entries(launch.files)) writeFileSync(path, content, { mode: 0o600 });
	const interactive = !parsed.claudeArgs.some((arg) => arg === "-p" || arg === "--print");
	if (parsed.watch && env.TMUX) {
		spawnSync("tmux", ["split-window", "-h", "-d", launch.watchCommand], { stdio: "ignore" });
	} else if (interactive || parsed.watch) {
		process.stderr.write(`Ultron RLM view: ${launch.watchCommand}\n`);
	}
	const child = spawn(claude, launch.args, { stdio: "inherit", env: { ...env, ...launch.env } });
	// Ctrl-C belongs to Claude Code while it runs.
	const ignore = (): void => {};
	process.on("SIGINT", ignore);
	const code = await new Promise<number>((resolve) => {
		child.once("error", (error) => {
			process.stderr.write(`ultron claude: ${error.message}\n`);
			resolve(1);
		});
		child.once("exit", (status, signal) => resolve(status ?? (signal ? 128 + 1 : 1)));
	});
	process.off("SIGINT", ignore);
	cleanup();
	process.exitCode = code;
}
