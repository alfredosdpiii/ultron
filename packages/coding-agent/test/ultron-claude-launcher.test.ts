import { spawnSync } from "node:child_process";
import { chmodSync, existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import { parseFlags } from "../src/ultron/claude/claude-cli.ts";
import { buildClaudeLaunch, parseLauncherArgs } from "../src/ultron/claude/launcher.ts";

/**
 * `ultron claude`: the Claude Code command line it builds (Ultron's REPL as the only tool, its guide as the system
 * prompt, no user settings, Ultron's hooks), run against a fake `claude` that records what it was given.
 */

const here = dirname(fileURLToPath(import.meta.url));
const cliPath = resolve(here, "../src/cli.ts");
const sourceResolverPath = resolve(here, "../src/experimental/source-resolver.ts");

const HELP = `Usage: claude [options] [command] [prompt]
  --mcp-config <configs...>  Load MCP servers
  --strict-mcp-config        Only use MCP servers from --mcp-config
  --tools <tools...>         Built-in tools
  --allowedTools, --allowed-tools <tools...>  Allowed tools
  --settings <file-or-json>  Settings
  --setting-sources <sources>  Sources
  --system-prompt <prompt>   System prompt
  --system-prompt-file <file>  System prompt file
  -p, --print                Print`;

let work: string;
let fake: string;
beforeAll(() => {
	work = mkdtempSync(join(tmpdir(), "ultron-claude-launch-"));
	fake = join(work, "claude");
	// Answers auth status and --help like Claude Code; otherwise records its arguments, the files they name and the
	// environment, then exits 7.
	writeFileSync(
		fake,
		`#!/usr/bin/env node
const fs = require("node:fs");
const args = process.argv.slice(2);
if (args[0] === "auth") { process.stdout.write(process.env.FAKE_LOGGED_OUT ? '{"loggedIn":false}' : '{"loggedIn":true,"authMethod":"claude.ai","apiProvider":"firstParty","email":"someone@example.com","subscriptionType":"max"}'); process.exit(0); }
if (args[0] === "--help") { process.stdout.write(process.env.FAKE_OLD ? "  --print" : ${JSON.stringify(HELP)}); process.exit(0); }
if (args[0] === "--version") { process.stdout.write("2.1.284 (Claude Code)\\n"); process.exit(0); }
const read = (flag) => { const i = args.indexOf(flag); return i === -1 ? undefined : fs.readFileSync(args[i + 1], "utf8"); };
fs.writeFileSync(process.env.FAKE_RECORD, JSON.stringify({ args, prompt: read("--system-prompt-file"), mcp: read("--mcp-config"), settings: read("--settings"), timeout: process.env.MCP_TOOL_TIMEOUT }));
process.exit(7);
`,
	);
	chmodSync(fake, 0o755);
});
afterAll(() => rmSync(work, { recursive: true, force: true }));

function launch(args: string[], env: Record<string, string> = {}) {
	const record = join(work, `record-${Math.random().toString(36).slice(2)}.json`);
	const result = spawnSync(process.execPath, ["--import", sourceResolverPath, cliPath, "claude", ...args], {
		cwd: work,
		encoding: "utf8",
		env: {
			...process.env,
			ULTRON_CLAUDE_BIN: fake,
			FAKE_RECORD: record,
			XDG_RUNTIME_DIR: work,
			MCP_TOOL_TIMEOUT: "",
			...env,
		},
		timeout: 60_000,
	});
	const recorded = existsSync(record)
		? (JSON.parse(readFileSync(record, "utf8")) as {
				args: string[];
				prompt?: string;
				mcp?: string;
				settings?: string;
				timeout?: string;
			})
		: undefined;
	return { ...result, recorded };
}

describe("ultron claude", () => {
	test("runs claude with the REPL as the only tool, Ultron's guide and hooks, and passes other args through", () => {
		const { status, recorded, stderr } = launch([
			"-p",
			"hello",
			"--model",
			"opus",
			"--frame-model",
			"claude-code/haiku",
		]);
		expect(stderr).toBe("");
		expect(status).toBe(7);
		const args = recorded!.args;
		const flagValue = (flag: string) => args[args.indexOf(flag) + 1];
		expect(args).toContain("--strict-mcp-config");
		expect(flagValue("--tools")).toBe("");
		expect(flagValue("--allowedTools")).toBe("mcp__ultron__rlm");
		expect(flagValue("--setting-sources")).toBe("");
		expect(args.slice(-4)).toEqual(["-p", "hello", "--model", "opus"]);
		// The system prompt replaces Claude Code's and stands alone.
		expect(args).not.toContain("--append-system-prompt");
		expect(recorded!.prompt).toContain("You are Claude, the root agent of Ultron");
		expect(recorded!.prompt).toContain(`- Working directory: ${work}`);
		const mcp = JSON.parse(recorded!.mcp!) as { mcpServers: { ultron: { command: string; args: string[] } } };
		const serverArgs = mcp.mcpServers.ultron.args;
		expect(serverArgs).toEqual(
			expect.arrayContaining(["mcp", "--no-instructions", "--frame-model", "claude-code/haiku"]),
		);
		const socket = serverArgs[serverArgs.indexOf("--socket") + 1]!;
		expect(socket.startsWith(join(work, "ultron-claude"))).toBe(true);
		const settings = JSON.parse(recorded!.settings!) as {
			permissions: { allow: string[] };
			hooks: Record<string, Array<{ hooks: Array<{ command: string }> }>>;
		};
		expect(settings.permissions.allow).toEqual(["mcp__ultron__rlm"]);
		expect(Object.keys(settings.hooks).sort()).toEqual(["SessionStart", "Stop", "UserPromptSubmit"]);
		expect(settings.hooks.UserPromptSubmit![0]!.hooks[0]!.command).toContain(`hook user-prompt --socket ${socket}`);
		expect(recorded!.timeout).toBe(String(2 * 60 * 60 * 1000));
		// The per-session config dir is gone once claude exited.
		const promptFile = args[args.indexOf("--system-prompt-file") + 1]!;
		expect(existsSync(dirname(promptFile))).toBe(false);
	}, 60_000);

	test("--keep-mcp, --keep-settings and --no-hooks", () => {
		const { recorded } = launch(["--keep-mcp", "--keep-settings", "--no-hooks", "-p", "x"]);
		expect(recorded!.args).not.toContain("--strict-mcp-config");
		expect(recorded!.args).not.toContain("--setting-sources");
		expect(JSON.parse(recorded!.settings!).hooks).toBeUndefined();
	}, 60_000);

	test("--print-config prints a self-contained command and runs nothing", () => {
		const { status, stdout, recorded } = launch(["--print-config", "-p", "x"]);
		expect(status).toBe(0);
		expect(recorded).toBeUndefined();
		const printed = JSON.parse(stdout) as {
			args: string[];
			auth: Record<string, unknown>;
			command: string;
			watch: string;
		};
		expect(printed.auth).toEqual({ loggedIn: true, method: "claude.ai, firstParty, max" });
		// Never the account's identity.
		expect(stdout).not.toContain("someone@example.com");
		expect(printed.args).toContain("--system-prompt");
		expect(JSON.parse(printed.args[printed.args.indexOf("--mcp-config") + 1]!).mcpServers.ultron).toBeDefined();
		expect(printed.watch).toContain("watch --socket");
	}, 60_000);

	test("a logged-out or outdated claude is refused", () => {
		const out = launch(["-p", "x"], { FAKE_LOGGED_OUT: "1" });
		expect(out.status).toBe(1);
		expect(out.stderr).toContain("not logged in");
		const old = launch(["-p", "x"], { FAKE_OLD: "1" });
		expect(old.status).toBe(1);
		expect(old.stderr).toContain("update Claude Code");
		const missing = launch(["-p", "x"], { ULTRON_CLAUDE_BIN: join(work, "missing") });
		expect(missing.status).toBe(1);
	}, 60_000);

	test("argument parsing and flag detection", () => {
		expect(parseLauncherArgs(["--watch", "--children", "ultron", "--", "--watch"])).toMatchObject({
			watch: true,
			children: "ultron",
			claudeArgs: ["--watch"],
		});
		expect(parseLauncherArgs(["--help"])).toBe("help");
		expect(() => parseLauncherArgs(["--children", "x"])).toThrow();
		expect([...parseFlags(HELP)]).toEqual(
			expect.arrayContaining(["mcp-config", "allowedTools", "allowed-tools", "system-prompt-file", "print"]),
		);
		// Without --system-prompt-file support the guide goes inline.
		const built = buildClaudeLaunch({
			claude: "claude",
			flags: new Set(["mcp-config"]),
			self: { command: "node", args: ["cli.js"] },
			options: {
				printConfig: false,
				keepMcp: false,
				keepSettings: false,
				watch: false,
				hooks: true,
				claudeArgs: [],
			},
			cwd: "/w",
			dir: "/tmp/x",
			socket: "/run/s.sock",
			env: { MCP_TOOL_TIMEOUT: "5" },
		});
		expect(built.args).toContain("--system-prompt");
		expect(built.env).toEqual({});
		expect(Object.keys(built.files).sort()).toEqual(["/tmp/x/mcp.json", "/tmp/x/settings.json"]);
	});
});
