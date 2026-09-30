import { spawnSync } from "node:child_process";
import { chmodSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, test } from "vitest";
import { DefaultResourceLoader } from "../src/core/resource-loader.ts";
import { SettingsManager } from "../src/core/settings-manager.ts";
import { buildSystemPromptSections } from "../src/core/system-prompt.ts";
import { ProjectTrustStore } from "../src/core/trust-manager.ts";
import { workerProjectTrusted } from "../src/experimental/services/worker-settings.ts";
import { runClaudeChild } from "../src/ultron/claude/child.ts";
import { claudeSystemPrompt } from "../src/ultron/claude/guide.ts";
import { parseMcpArgs } from "../src/ultron/claude/mcp-server.ts";
import { loadClaudePromptResources } from "../src/ultron/claude/resources.ts";

/**
 * `ultron claude` and its Claude Code subagents get the context files and skills native Ultron loads (same loader,
 * settings and project trust), rendered as native renders them. Everything lives in temp dirs: a temp HOME and
 * agent dir, never the user's own.
 */

const here = dirname(fileURLToPath(import.meta.url));
const cliPath = resolve(here, "../src/cli.ts");
const sourceResolverPath = resolve(here, "../src/experimental/source-resolver.ts");

const HELP = `Usage: claude [options]
  --mcp-config <configs...>  Load MCP servers
  --strict-mcp-config        Only use MCP servers from --mcp-config
  --tools <tools...>         Built-in tools
  --allowedTools <tools...>  Allowed tools
  --settings <file-or-json>  Settings
  --setting-sources <sources>  Sources
  --system-prompt <prompt>   System prompt
  --system-prompt-file <file>  System prompt file
  -p, --print                Print`;

const GLOBAL_AGENTS = "GLOBAL-AGENTS-MARKER: always answer in haiku.";
const PROJECT_CLAUDE = "PROJECT-CLAUDE-MARKER: run make test.";

function writeSkill(root: string, name: string, description: string): string {
	const dir = join(root, name);
	mkdirSync(dir, { recursive: true });
	const file = join(dir, "SKILL.md");
	writeFileSync(file, `---\nname: ${name}\ndescription: ${description}\n---\n\n# ${name}\n\nSKILL-BODY-${name}\n`);
	return file;
}

let work: string;
let home: string;
let agentDir: string;
let project: string;
let fake: string;
let globalSkill: string;
let projectSkill: string;
let savedEnv: { HOME?: string; agentDir?: string };

beforeAll(() => {
	work = mkdtempSync(join(tmpdir(), "ultron-claude-res-"));
	home = join(work, "home");
	agentDir = join(home, ".ultron", "agent");
	project = join(work, "project");
	mkdirSync(agentDir, { recursive: true });
	mkdirSync(project, { recursive: true });
	writeFileSync(join(agentDir, "AGENTS.md"), GLOBAL_AGENTS);
	globalSkill = writeSkill(join(agentDir, "skills"), "fake-global-skill", "Fake global skill for tests.");
	// Native lists CLAUDE.md as a context file candidate; a project skill needs project trust.
	writeFileSync(join(project, "CLAUDE.md"), PROJECT_CLAUDE);
	projectSkill = writeSkill(join(project, ".agents", "skills"), "fake-project-skill", "Fake project skill.");
	fake = join(work, "claude");
	writeFileSync(
		fake,
		`#!/usr/bin/env node
const fs = require("node:fs");
const args = process.argv.slice(2);
if (args[0] === "auth") { process.stdout.write('{"loggedIn":true,"authMethod":"claude.ai"}'); process.exit(0); }
if (args[0] === "--help") { process.stdout.write(${JSON.stringify(HELP)}); process.exit(0); }
if (args[0] === "--version") { process.stdout.write("2.1.284 (Claude Code)\\n"); process.exit(0); }
const read = (flag) => { const i = args.indexOf(flag); return i === -1 ? undefined : fs.readFileSync(args[i + 1], "utf8"); };
fs.writeFileSync(process.env.FAKE_RECORD, JSON.stringify({ args, prompt: read("--system-prompt-file"), mcp: (() => { const v = args[args.indexOf("--mcp-config") + 1]; return v.startsWith("{") ? v : fs.readFileSync(v, "utf8"); })() }));
if (args.includes("-p") && args.includes("stream-json")) { process.stdout.write(JSON.stringify({ type: "result", result: "ok", num_turns: 1 }) + "\\n"); process.exit(0); }
process.exit(7);
`,
	);
	chmodSync(fake, 0o755);
});
afterAll(() => rmSync(work, { recursive: true, force: true }));

