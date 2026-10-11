// Generates crates/interactive/fixtures/conversation-parity.json from packages/coding-agent interactive components (TypeScript,
// this checkout): UserMessage, AssistantMessage, Compaction/BranchSummary, ToolExecution, BashExecution, Footer,
// renderDiff and jsdiff diffWords, the rlm cell renderer (experimental/rlm-tool-renderer.ts), rendered with the TS theme (dark truecolor, light 256color) at several widths.
// Usage: node crates/interactive/fixtures/generate.mjs   (deterministic; output is written next to this file)
process.env.FORCE_COLOR = "3";
delete process.env.PI_EXPERIMENTAL;
process.env.HOME = "/home/u";
import { writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { createRequire } from "node:module";
import { fileURLToPath, pathToFileURL } from "node:url";
const here = dirname(fileURLToPath(import.meta.url));
const ca = (p) => import(join(here, "../../../packages/coding-agent/src", p));
const comp = (p) => ca(`modes/interactive/components/${p}`);
const T = await ca("modes/interactive/theme/theme.ts");
const { validateThemeJson } = await ca("modes/interactive/theme/theme-json.ts");
T.setThemeJsonValidator(validateThemeJson);
const { KeybindingsManager } = await ca("core/keybindings.ts");
// The components import "@ultron/tui" as resolved from packages/coding-agent; use that same module instance so the
// keybinding and capability globals set here are the ones the components read.
const tuiUrl = pathToFileURL(createRequire(join(here, "../../../packages/coding-agent/package.json")).resolve("@ultron/tui")).href;
const { setKeybindings, setCapabilities, Text } = await import(tuiUrl);
const { UserMessageComponent } = await comp("user-message.ts");
const { AssistantMessageComponent } = await comp("assistant-message.ts");
const { CompactionSummaryMessageComponent } = await comp("compaction-summary-message.ts");
const { BranchSummaryMessageComponent } = await comp("branch-summary-message.ts");
const { ToolExecutionComponent } = await comp("tool-execution.ts");
const { BashExecutionComponent } = await comp("bash-execution.ts");
const { FooterComponent } = await comp("footer.ts");
const { renderDiff } = await comp("diff.ts");
const { keyText, formatKeyText, keyDisplayText } = await comp("keybinding-hints.ts");
const { rlmToolRenderers } = await ca("experimental/rlm-tool-renderer.ts");
const Diff = await import("diff");

setCapabilities({ images: null, trueColor: true, hyperlinks: false });
setKeybindings(new KeybindingsManager({}));
const themeDir = join(here, "../../../packages/coding-agent/src/modes/interactive/theme");
const THEMES = [["dark", "truecolor"], ["light", "256color"]];
const WIDTHS = [12, 30, 80];
const ui = { requestRender() {} };
const out = { widths: WIDTHS, themes: THEMES.map(([n, m]) => `${n}-${m}`), keys: {}, user: [], assistant: [], summary: [], diff: [], words: [], tool: [], bash: [], footer: [], rlm: [] };

out.keys = {
	expand: keyText("app.tools.expand"), cancel: keyText("tui.select.cancel"), interrupt: keyText("app.interrupt"),
	display: keyDisplayText("app.tools.expand"),
	format: ["alt+enter/ctrl+j", "shift+tab", "", "a+b/c"].map((k) => [k, formatKeyText(k), formatKeyText(k, { capitalize: true })]),
};

const each = (fn) => THEMES.map(([name, mode]) => {
	T.setThemeInstance(T.loadThemeFromPath(join(themeDir, `${name}.json`), mode));
	return WIDTHS.map((w) => fn(w));
});

const USER = [
	["Hello world", 1],
	["1. first\n2. second\n\nSome **bold**, `code` and \\*escaped\\* text", 1],
	["A long user message that keeps going so that it needs to wrap across several lines at narrow widths.", 2],
	["```\ncode block\n  indented\n```\n\n> quote", 0],
	["", 1],
	["   ", 1],
];
for (const [text, pad] of USER) out.user.push({ text, pad, outputs: each((w) => new UserMessageComponent(text, undefined, pad).render(w)) });

const A = (content, stopReason = "stop", errorMessage) => ({ role: "assistant", content, stopReason, errorMessage, api: "x", provider: "x", model: "x", usage: {}, timestamp: 0 });
const tx = (text) => ({ type: "text", text });
const th = (thinking) => ({ type: "thinking", thinking });
const tc = { type: "toolCall", id: "t1", name: "rlm", arguments: {} };
const ASSISTANT = [
	{ m: A([tx("  Plain **answer** with `code`.  ")]) },
	{ m: A([th("Let me think\nabout *this*."), tx("Done.")]) },
	{ m: A([th("one"), th("   "), th("two"), tx("after")]), hide: true, label: "Thinking..." },
	{ m: A([th("one"), th("two"), tx("after")]), hide: false },
	{ m: A([th("hidden then shown"), tx("x"), th("second run")]), hide: true, label: "[thinking]", overrides: [[1, false]] },
	{ m: A([th("only thinking")]), hide: false },
	{ m: A([tx("calling"), tc], "toolUse") },
	{ m: A([th("pre"), tc], "toolUse") },
	{ m: A([tx("partial")], "length") },
	{ m: A([tx("partial"), tc], "length") },
	{ m: A([tx("cut")], "aborted", "Request was aborted") },
	{ m: A([], "aborted", "Custom abort reason") },
	{ m: A([tx("x")], "aborted") },
	{ m: A([tc], "aborted", "boom") },
	{ m: A([tx("bad")], "error", "Rate limited") },
	{ m: A([], "error") },
	{ m: A([tc], "error", "boom") },
	{ m: A([tx("   ")]) },
	{ m: A([tx("- list\n- items\n\n# Heading")]), pad: 2 },
	{ m: A([th("- a\n- b")]), pad: 0 },
];
for (const c of ASSISTANT) {
	out.assistant.push({ ...c, outputs: each((w) => {
		const a = new AssistantMessageComponent(undefined, c.hide ?? false, undefined, c.label ?? "Thinking...", c.pad ?? 1);
		for (const [k, v] of c.overrides ?? []) a.thinkingVisibilityOverrides.set(k, v);
		a.updateContent(c.m);
		return a.render(w);
	}) });
}

const SUMMARIES = [
	{ kind: "compaction", tokensBefore: 123456789, summary: "## Goal\n- did things\n- **more**", expanded: false },
	{ kind: "compaction", tokensBefore: 999, summary: "## Goal\n- did things\n- **more**", expanded: true },
	{ kind: "compaction", tokensBefore: 1000, summary: "", expanded: true },
	{ kind: "branch", summary: "Explored `a` then *b*.", expanded: false },
	{ kind: "branch", summary: "Explored `a` then *b*, which was a long detour through the codebase.", expanded: true },
];
for (const c of SUMMARIES) {
	out.summary.push({ ...c, outputs: each((w) => {
		const msg = { role: c.kind === "branch" ? "branchSummary" : "compactionSummary", summary: c.summary, tokensBefore: c.tokensBefore, fromId: "x", timestamp: 0 };
		const s = c.kind === "branch" ? new BranchSummaryMessageComponent(msg) : new CompactionSummaryMessageComponent(msg);
		s.setExpanded(c.expanded);
		return s.render(w);
	}) });
}

const DIFFS = [
	"-1 const a = 1;\n+1 const a = 2;\n 2 ok",
	"-3 foo bar baz\n+3 foo baz",
	"-3 foo bar baz\n+3 foo qux baz",
	"-4     return x\n+4   return y;",
	"-5 one\n-6 two\n+5 three",
	"+7 added only\n+8\tTabbed",
	"     ...\n 9 context\n@@ -1 +1 @@\n+bad",
	"-10 \tindent\tx\n+10 \tindent\ty",
	"-11 foo\n+11 foo   bar",
	"",
	"-12 héllo wörld, 日本\n+12 héllo world, 日本!",
	"+  12x\n-  3y\n \t4z",
];
for (const d of DIFFS) out.diff.push({ diff: d, outputs: THEMES.map(([name, mode]) => { T.setThemeInstance(T.loadThemeFromPath(join(themeDir, `${name}.json`), mode)); return renderDiff(d); }) });

const WORDS = [
	["foo bar baz", "foo baz"], ["foo bar baz", "foo qux baz"], ["foo\nbar baz", "foo baz"], ["foo baz", "foo\nbar baz"],
	["foo   bar baz", "foo  baz"], ["foo\tbar\nbaz", "foo baz"], ["", "new words"], ["old words", ""], ["same", "same"],
	["  lead", "lead  "], ["a.b(c)", "a.d(c, e)"], ["x = 1;", "y = 2;"], ["the quick brown fox", "a quick red fox jumps"],
	["café au lait", "cafe au lait"], ["one two three four five", "five four three two one"], [" ", "  "], ["a  b", "a b"], ["snake_case_name = 1", "snake_case_other = 1"], ["x9 y", "x8 y"],
];
for (const [a, b] of WORDS) out.words.push({ old: a, new: b, changes: Diff.diffWords(a, b).map((c) => ({ value: c.value, added: !!c.added, removed: !!c.removed })) });

// 1x1 PNG
const PNG = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";
const many = Array.from({ length: 14 }, (_, i) => `line ${i + 1}`).join("\n");
const callR = (args, theme) => new Text(theme.fg("accent", `call ${args.code ?? "?"}`), 0, 0);
const resultR = (result, opts, theme) => new Text(theme.fg("toolOutput", `res expanded=${opts.expanded} partial=${opts.isPartial} ${result.content.map((c) => c.text ?? c.type).join("|")}`), 0, 0);
const thrower = () => { throw new Error("x"); };
const DEFS = { none: undefined, empty: {}, both: { renderCall: callR, renderResult: resultR }, self: { renderShell: "self", renderCall: callR, renderResult: resultR }, throws: { renderCall: thrower, renderResult: thrower }, selfEmpty: { renderShell: "self", renderCall: () => new Text("", 0, 0) } };
const TOOLS = [
	{ def: "none", args: { code: "print(1)", n: 2 } },
	{ def: "none", args: { code: "x" }, result: { content: [{ type: "text", text: "out \x1b[31mred\x1b[0m\r\nnext" }], isError: false } },
	{ def: "none", args: {}, result: { content: [{ type: "text", text: "Traceback" }], isError: true } },
	{ def: "none", args: { a: [1, "b", null, true] }, result: { content: [{ type: "image", data: PNG, mimeType: "image/png" }, { type: "text", text: "with image" }], isError: false } },
	{ def: "empty", args: { code: "x" } },
	{ def: "empty", args: { code: "x" }, result: { content: [{ type: "text", text: many }], isError: false } },
	{ def: "empty", args: { code: "x" }, result: { content: [{ type: "text", text: many }], isError: false }, expanded: true },
	{ def: "empty", args: { code: "x" }, result: { content: [{ type: "text", text: "partial" }], isError: false }, partial: true },
	{ def: "empty", args: {}, result: { content: [], isError: false } },
	{ def: "both", args: { code: "print(2)" } },
	{ def: "both", args: { code: "print(2)" }, result: { content: [{ type: "text", text: "2" }], isError: true } },
	{ def: "both", args: { code: "p" }, result: { content: [{ type: "text", text: "2" }], isError: false }, expanded: true, partial: true },
	{ def: "self", args: { code: "s" }, result: { content: [{ type: "text", text: "ok" }], isError: false } },
	{ def: "self", args: { code: "s" } },
	{ def: "selfEmpty", args: {} },
	{ def: "throws", args: { code: "t" }, result: { content: [{ type: "text", text: "fallback\ntext" }], isError: false } },
];
for (const c of TOOLS) {
	out.tool.push({ ...c, outputs: each((w) => {
		const t = new ToolExecutionComponent("rlm", "id1", c.args, {}, DEFS[c.def], ui, "/tmp");
		if (c.expanded) t.setExpanded(true);
		if (c.result) t.updateResult(c.result, c.partial ?? false);
		return t.render(w);
	}) });
}

const lines = (n) => Array.from({ length: n }, (_, i) => `out ${i + 1}`).join("\n");
const BASH = [
	{ cmd: "ls", steps: [] },
	{ cmd: "ls", exclude: true, steps: [] },
	{ cmd: "echo hi", steps: [["append", "hi\n"], ["complete", 0, false]] },
	{ cmd: "printf", steps: [["append", "par"], ["append", "tial\nnext \x1b[32mgreen\x1b[0m\r\nend"], ["complete", 0, false]] },
	{ cmd: "seq 25", steps: [["append", lines(25)], ["complete", 0, false]] },
	{ cmd: "seq 25", steps: [["append", lines(25)], ["expand"], ["complete", 0, false]] },
	{ cmd: "sleep 9", steps: [["complete", undefined, true]] },
	{ cmd: "false", steps: [["append", "err"], ["complete", 2, false]] },
	{ cmd: "big", steps: [["append", "tail"], ["complete", 0, false, true, "/tmp/full.log"]] },
	{ cmd: "big", steps: [["append", "tail"], ["complete", 0, false, false, "/tmp/full.log"]] },
	{ cmd: "running", steps: [["append", "so far"]] },
	{ cmd: "exactly 50KB", steps: [["append", Array.from({ length: 1024 }, () => "y".repeat(49)).join("\n") + "\n"], ["complete", 0, false, false, "/tmp/f.log"]] },
	{ cmd: "over 50KB", steps: [["append", Array.from({ length: 1025 }, () => "y".repeat(49)).join("\n") + "\n"], ["complete", 0, false, false, "/tmp/f.log"]] },
	{ cmd: "!! quiet", exclude: true, steps: [["append", "x"], ["complete", 0, false]] },
	{ cmd: "a very long command line that will need to wrap around", steps: [["append", "a fairly long output line that wraps at narrow widths too"], ["complete", 0, false]] },
];
for (const c of BASH) {
	out.bash.push({ ...c, outputs: each((w) => {
		const b = new BashExecutionComponent(c.cmd, ui, c.exclude ?? false);
		for (const s of c.steps) {
			if (s[0] === "append") b.appendOutput(s[1]);
			else if (s[0] === "expand") b.setExpanded(true);
			else b.setComplete(s[1], s[2], s[3] ? { truncated: true } : undefined, s[4]);
		}
		const r = b.render(w);
		b.loader.stop();
		return r;
	}) });
}

// rlm cells. Syntax highlighting is recorded separately (`hl`: the highlighter's input and output per theme) and
// injected into the Rust renderer, which does not port cli-highlight.
const code15 = Array.from({ length: 15 }, (_, i) => `x${i} = ${i}`).join("\n");
const text = (t) => ({ content: [{ type: "text", text: t }], isError: false });
const RLM = [
	{ args: {} },
	{ args: {}, complete: true },
	{ args: { code: "   " } },
	{ args: { code: "print(1)" } },
	{ args: { code: "def f():\n\treturn 1\n\nprint(f())  \n\n" }, result: text("1\n") },
	{ args: { code: code15 } },
	{ args: { code: code15 }, expanded: true },
	{ args: { code: Array.from({ length: 13 }, (_, i) => `y${i}`).join("\n") } },
	{ args: { code: "x" }, result: text(lines(25)) },
	{ args: { code: "x" }, result: text(lines(11)) },
	{ args: { code: "x" }, result: text(lines(10)) },
	{ args: { code: "x" }, result: text(lines(25)), expanded: true },
	{ args: { code: "x" }, result: text(""), partial: true },
	{ args: { code: "x" }, result: text("  \n "), partial: false },
	{ args: { code: "x" }, result: text("step 1\nstep 2"), partial: true },
	{ args: { code: "x" }, result: { content: [{ type: "text", text: "Traceback (most recent call last):\nValueError: bad" }], isError: true } },
	{ args: { code: "x" }, result: text("a\tb\tc\n" + "w".repeat(70) + "\nshort") },
	{ args: { code: "x" }, result: text("one \x1b[31mred\x1b[0m\r\ntwo\n" + lines(12)) },
	{ args: { code: "x" }, result: text("日本語のテキスト ".repeat(6) + "\n" + lines(9)) },
	{ args: { code: "x" }, result: { content: [{ type: "text", text: "plot" }, { type: "image", data: PNG, mimeType: "image/png" }], isError: false } },
];
for (const c of RLM) {
	const code = c.args.code;
	const hl = THEMES.map(([name, mode]) => {
		T.setThemeInstance(T.loadThemeFromPath(join(themeDir, `${name}.json`), mode));
		if (code === undefined || code.trim().length === 0) return null;
		const input = code.replace(/\s+$/u, "").replace(/\t/g, "    ");
		return { input, lines: T.highlightCode(input, "python") };
	});
	out.rlm.push({ ...c, hl, outputs: each((w) => {
		const t = new ToolExecutionComponent("rlm", "id1", c.args, {}, rlmToolRenderers, ui, "/tmp");
		if (c.complete) t.setArgsComplete();
		if (c.expanded) t.setExpanded(true);
		if (c.result) t.updateResult(c.result, c.partial ?? false);
		return t.render(w);
	}) });
}

const FOOTERS = [
	{ cwd: "/home/u/proj", model: { id: "claude-x", provider: "anthropic", reasoning: true, contextWindow: 200000 }, thinking: "high", usage: { input: 1234, output: 56789, cacheRead: 1500000, cacheWrite: 25, cost: 0.4567 }, percent: 12.345, providers: 2, branch: "main", name: "my session" },
	{ cwd: "/home/u", model: { id: "gpt", provider: "openai", reasoning: true, contextWindow: 128000 }, thinking: "off", usage: { input: 999, output: 0, cacheRead: 0, cacheWrite: 0, cost: 0 }, percent: 75, providers: 1 },
	{ cwd: "/srv/app", model: { id: "m", provider: "kimi-coding", reasoning: false, contextWindow: 12000000 }, usage: { input: 25000000, output: 10000, cacheRead: 0, cacheWrite: 0, cost: 0 }, percent: 95.05, providers: 3, autoCompact: false },
	{ cwd: "/home/u/x", model: { id: "sonnet", provider: "claude-code", reasoning: true, contextWindow: 1000000 }, thinking: "medium", usage: { input: 10, output: 20, cacheRead: 30, cacheWrite: 40, cost: 1.0005 }, percent: null, providers: 2, statuses: [["b", "beta\nstatus"], ["a", "  alpha\t\ttab  "]] },
	{ cwd: "/home/u", model: { id: "abcdefghijklmnop", provider: "p", reasoning: false, contextWindow: 0 }, usage: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, cost: 0 }, providers: 1 },
	{ cwd: "/home/user2", usage: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, cost: 0 }, percent: undefined, providers: 0 },
	{ cwd: "/home/u/deep/nested/path/that/is/long", model: { id: "a-very-long-model-identifier-name", provider: "openrouter", reasoning: true, contextWindow: 1048576 }, thinking: "xhigh", usage: { input: 9999, output: 99999, cacheRead: 999999, cacheWrite: 9999999, cost: 0.0125 }, percent: 0.05, contextWindow: 400000, providers: 2, subscription: true, branch: "feature/x" },
];
for (const c of FOOTERS) {
	out.footer.push({ ...c, outputs: each((w) => {
		const usage = { ...c.usage, cost: { total: c.usage.cost } };
		const session = {
			state: { model: c.model, thinkingLevel: c.thinking },
			sessionManager: { getEntries: () => [{ type: "message", message: { role: "assistant", usage } }], getCwd: () => c.cwd, getSessionName: () => c.name },
			getContextUsage: () => (c.percent === undefined ? undefined : { percent: c.percent, contextWindow: c.contextWindow ?? c.model?.contextWindow ?? 0 }),
			modelRuntime: { isUsingSubscription: () => c.subscription ?? false },
		};
		const data = { getGitBranch: () => c.branch ?? null, getAvailableProviderCount: () => c.providers, getExtensionStatuses: () => new Map(c.statuses ?? []) };
		const f = new FooterComponent(session, data);
		f.setAutoCompactEnabled(c.autoCompact ?? true);
		return f.render(w);
	}) });
}

writeFileSync(join(here, "conversation-parity.json"), `${JSON.stringify(out, null, 1)}\n`);
