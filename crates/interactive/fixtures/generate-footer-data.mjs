// Records crates/interactive/fixtures/footer-data.json: the client TUI's footer (experimental/client-tui-footer.ts
// `NativeFooter`, Pi's FooterComponent over a FooterDataProvider) for working directories in git layouts — a branch,
// a linked worktree (`gitdir:` file and `commondir`), a detached HEAD, a reftable `.invalid` HEAD answered by a fake
// `git` (and one that fails), no repository — and for Claude Code transcripts (`claudeCodeUsageText`, `(sub)`),
// dark theme, truecolor. Each case lists the files written under a private temp root (also HOME), the cwd, the PATH
// `git` is looked up on, the session view the footer reads and the rendered lines.
// crates/interactive/tests/footer_data.rs replays it through the Rust App.
// Usage: node crates/interactive/fixtures/generate-footer-data.mjs
process.env.FORCE_COLOR = "3";
import "../../cli/fixtures/offline-guard.mjs";
import { chmodSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const ca = (p) => import(join(here, "../../../packages/coding-agent/src", p));
const T = await ca("modes/interactive/theme/theme.ts");
const { validateThemeJson } = await ca("modes/interactive/theme/theme-json.ts");
T.setThemeJsonValidator(validateThemeJson);
T.setThemeInstance(T.loadThemeFromPath(join(here, "../../../packages/coding-agent/src/modes/interactive/theme/dark.json"), "truecolor"));
const { NativeFooter, claudeCodeUsageText } = await ca("experimental/client-tui-footer.ts");
delete process.env.PI_EXPERIMENTAL;

const usage = (input, output, cost) => ({
	input,
	output,
	cacheRead: 0,
	cacheWrite: 0,
	totalTokens: input + output,
	cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: cost },
});
const user = (text) => ({ role: "user", content: text, timestamp: 1 });
const assistant = (provider, model, extra = {}) => ({
	role: "assistant",
	content: [{ type: "text", text: "ok" }],
	api: "anthropic-messages",
	provider,
	model,
	usage: usage(1200, 340, 0.0123),
	stopReason: "stop",
	timestamp: 2,
	...extra,
});
const windows = (status, w) => ({
	diagnostics: [{ type: "claude_code_usage", timestamp: 0, details: { ...(status ? { status } : {}), windows: w } }],
});
const anthropic = { provider: "anthropic", id: "claude-sonnet-4-5", name: "Claude Sonnet 4.5", reasoning: true, contextWindow: 200000 };
const claudeCode = { provider: "claude-code", id: "claude-opus-5-5", name: "Claude Opus 5.5", reasoning: true, contextWindow: 200000 };
const openai = { provider: "openai", id: "gpt-5", name: "GPT-5", reasoning: true, contextWindow: 400000 };
const fakeGit = (out, code) => `#!/bin/sh\n${out ? `printf '%s\\n' '${out}'\n` : ""}exit ${code}\n`;