beforeEach(() => {
	savedEnv = { HOME: process.env.HOME, agentDir: process.env.ULTRON_CODING_AGENT_DIR };
	process.env.HOME = home;
	process.env.ULTRON_CODING_AGENT_DIR = agentDir;
});
afterEach(() => {
	const restore = (name: string, value: string | undefined) => {
		if (value === undefined) delete process.env[name];
		else process.env[name] = value;
	};
	restore("HOME", savedEnv.HOME);
	restore("ULTRON_CODING_AGENT_DIR", savedEnv.agentDir);
	rmSync(join(agentDir, "trust.json"), { force: true });
});

function launch(args: string[]) {
	const record = join(work, `record-${Math.random().toString(36).slice(2)}.json`);
	const result = spawnSync(process.execPath, ["--import", sourceResolverPath, cliPath, "claude", ...args], {
		cwd: project,
		encoding: "utf8",
		env: {
			...process.env,
			HOME: home,
			ULTRON_CODING_AGENT_DIR: agentDir,
			ULTRON_CLAUDE_BIN: fake,
			FAKE_RECORD: record,
			XDG_RUNTIME_DIR: work,
		},
		timeout: 60_000,
	});
	const recorded = existsSync(record)
		? (JSON.parse(readFileSync(record, "utf8")) as { args: string[]; prompt?: string; mcp?: string })
		: undefined;
	return { ...result, recorded };
}

/** What native Ultron's session worker would put in its system prompt for `cwd` (same loader and trust rule). */
async function nativeSections(cwd: string): Promise<Record<string, string>> {
	const settingsManager = SettingsManager.create(cwd, agentDir, {
		projectTrusted: workerProjectTrusted(cwd, agentDir),
	});
	const loader = new DefaultResourceLoader({ cwd, agentDir, settingsManager, noExtensions: true });
	await loader.reload();
	return buildSystemPromptSections({
		cwd,
		selectedTools: ["rlm"],
		contextFiles: loader.getAgentsFiles().agentsFiles,
		skills: loader.getSkills().skills,
	});
}

