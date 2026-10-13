// Records the TypeScript client TUI's slash commands and keys that the Rust TUI ports after the first batch
// (interactive.mjs): `!`/`!!` bash, /compact, /thinking and the cycling keys, /fork, /clone, /export, /usage, and
// the toggles. Same harness as interactive.mjs (tmux, scripted Anthropic SSE server, private HOME /
// ULTRON_CODING_AGENT_DIR / ULTRON_SERVER_DIR temp dirs, screens normalized); a scenario may also write `files`
// into the temp root first and read `artifacts` (paths under the root, normalized) after its last step.
// crates/cli/tests/interactive_cmds.rs replays them against the Rust binary.
// Usage: node crates/cli/fixtures/interactive_cmds.mjs [scenario-name...]
import { execFileSync } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { createServer } from "node:http";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { normalize } from "./interactive.mjs";

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, "../../..");
const out = join(here, "interactive_cmds.json");

function sse(events) {
	return events.map((e) => `event: ${e.type}\ndata: ${JSON.stringify(e)}\n\n`);
}
const start = (id) => ({ type: "message_start", message: { id, type: "message", role: "assistant", model: "claude-sonnet-4-5", content: [], stop_reason: null, usage: { input_tokens: 10, output_tokens: 1 } } });
function text(parts, hold = false) {
	const chunks = sse([
		start("msg_1"),
		{ type: "content_block_start", index: 0, content_block: { type: "text", text: "" } },
		...parts.map((t) => ({ type: "content_block_delta", index: 0, delta: { type: "text_delta", text: t } })),
		{ type: "content_block_stop", index: 0 },
		{ type: "message_delta", delta: { stop_reason: "end_turn" }, usage: { output_tokens: 5 } },
		{ type: "message_stop" },
	]);
	return hold ? { chunks, holdAfter: 3 } : { chunks };
}
function thinking(thought, answer) {
	const chunks = sse([
		start("msg_3"),
		{ type: "content_block_start", index: 0, content_block: { type: "thinking", thinking: "" } },
		{ type: "content_block_delta", index: 0, delta: { type: "thinking_delta", thinking: thought } },
		{ type: "content_block_delta", index: 0, delta: { type: "signature_delta", signature: "sig" } },
		{ type: "content_block_stop", index: 0 },
		{ type: "content_block_start", index: 1, content_block: { type: "text", text: "" } },
		{ type: "content_block_delta", index: 1, delta: { type: "text_delta", text: answer } },
		{ type: "content_block_stop", index: 1 },
		{ type: "message_delta", delta: { stop_reason: "end_turn" }, usage: { output_tokens: 5 } },
		{ type: "message_stop" },
	]);
	return { chunks };
}

const COLS = 100;
const ROWS = 30;
const S = { wait: "Session:" };
const enter = (type, wait, extra = {}) => ({ type, keys: ["Enter"], wait, ...extra });