const cases = [
	{ name: "branch", files: { "repo/.git/HEAD": "ref: refs/heads/feature/x\n", "repo/sub/.keep": "" }, cwd: "repo/sub", model: anthropic, models: [anthropic, openai], sessionName: "demo", messages: [user("hi"), assistant("anthropic", anthropic.id)] },
	{ name: "worktree", files: { "repo/.git/HEAD": "ref: refs/heads/main\n", "repo/.git/worktrees/wt/HEAD": "ref: refs/heads/topic\n", "repo/.git/worktrees/wt/commondir": "../..\n", "wt/.git": "gitdir: ../repo/.git/worktrees/wt\n" }, cwd: "wt", model: anthropic, models: [anthropic], messages: [] },
	{ name: "detached", files: { "repo/.git/HEAD": "0123456789abcdef0123456789abcdef01234567\n" }, cwd: "repo", model: anthropic, models: [anthropic], messages: [] },
	{ name: "reftable", files: { "repo/.git/HEAD": "ref: refs/heads/.invalid\n", "repo/.git/reftable/tables.list": "", "bin/git": fakeGit("reftable-main", 0) }, path: "bin", cwd: "repo", model: anthropic, models: [anthropic], messages: [] },
	{ name: "reftable-git-fails", files: { "repo/.git/HEAD": "ref: refs/heads/.invalid\n", "bin/git": fakeGit("", 1) }, path: "bin", cwd: "repo", model: anthropic, models: [anthropic], messages: [] },
	{ name: "gitdir-without-head", files: { "repo/.git": "gitdir: ./missing\n" }, cwd: "repo", model: anthropic, models: [anthropic], messages: [] },
	{ name: "no-repo", files: { "plain/.keep": "" }, cwd: "plain", model: anthropic, models: [anthropic], messages: [] },
	{ name: "claude-code-usage", files: { "plain/.keep": "" }, cwd: "plain", model: claudeCode, models: [claudeCode, anthropic], messages: [user("hi"), assistant("claude-code", claudeCode.id, windows("allowed", { five_hour: { utilization: 0.09 }, seven_day: { utilization: 0.615, resetsAt: 1 }, seven_day_opus: { utilization: 0.2 }, weekly_extra_x: { utilization: 0.005 } }))] },
	{ name: "claude-code-limit-reached", files: { "plain/.keep": "" }, cwd: "plain", model: claudeCode, models: [claudeCode], messages: [assistant("claude-code", claudeCode.id, windows("rejected", { five_hour: { utilization: 1 } }))] },
	{ name: "claude-code-falls-back-to-an-earlier-report", files: { "plain/.keep": "" }, cwd: "plain", model: claudeCode, models: [claudeCode], messages: [assistant("claude-code", claudeCode.id, windows(undefined, { five_hour: { utilization: 0.3 } })), assistant("claude-code", claudeCode.id), assistant("anthropic", anthropic.id, windows(undefined, { five_hour: { utilization: 0.9 } }))] },
	{ name: "claude-code-no-numbers", files: { "plain/.keep": "" }, cwd: "plain", model: claudeCode, models: [claudeCode], messages: [assistant("claude-code", claudeCode.id, windows(undefined, { five_hour: { utilization: 0.3 } })), assistant("claude-code", claudeCode.id, windows(undefined, { five_hour: {} }))] },
];

const savedPath = process.env.PATH;
const recorded = [];
for (const c of cases) {
	const root = mkdtempSync(join(tmpdir(), "ultron-footer-"));
	for (const [path, content] of Object.entries(c.files)) {
		mkdirSync(dirname(join(root, path)), { recursive: true });
		writeFileSync(join(root, path), content);
		if (path.startsWith("bin/")) chmodSync(join(root, path), 0o755);
	}
	process.env.HOME = root;
	process.env.PATH = c.path ? join(root, c.path) : "/nonexistent";
	const ref = { provider: c.model.provider, modelId: c.model.id };
	const snapshot = { transcript: c.messages.map((message) => ({ type: "message", message })), configuration: { model: ref, thinkingLevel: "medium" } };
	const models = {
		configuration: { model: ref, thinkingLevel: "medium" },
		catalog: {
			availableModels: c.models.map((m) => ({ provider: m.provider, modelId: m.id, name: m.name, reasoning: m.reasoning, model: m })),
		},
	};
	const footer = new NativeFooter(join(root, c.cwd), { snapshot: () => snapshot, models: () => models, sessionName: () => c.sessionName });
	const lines = footer.render(100);
	footer.dispose();
	recorded.push({ ...c, path: c.path ?? null, usageText: claudeCodeUsageText(snapshot.transcript) ?? null, lines });
	rmSync(root, { recursive: true, force: true });
}
process.env.PATH = savedPath;
writeFileSync(join(here, "footer-data.json"), `${JSON.stringify({ width: 100, cases: recorded }, null, 1)}\n`);
console.log(`wrote ${recorded.length} cases`);