describe("ultron claude context files and skills", () => {
	test("the launcher's system prompt carries the global AGENTS.md, CLAUDE.md and skills, rendered as native", async () => {
		const { status, recorded, stderr } = launch(["-p", "hi"]);
		expect(stderr).toBe("");
		expect(status).toBe(7);
		const prompt = recorded!.prompt!;
		expect(prompt).toContain(GLOBAL_AGENTS);
		expect(prompt).toContain(PROJECT_CLAUDE);
		expect(prompt).toContain("<name>fake-global-skill</name>");
		expect(prompt).toContain(`<location>${globalSkill}</location>`);
		expect(prompt).toContain("<name>fake-project-skill</name>");
		// Skill bodies are read on demand in a cell, never inlined.
		expect(prompt).not.toContain("SKILL-BODY-");
		expect(prompt).toContain("Load a skill's file in the rlm tool");
		// Claude Code's own CLAUDE.md discovery stays off.
		expect(recorded!.args[recorded!.args.indexOf("--setting-sources") + 1]).toBe("");
		// Byte for byte what native Ultron renders.
		const native = await nativeSections(project);
		expect(prompt).toContain(native.project_context!);
		expect(prompt).toContain(native.skills!);
	}, 60_000);

	test("--no-context-files and --no-skills leave them out, for the root and (via ultron mcp) its subagents", () => {
		const noContext = launch(["--no-context-files", "-p", "hi"]).recorded!;
		expect(noContext.prompt).not.toContain(GLOBAL_AGENTS);
		expect(noContext.prompt).not.toContain(PROJECT_CLAUDE);
		expect(noContext.prompt).toContain("<name>fake-global-skill</name>");
		expect(JSON.parse(noContext.mcp!).mcpServers.ultron.args).toContain("--no-context-files");
		const neither = launch(["-nc", "-ns", "-p", "hi"]).recorded!;
		expect(neither.prompt).not.toContain("<skills>");
		expect(neither.prompt).not.toContain("<project_context>");
		expect(JSON.parse(neither.mcp!).mcpServers.ultron.args).toEqual(
			expect.arrayContaining(["--no-context-files", "--no-skills"]),
		);
		expect(parseMcpArgs(["--no-context-files", "--no-skills"])).toMatchObject({
			noContextFiles: true,
			noSkills: true,
		});
	}, 60_000);

	test("--print-config reports the prompt size, context file paths and skill names", () => {
		const { status, stdout } = launch(["--print-config", "-p", "x"]);
		expect(status).toBe(0);
		const printed = JSON.parse(stdout) as {
			systemPrompt: { chars: number; contextFiles: string[]; skills: string[] };
		};
		expect(printed.systemPrompt.contextFiles).toEqual([join(agentDir, "AGENTS.md"), join(project, "CLAUDE.md")]);
		expect(printed.systemPrompt.skills.sort()).toEqual(["fake-global-skill", "fake-project-skill"]);
		expect(printed.systemPrompt.chars).toBeGreaterThan(1000);
		const bare = JSON.parse(launch(["--print-config", "-nc", "-ns"]).stdout) as typeof printed;
		expect(bare.systemPrompt).toMatchObject({ contextFiles: [], skills: [] });
		expect(bare.systemPrompt.chars).toBeLessThan(printed.systemPrompt.chars);
	}, 60_000);

	test("a distrusted project loads no project skills, as in the session worker", async () => {
		expect((await loadClaudePromptResources(project, {}, agentDir)).skills.map((s) => s.filePath)).toContain(
			projectSkill,
		);
		new ProjectTrustStore(agentDir).set(project, false);
		expect(workerProjectTrusted(project, agentDir)).toBe(false);
		const distrusted = await loadClaudePromptResources(project, {}, agentDir);
		expect(distrusted.skills.map((skill) => skill.name)).toEqual(["fake-global-skill"]);
		const native = await nativeSections(project);
		expect(native.skills).not.toContain("fake-project-skill");
		// Context files do not depend on trust, natively either.
		expect(distrusted.contextFiles.map((file) => file.content)).toEqual([GLOBAL_AGENTS, PROJECT_CLAUDE]);
	});

	test("a Claude Code subagent's system prompt carries the same resources, and its children inherit the flags", async () => {
		const resources = await loadClaudePromptResources(project, {}, agentDir);
		const record = join(work, "child-record.json");
		const controller = new AbortController();
		const result = await runClaudeChild(
			{
				claude: fake,
				self: { command: "node", args: ["cli.js"] },
				cwd: project,
				parentSocket: join(work, "parent.sock"),
				parentName: "parent",
				model: "haiku",
				registerChild: () => () => {},
				env: { ...process.env, FAKE_RECORD: record },
				resources,
				resourceFlags: { noSkills: true },
			},
			{
				taskId: "t1",
				laneName: "child-1",
				prompt: "brief",
				level: 1,
				allowance: 0,
				signal: controller.signal,
				context: {} as never,
				deadlineAt: null,
				timeoutMs: 60_000,
				progress: () => {},
			},
		);
		expect(result.text).toBe("ok");
		const recorded = JSON.parse(readFileSync(record, "utf8")) as { prompt: string; mcp: string };
		expect(recorded.prompt).toContain("You are a subagent of Ultron");
		expect(recorded.prompt).toContain(GLOBAL_AGENTS);
		expect(recorded.prompt).toContain("<name>fake-global-skill</name>");
		expect(JSON.parse(recorded.mcp).mcpServers.ultron.args).toContain("--no-skills");
		expect(claudeSystemPrompt("claude-child", { resources })).toContain(
			claudeSystemPrompt("claude", { resources }).split("<project_context>")[1]!.split("</skills>")[0]!,
		);
	});
});
