/**
 * The Claude Code variants of the quality eval (`claude`: plain Claude Code; `claude-ultron`: `ultron claude`, Claude
 * Code as Ultron's root agent): their command lines keep the user's Claude Code setup out (no settings, hooks,
 * CLAUDE.md, MCP servers or skills) while keeping the login, and their transcripts are scored like the other variants.
 */
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdtempSync, rmSync } from "node:fs";
import { homedir, tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";
import { claudeModelArg, claudeStreamStats, claudeVariantArgs, claudeVariantEnv } from "./eval-quality.mjs";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");

test("claude-code/<alias> becomes Claude Code's own model alias", () => {
	assert.equal(claudeModelArg("claude-code/sonnet"), "sonnet");
	assert.equal(claudeModelArg("opus"), "opus");
});

test("plain Claude Code runs with its default tools but none of the user's setup", () => {
	const args = claudeVariantArgs("claude", { model: "claude-code/sonnet" });
	const after = (flag) => args[args.indexOf(flag) + 1];
	assert.equal(after("--model"), "sonnet");
	assert.equal(after("--setting-sources"), "");
	assert.equal(after("--input-format"), "stream-json");
	assert.equal(after("--permission-mode"), "bypassPermissions");
	for (const flag of ["--strict-mcp-config", "--disable-slash-commands", "--no-session-persistence", "-p"])
		assert.ok(args.includes(flag), flag);
	assert.ok(!args.includes("--tools"), "default tools stay");
});

test("claude-ultron passes Ultron's options before -- and Claude Code's after it", () => {
	const args = claudeVariantArgs("claude-ultron", { model: "claude-code/sonnet", frameModel: "cliproxyapi/glm" });
	const split = args.indexOf("--");
	assert.deepEqual(args.slice(0, split), ["--frame-model", "cliproxyapi/glm", "--child-model", "sonnet"]);
	assert.ok(args.slice(split).includes("-p"));
	assert.ok(!args.slice(split).includes("--setting-sources"), "ultron claude sets it itself");
});

test("HOME is the run's own; Claude Code's config dir (the login) is passed through, never copied", () => {
	const work = mkdtempSync(join(tmpdir(), "ultron-eval-claude-"));
	try {
		const env = claudeVariantEnv({ work, agentDir: join(work, "agent"), baseEnv: { PATH: "/bin" } });
		assert.equal(env.HOME, join(work, "home"));
		assert.equal(env.CLAUDE_CONFIG_DIR, join(homedir(), ".claude"));
		assert.equal(env.ULTRON_CODING_AGENT_DIR, join(work, "agent"));
		const custom = claudeVariantEnv({ work, agentDir: join(work, "agent"), baseEnv: { CLAUDE_CONFIG_DIR: "/cfg" } });
		assert.equal(custom.CLAUDE_CONFIG_DIR, "/cfg");
	} finally {
		rmSync(work, { recursive: true, force: true });
	}
});

test("stream-json transcripts give tool calls (the REPL as rlm), frames, cost and first-request tokens", () => {
	const lines = [
		JSON.stringify({ type: "system", subtype: "init" }),
		JSON.stringify({
			type: "assistant",
			message: {
				usage: { input_tokens: 3, cache_creation_input_tokens: 3000, cache_read_input_tokens: 0 },
				content: [
					{
						type: "tool_use",
						id: "t1",
						name: "mcp__ultron__rlm",
						input: { code: "r = await rlm.map('x', items)\nh = await rlm.spawn('b', name='c')" },
					},
				],
			},
		}),
		// The same block repeated by a later message is counted once.
		JSON.stringify({
			type: "assistant",
			message: { content: [{ type: "tool_use", id: "t1", name: "mcp__ultron__rlm", input: {} }] },
		}),
		JSON.stringify({ type: "assistant", message: { content: [{ type: "tool_use", id: "t2", name: "Bash", input: {} }] } }),
		JSON.stringify({
			type: "result",
			num_turns: 3,
			total_cost_usd: 0.25,
			usage: { input_tokens: 10, cache_creation_input_tokens: 100, cache_read_input_tokens: 1000, output_tokens: 50 },
		}),
		"not json",
	];
	const stats = claudeStreamStats(lines);
	assert.deepEqual(stats.toolsByName, { rlm: 1, Bash: 1 });
	assert.equal(stats.toolCalls, 2);
	assert.equal(stats.frameCalls, 2);
	assert.equal(stats.spawnCalls, 1);
	assert.equal(stats.cost, 0.25);
	assert.equal(stats.tokens, 1160);
	assert.equal(stats.firstRequestInputTokens, 3003);
});

test("an unknown variant is a usage error, never a paid run", () => {
	const run = spawnSync(process.execPath, ["scripts/eval-quality.mjs", "--tasks", "default", "--variants", "claude,nope"], {
		cwd: root,
		encoding: "utf8",
	});
	assert.equal(run.status, 2);
	assert.match(run.stderr, /Unknown variants: nope/);
});