export const scenarios = [
	{
		name: "bash-output",
		replies: [],
		steps: [S, enter("!printf 'line-one\\nline-two\\n'", "line-two"), enter("!!printf 'quiet-%s' out", "quiet-out")],
	},
	{
		name: "bash-exit-code",
		replies: [],
		steps: [S, enter("!echo failing; exit 3", "(exit 3)")],
	},
	{
		name: "bash-cancel",
		replies: [],
		steps: [S, enter("!sleep 30", "Running..."), { keys: ["Escape"], wait: "(cancelled)" }],
	},
	{
		name: "bash-in-context",
		replies: [text(["Seen it."])],
		steps: [S, enter("!echo context-line", "context-line"), enter("what did it print", "Seen it.")],
	},
	{
		name: "bash-empty",
		replies: [],
		steps: [S, { type: "!", wait: "!" }, { keys: ["Enter"], wait: "(no more replies)" }],
	},
	{
		name: "compact",
		replies: [text(["Hi."]), text(["The summary of it all."])],
		steps: [S, enter("hello", "Hi."), enter("/compact", "compaction")],
	},
	{
		name: "compact-then-prompt",
		replies: [text(["Hi."]), text(["Summary."]), text(["After."])],
		steps: [S, enter("hello", "Hi."), enter("/compact keep it short", "compaction"), enter("again", "After.")],
	},
	{
		name: "compact-empty",
		replies: [],
		steps: [S, enter("/compact", "compact")],
	},
	{
		name: "thinking-selector",
		replies: [],
		steps: [S, enter("/thinking", "Select thinking level"), { keys: ["Down", "Enter"], wait: "Thinking level:" }],
	},
	{
		name: "thinking-arg",
		replies: [],
		steps: [S, enter("/thinking high", "Thinking level: high"), enter("/thinking nope", "Unknown thinking level")],
	},
	{
		name: "thinking-cycle",
		replies: [],
		steps: [S, { keys: ["BTab"], wait: "Thinking level:" }, { keys: ["BTab"], wait: "Thinking level: off" }],
	},
	{
		name: "model-cycle",
		replies: [],
		steps: [S, { keys: ["C-p"], wait: "Switched to" }],
	},
	{
		name: "thinking-toggle",
		replies: [thinking("Pondering deeply.", "Answer.")],
		steps: [S, enter("think", "Answer."), { keys: ["C-t"], wait: "Thinking blocks: hidden" }, { keys: ["C-t"], wait: "Thinking blocks: visible" }],
	},
	{
		name: "tools-expand",
		replies: [],
		steps: [S, { keys: ["C-o"], wait: "Tool output: expanded" }, { keys: ["C-o"], wait: "Tool output: collapsed" }],
	},
	{
		name: "fork",
		replies: [text(["One."]), text(["Two."])],
		steps: [S, enter("first ask", "One."), enter("second ask", "Two."), enter("/fork", "second ask"), { keys: ["Up", "Enter"], wait: "Forked to new session" }],
	},
	{
		name: "fork-empty",
		replies: [],
		steps: [S, enter("/fork", "No messages to fork from")],
	},
	{
		name: "clone",
		replies: [text(["Cloned reply."])],
		steps: [S, enter("only ask", "Cloned reply."), enter("/clone", "Cloned to new session")],
	},
	{
		name: "clone-empty",
		replies: [],
		steps: [S, enter("/clone", "Nothing to clone")],
	},
	{
		name: "export-jsonl",
		replies: [text(["Exported."])],
		steps: [S, enter("export me", "Exported."), enter("/export out.jsonl", "Session exported to")],
		artifacts: ["cwd/out.jsonl"],
	},
	{
		name: "export-html",
		replies: [text(["Exported."])],
		steps: [S, enter("export me", "Exported."), enter("/export page.html", "Session exported to")],
	},
	{
		name: "history-search",
		replies: [text(["A."]), text(["B."])],
		steps: [
			S,
			enter("first prompt", "A."),
			enter("second prompt", "B."),
			{ keys: ["M-r"], wait: "second prompt" },
			{ type: "fir", wait: "fir" },
			{ keys: ["Enter"], wait: "first prompt" },
		],
	},
	{
		name: "history-search-empty",
		replies: [],
		steps: [S, { keys: ["M-r"], wait: "No prompt history yet" }],
	},
	{
		name: "tree",
		replies: [text(["One."]), text(["Two."])],
		steps: [
			S,
			enter("first ask", "One."),
			enter("second ask", "Two."),
			enter("/tree", "Session Tree"),
			{ keys: ["Up"], wait: "(3/4)" },
			{ keys: ["Up"], wait: "(2/4)" },
			{ keys: ["Enter"], wait: "Summarize branch?" },
			{ keys: ["Enter"], wait: "Navigated to selected point" },
		],
	},
	{
		name: "tree-branches",
		replies: [text(["One."]), text(["Two."]), text(["Three."])],
		steps: [
			S,
			enter("first ask", "One."),
			enter("second ask", "Two."),
			enter("/tree", "Session Tree"),
			{ keys: ["Up", "Up", "Enter"], wait: "Summarize branch?" },
			{ keys: ["Enter"], wait: "Navigated to selected point" },
			{ type: "third ask", wait: "third ask" },
			{ keys: ["Enter"], wait: "Three." },
			enter("/tree", "Session Tree"),
			{ type: "ask", wait: "Type to search: ask" },
			{ keys: ["C-u"], wait: "[user]" },
			{ keys: ["Escape"], wait: "Type to search:" },
			{ keys: ["Escape"], wait: "Three." },
		],
	},
	{
		name: "tree-filters",
		replies: [text(["One."])],
		steps: [
			S,
			enter("only ask", "One."),
			enter("/tree", "Session Tree"),
			{ keys: ["C-a"], wait: "[all]" },
			{ keys: ["C-o"], wait: "(" },
			{ keys: ["C-d"], wait: "(" },
			{ keys: ["Escape"], wait: "One." },
		],
	},
	{
		name: "tree-summarize",
		replies: [text(["One."]), text(["Two."]), text(["Left branch summary."])],
		steps: [
			S,
			enter("first ask", "One."),
			enter("second ask", "Two."),
			enter("/tree", "Session Tree"),
			{ keys: ["Up", "Up", "Enter"], wait: "Summarize branch?" },
			{ keys: ["Down", "Enter"], wait: "Navigated to selected point" },
			enter("/tree", "Session Tree"),
			{ keys: ["Escape"], wait: "Navigated" },
		],
	},
	{
		name: "compact-resume",
		replies: [text(["Hi."]), text(["Sum."])],
		steps: [S, enter("hello", "Hi."), enter("/compact", "compaction"), enter("/new", "New session started"), enter("/resume", "Resume Session"), { keys: ["Down", "Enter"], wait: "compaction" }],
	},
	{
		name: "resume-selector",
		replies: [text(["Alpha done."]), text(["Beta done."]), text(["Gamma done."])],
		steps: [
			S,
			enter("alpha task", "Alpha done."),
			enter("/new", "New session started"),
			enter("beta task", "Beta done."),
			enter("/name Beta named", "Session name set"),
			enter("/new", "New session started"),
			enter("gamma task", "Gamma done."),
			enter("/resume", "Resume Session"),
			{ type: "alp", wait: "> alp" },
			{ keys: ["BSpace", "BSpace", "BSpace"], wait: "Beta named" },
			{ keys: ["C-s"], wait: "Sort: Recent" },
			{ keys: ["C-s"], wait: "Sort: Fuzzy" },
			{ keys: ["C-s"], wait: "Sort: Threaded" },
			{ keys: ["C-n"], wait: "Name: Named" },
			{ keys: ["C-n"], wait: "Name: All" },
			{ keys: ["Down", "C-r"], wait: "Rename Session" },
			{ keys: ["C-u"], wait: "Rename Session" },
			{ type: "Renamed beta", wait: "Renamed beta" },
			{ keys: ["Enter"], wait: "Name: All" },
			{ keys: ["Down", "C-d"], wait: "Cannot delete" },
			{ keys: ["Down", "C-d"], wait: "Delete session?" },
			{ keys: ["Enter"], wait: "Session moved to trash" },
			{ keys: ["Escape"], wait: "Gamma done." },
		],
	},
	{
		name: "resume-switch",
		replies: [text(["Alpha done."]), text(["Beta done."])],
		steps: [
			S,
			enter("alpha task", "Alpha done."),
			enter("/new", "New session started"),
			enter("beta task", "Beta done."),
			enter("/resume", "Resume Session"),
			{ keys: ["Enter"], wait: "Already in this session" },
			enter("/resume", "Resume Session"),
			{ keys: ["Down", "Enter"], wait: "Resumed session" },
		],
	},
	{
		name: "settings",
		rows: 40,
		replies: [],
		steps: [
			S,
			enter("/settings", "Type to search"),
			{ keys: ["Down"], wait: "Auto-compact" },
			{ keys: ["Enter"], wait: "Auto-compact" },
			{ type: "hide", wait: "Hide thinking" },
			{ keys: ["Enter"], wait: "Hide thinking" },
			{ keys: ["BSpace", "BSpace", "BSpace", "BSpace"], wait: "Models" },
			{ type: "http", wait: "HTTP idle timeout" },
			{ keys: ["Enter"], wait: "HTTP idle timeout:" },
			{ keys: ["Escape"], wait: "Session:" },
			enter("/settings", "Type to search"),
			{ type: "theme", wait: "Theme" },
			{ keys: ["Enter"], wait: "Select a theme" },
			{ keys: ["Escape"], wait: "Type to search" },
			{ keys: ["BSpace", "BSpace", "BSpace", "BSpace", "BSpace"], wait: "Models" },
			{ type: "warn", wait: "Warnings" },
			{ keys: ["Enter"], wait: "Anthropic extra usage" },
			{ keys: ["Enter"], wait: "Saved warnings" },
			{ keys: ["Escape"], wait: "Type to search" },
			{ keys: ["Escape"], wait: "Session:" },
		],
	},
	{
		name: "settings-models",
		rows: 40,
		replies: [],
		steps: [
			S,
			enter("/settings", "Type to search"),
			{ keys: ["Enter"], wait: "Session model" },
			{ keys: ["Down", "Enter"], wait: "Same as session model" },
			{ type: "haiku", wait: "haiku" },
			{ keys: ["Enter"], wait: "Frame model:" },
			{ keys: ["Escape"], wait: "Type to search" },
			{ keys: ["Escape"], wait: "Session:" },
		],
	},
	{
		name: "settings-thinking",
		rows: 40,
		replies: [],
		steps: [
			S,
			enter("/settings", "Type to search"),
			{ type: "per model", wait: "Default thinking level per model" },
			{ keys: ["Enter"], wait: "Per-Model Thinking Level" },
			{ keys: ["Enter"], wait: "Thinking Level for" },
			{ keys: ["Down", "Enter"], wait: "Per-Model Thinking Level" },
			{ keys: ["Escape"], wait: "1 configured" },
			{ keys: ["Escape"], wait: "Session:" },
		],
	},
	{
		name: "rlm-panel",
		rows: 40,
		replies: [],
		steps: [S, { keys: ["C-r"], wait: "RLM" }, { keys: ["C-r"], wait: "Session:" }, enter("/rlm focus", "RLM"), { keys: ["Escape"], wait: "Session:" }, enter("/rlm pane", "RLM")],
	},
	{
		name: "external-editor",
		env: { EDITOR: "sed -i s/draft/edited/" },
		replies: [],
		steps: [S, { type: "my draft text", wait: "my draft text" }, { keys: ["C-g"], wait: "my edited text" }],
	},
	{
		name: "external-editor-missing",
		env: { EDITOR: "/nonexistent/editor" },
		replies: [],
		steps: [S, { type: "still here", wait: "still here" }, { keys: ["C-g"], wait: "still here" }],
	},
	{
		name: "external-editor-fails",
		env: { EDITOR: "false" },
		replies: [],
		steps: [S, { type: "keep me", wait: "keep me" }, { keys: ["C-g"], wait: "keep me" }],
	},
	{
		name: "logout-empty",
		replies: [],
		steps: [S, enter("/logout", "No stored credentials")],
	},
	{
		name: "logout",
		files: { "agent/auth.json": JSON.stringify({ openai: { type: "api_key", key: "sk-test" }, anthropic: { type: "api_key", key: "sk-ant" } }) },
		replies: [],
		steps: [S, enter("/logout", "logout"), { keys: ["Down"], wait: "logout" }, { keys: ["Enter"], wait: "Removed stored API key" }],
	},
	{
		name: "share-no-gh",
		replies: [],
		steps: [S, enter("/share", "GitHub CLI")],
	},
	{
		name: "changelog",
		replies: [],
		steps: [S, enter("/changelog", "[0.87.39]")],
	},
	{
		name: "login-api-key",
		replies: [],
		steps: [
			S,
			enter("/login", "Select authentication method"),
			{ keys: ["Down"], wait: "API key" },
			{ keys: ["Enter"], wait: "Select provider to configure" },
			{ type: "groq", wait: "Groq" },
			{ keys: ["Enter"], wait: "Groq" },
			{ type: "gsk-test-key", wait: "Groq" },
			{ keys: ["Enter"], wait: "Saved API key" },
		],
		artifacts: ["agent/auth.json"],
	},
	{
		name: "review",
		rows: 40,
		replies: [text(["Review done."])],
		steps: [S, enter("/review main --post", "Review done.")],
	},
	{
		name: "tree-empty",
		replies: [],
		steps: [S, enter("/tree", "No entries in session")],
	},
	{
		name: "usage",
		replies: [text(["Counted."])],
		steps: [S, enter("count this", "Counted."), enter("/usage", "Session")],
	},
];

async function mockServer(replies) {
	let n = 0;
	let release;
	const server = createServer((req, res) => {
		req.resume();
		req.on("end", async () => {
			const reply = replies[n++] ?? text(["(no more replies)"]);
			res.writeHead(200, { "content-type": "text/event-stream" });
			const cut = reply.holdAfter ?? reply.chunks.length;
			for (const c of reply.chunks.slice(0, cut)) res.write(c);
			if (reply.holdAfter !== undefined) await new Promise((r) => (release = r));
			for (const c of reply.chunks.slice(cut)) res.write(c);
			res.end();
		});
	});
	await new Promise((r) => server.listen(0, "127.0.0.1", r));
	return { port: server.address().port, release: () => release?.(), close: () => server.close() };
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const tmux = (socket, ...args) => execFileSync("tmux", ["-S", socket, "-f", "/dev/null", ...args], { encoding: "utf8" });

/** JSONL artifacts: each line's object with ids, times and the temp dir normalized. */
function normalizeArtifact(content, tmp) {
	return normalize(content, tmp)
		.replace(/"timestamp":"[^"]*"/g, '"timestamp":"<T>"')
		.replace(/"timestamp":\d+/g, '"timestamp":0')
		.replace(/"(id|parentId)":"[0-9a-f]{8}"/g, '"$1":"<E>"');
}

async function runScenario(scenario, command) {
	const server = await mockServer(scenario.replies);
	const tmp = mkdtempSync("/tmp/ultron-tui-");
	const agentDir = join(tmp, "agent");
	const cwd = join(tmp, "cwd");
	mkdirSync(agentDir, { recursive: true });
	mkdirSync(cwd);
	mkdirSync(join(tmp, "server"));
	writeFileSync(join(agentDir, "models.json"), JSON.stringify({ providers: { anthropic: { baseUrl: `http://127.0.0.1:${server.port}` } } }));
	for (const [path, content] of Object.entries(scenario.files ?? {})) {
		mkdirSync(dirname(join(tmp, path)), { recursive: true });
		writeFileSync(join(tmp, path), content);
	}
	const env = {
		PATH: "/usr/bin:/bin",
		HOME: tmp,
		ULTRON_CODING_AGENT_DIR: agentDir,
		ULTRON_SERVER_DIR: join(tmp, "server"),
		ANTHROPIC_API_KEY: "test-key",
		TZ: "UTC",
		TERM: "xterm-256color",
		COLORTERM: "truecolor",
		LANG: "C.UTF-8",
		ULTRON_AUTO_NAME: "off",
		ULTRON_SPLASH: "off",
		...(scenario.env ?? {}),
	};
	const script = join(tmp, "run.sh");
	const quoted = (s) => `'${s.replace(/'/g, "'\\''")}'`;
	writeFileSync(
		script,
		`#!/bin/sh\ncd ${quoted(cwd)}\nexec env -i ${Object.entries(env).map(([k, v]) => quoted(`${k}=${v}`)).join(" ")} ${command.map(quoted).join(" ")}\n`,
		{ mode: 0o755 },
	);
	const socket = join(tmp, "tmux.sock");
	const cols = scenario.cols ?? COLS;
	const rows = scenario.rows ?? ROWS;
	tmux(socket, "new-session", "-d", "-s", "t", "-x", String(cols), "-y", String(rows), `${script}; echo '<exited>'; sleep 60`);
	const capture = () => tmux(socket, "capture-pane", "-p", "-t", "t");
	const captures = [];
	const artifacts = {};
	try {
		for (const step of scenario.steps) {
			if (step.release) server.release();
			if (step.type !== undefined) tmux(socket, "send-keys", "-t", "t", "-l", step.type);
			if (step.keys) tmux(socket, "send-keys", "-t", "t", ...step.keys);
			const deadline = Date.now() + 30_000;
			let screen = capture();
			while (!screen.includes(step.wait)) {
				if (Date.now() > deadline) throw new Error(`${scenario.name}: timed out waiting for ${step.wait}\n${screen}`);
				await sleep(20);
				screen = capture();
			}
			let stable = normalize(screen, tmp);
			let since = Date.now();
			while (Date.now() - since < 400) {
				await sleep(50);
				const next = normalize(capture(), tmp);
				if (next !== stable) {
					stable = next;
					since = Date.now();
				}
			}
			captures.push(stable);
		}
		for (const path of scenario.artifacts ?? []) {
			const file = join(tmp, path);
			artifacts[path] = existsSync(file) ? normalizeArtifact(readFileSync(file, "utf8"), tmp) : null;
		}
	} finally {
		try {
			tmux(socket, "kill-server");
		} catch {}
		server.close();
		await sleep(300);
		rmSync(tmp, { recursive: true, force: true });
	}
	return { captures, artifacts };
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
	const ts = [process.execPath, join(root, "packages/coding-agent/src/cli.ts"), "--model", "anthropic/claude-sonnet-4-5"];
	const only = process.argv.slice(2);
	const recorded = existsSync(out) && only.length > 0 ? JSON.parse(readFileSync(out, "utf8")) : { scenarios: [] };
	for (const scenario of scenarios) {
		if (only.length > 0 && !only.includes(scenario.name)) continue;
		const { captures, artifacts } = await runScenario(scenario, ts);
		const entry = { ...scenario, cols: scenario.cols ?? COLS, rows: scenario.rows ?? ROWS, captures, artifacts };
		const index = recorded.scenarios.findIndex((s) => s.name === scenario.name);
		if (index >= 0) recorded.scenarios[index] = entry;
		else recorded.scenarios.push(entry);
		process.stderr.write(`${scenario.name}: ${captures.length} captures\n`);
	}
	recorded.scenarios.sort((a, b) => scenarios.findIndex((s) => s.name === a.name) - scenarios.findIndex((s) => s.name === b.name));
	writeFileSync(out, `${JSON.stringify(recorded, null, 1)}\n`);
}
