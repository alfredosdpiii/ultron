// Records the TypeScript native command (main.ts `readPipedStdin`, native-command.ts `runNativeUltronCommand`,
// cli/file-processor.ts) against a scripted Anthropic SSE server: a prompt without -p (streamed text), runs without
// a prompt on a pipe (the server's sessions, an attached session), piped stdin as the prompt, @file text and images,
// a missing @file, and the session flags --system-prompt, --no-skills and --no-context-files (their effect on the
// request), plus the setup hints after a failed start. offline-guard.mjs is loaded into every TypeScript process
// (PI_OFFLINE=1); the generator fails if anything tried to leave loopback. crates/cli/tests/native_run.rs replays it.
// Usage: node crates/cli/fixtures/native_run.mjs   (from the repository root)
import { spawn } from "node:child_process";
import { mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { deflateSync } from "node:zlib";
import { assertOffline, guardNodeOptions } from "./offline-guard.mjs";

function textReply(text) {
	const events = [
		{ type: "message_start", message: { id: "msg_1", type: "message", role: "assistant", model: "claude-sonnet-4-5", content: [], stop_reason: null, usage: { input_tokens: 10, output_tokens: 1 } } },
		{ type: "content_block_start", index: 0, content_block: { type: "text", text: "" } },
		{ type: "content_block_delta", index: 0, delta: { type: "text_delta", text } },
		{ type: "content_block_stop", index: 0 },
		{ type: "message_delta", delta: { stop_reason: "end_turn" }, usage: { output_tokens: 5 } },
		{ type: "message_stop" },
	];
	return events.map((e) => `event: ${e.type}\ndata: ${JSON.stringify(e)}\n\n`).join("");
}

const root = new URL("../../../", import.meta.url).pathname;

function crc32(bytes) {
	let c = ~0;
	for (const b of bytes) {
		c ^= b;
		for (let k = 0; k < 8; k++) c = (c >>> 1) ^ (0xedb88320 & -(c & 1));
	}
	return ~c >>> 0;
}

/** A deterministic RGB PNG of `w` x `h`. */
export function png(w, h) {
	const chunk = (type, data) => {
		const len = Buffer.alloc(4);
		len.writeUInt32BE(data.length);
		const body = Buffer.concat([Buffer.from(type), data]);
		const crc = Buffer.alloc(4);
		crc.writeUInt32BE(crc32(body));
		return Buffer.concat([len, body, crc]);
	};
	const header = Buffer.alloc(13);
	header.writeUInt32BE(w, 0);
	header.writeUInt32BE(h, 4);
	header.set([8, 2, 0, 0, 0], 8);
	const rows = [];
	for (let y = 0; y < h; y++) {
		const row = [0];
		for (let x = 0; x < w; x++) row.push((x * 7) % 256, (y * 13) % 256, (x * y) % 256);
		rows.push(Buffer.from(row));
	}
	return Buffer.concat([
		Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
		chunk("IHDR", header),
		chunk("IDAT", deflateSync(Buffer.concat(rows), { level: 9 })),
		chunk("IEND", Buffer.alloc(0)),
	]);
}

function sessionIds(sessionsDir) {
	try {
		return readdirSync(sessionsDir, { recursive: true })
			.filter((f) => f.endsWith(".jsonl"))
			.map((f) => JSON.parse(readFileSync(join(sessionsDir, f), "utf8").split("\n")[0]).id);
	} catch {
		return [];
	}
}

/** One run in `tmp`: files written first (`base64:` content is binary), stdin piped when given. */
async function runCli(tmp, c, replies) {
	const requests = [];
	const server = createServer((req, res) => {
		let body = "";
		req.on("data", (d) => (body += d));
		req.on("end", () => {
			const parsed = JSON.parse(body || "null");
			// The session-naming request is answered apart and not recorded.
			if (JSON.stringify(parsed).includes("Name this coding session in 3-6 words")) {
				res.writeHead(200, { "content-type": "text/event-stream" });
				res.end(textReply("Scripted title"));
				return;
			}
			requests.push({ method: req.method, url: req.url, body: parsed });
			const reply = replies[requests.length - 1] ?? textReply("(no more replies)");
			res.writeHead(200, { "content-type": "text/event-stream" });
			res.end(reply);
		});
	});
	await new Promise((r) => server.listen(0, "127.0.0.1", r));
	const port = server.address().port;
	const agentDir = join(tmp, "agent");
	writeFileSync(join(agentDir, "models.json"), JSON.stringify({ providers: { anthropic: { baseUrl: `http://127.0.0.1:${port}` } } }));
	const env = {
		PATH: "/usr/bin:/bin",
		HOME: tmp,
		ULTRON_CODING_AGENT_DIR: agentDir,
		ULTRON_SERVER_DIR: join(tmp, "server"),
		NO_COLOR: "1",
		TZ: "UTC",
		PI_OFFLINE: "1",
		NODE_OPTIONS: guardNodeOptions,
		ULTRON_GUARD_LOG: join(tmp, "guard.log"),
		...(c.noKey ? {} : { ANTHROPIC_API_KEY: "test-key" }),
		...(c.env ?? {}),
	};
	const child = spawn(process.execPath, [join(root, "packages/coding-agent/src/cli.ts"), ...c.args], {
		cwd: join(tmp, "cwd"),
		env,
		stdio: [c.stdin === undefined ? "ignore" : "pipe", "pipe", "pipe"],
	});
	if (c.stdin !== undefined) child.stdin.end(c.stdin);
	let stdout = "";
	let stderr = "";
	child.stdout.on("data", (d) => (stdout += d));
	child.stderr.on("data", (d) => (stderr += d));
	const code = await new Promise((r) => child.on("close", r));
	server.close();
	return { code, stdout, stderr, requests };
}

const M = ["--model", "anthropic/claude-sonnet-4-5"];
const SKILL = "---\nname: demo-skill\ndescription: A demo skill for the fixture.\n---\nDo the demo.\n";
const cases = [
	{ name: "message-streams", args: [...M, "hello"], replies: [textReply("Hi there.")] },
	{ name: "two-messages-stream", args: [...M, "one", "two"], replies: [textReply("Both.")] },
	{ name: "stdin-print", args: ["-p", ...M], stdin: "piped question\n", replies: [textReply("Piped.")] },
	{ name: "stdin-and-message", args: ["-p", ...M, "the question"], stdin: "  some context \n\n", replies: [textReply("ok")] },
	{ name: "stdin-blank-no-prompt", args: [...M], stdin: "  \n", replies: [] },
	{ name: "no-prompt-lists-nothing", args: [...M], replies: [] },
	{ name: "no-prompt-continue-attaches", setup: [{ args: ["-p", ...M, "first"], replies: [textReply("one")] }], args: ["-c", ...M], replies: [] },
	{ name: "json-stdin", args: ["--mode", "json", ...M], stdin: "from a pipe", replies: [textReply("json answer")] },
	{ name: "file-text", files: { "cwd/notes.txt": "\ufeffline one\nline two\n" }, args: ["-p", ...M, "@notes.txt", "summarize"], replies: [textReply("Summary.")] },
	{ name: "file-text-only", files: { "cwd/sub/a.md": "# A\n" }, args: ["-p", ...M, "@./sub/../sub/a.md"], replies: [textReply("Read.")] },
	{ name: "file-empty-skipped", files: { "cwd/empty.txt": "" }, args: ["-p", ...M, "@empty.txt", "still asks"], replies: [textReply("ok")] },
	{ name: "file-stdin-and-message", files: { "cwd/n.txt": "N" }, args: ["-p", ...M, "@n.txt", "msg"], stdin: "piped", replies: [textReply("ok")] },
	{ name: "file-image", files: { "cwd/pic.png": `base64:${png(2, 2).toString("base64")}` }, args: ["-p", ...M, "@pic.png", "describe"], replies: [textReply("A picture.")] },
	{ name: "file-image-resized", files: { "cwd/wide.png": `base64:${png(2500, 10).toString("base64")}` }, args: ["-p", ...M, "@wide.png", "describe"], replies: [textReply("Wide.")] },
	{ name: "file-missing", args: ["-p", ...M, "@nope.txt", "x"], replies: [] },
	{ name: "system-prompt", args: ["-p", "--system-prompt", "You are terse.", ...M, "hi"], replies: [textReply("Hi.")] },
	{ name: "context-files", files: { "cwd/AGENTS.md": "Project rule: always say banana.\n" }, args: ["-p", ...M, "hi"], replies: [textReply("banana")] },
	{ name: "no-context-files", files: { "cwd/AGENTS.md": "Project rule: always say banana.\n" }, args: ["-p", "--no-context-files", ...M, "hi"], replies: [textReply("hi")] },
	{ name: "skills", files: { "agent/skills/demo-skill/SKILL.md": SKILL }, args: ["-p", ...M, "hi"], replies: [textReply("hi")] },
	{ name: "no-skills", files: { "agent/skills/demo-skill/SKILL.md": SKILL }, args: ["-p", "--no-skills", ...M, "hi"], replies: [textReply("hi")] },
	{ name: "hint-none", noKey: true, args: ["-p", "hi"], replies: [] },
	{ name: "hint-default-without-credentials", noKey: true, files: { "agent/settings.json": JSON.stringify({ defaultProvider: "anthropic", defaultModel: "claude-sonnet-4-5" }) }, args: ["-p", "hi"], replies: [] },
];

function normalize(value, tmp, ids) {
	let text = JSON.stringify(value)
		.split(tmp).join("<TMP>")
		.split(root.replace(/\/$/, "")).join("<root>")
		.split(tmp.slice(1).replace(/[/\\:]/g, "-")).join("<TMPNAME>")
		.replace(/\[clock\] \d{4}-\d\d-\d\d \d\d:\d\d UTC[+-]\d\d:\d\d(, \d+ (min|h|d) after the previous message)?/g, "[clock] <CLOCK>")
		.replace(/\\"(serverId|sessionId)\\":\\"[^"\\]*\\"/g, '\\"$1\\":\\"<ID>\\"');
	for (const [n, id] of ids.entries()) text = text.split(id).join(`<SESSION:${n}>`);
	return JSON.parse(text);
}

const out = [];
for (const c of cases) {
	const tmp = mkdtempSync(join(tmpdir(), "ultron-native-run-"));
	mkdirSync(join(tmp, "agent"), { recursive: true });
	mkdirSync(join(tmp, "cwd"));
	for (const [path, content] of Object.entries(c.files ?? {})) {
		mkdirSync(dirname(join(tmp, path)), { recursive: true });
		writeFileSync(join(tmp, path), content.startsWith("base64:") ? Buffer.from(content.slice(7), "base64") : content);
	}
	for (const step of c.setup ?? []) await runCli(tmp, step, step.replies);
	const ids = sessionIds(join(tmp, "agent", "experimental", "sessions"));
	const result = await runCli(tmp, c, c.replies);
	// The server id leads each line a run without a prompt prints.
	result.stdout = result.stdout.replace(/^[^\t\n]+\t/gm, "<SERVER>\t");
	assertOffline(join(tmp, "guard.log"));
	out.push({ name: c.name, args: c.args, stdin: c.stdin ?? null, noKey: c.noKey === true, files: c.files ?? {}, setup: c.setup ?? [], replies: c.replies, ...normalize(result, tmp, ids) });
	rmSync(tmp, { recursive: true, force: true });
}
writeFileSync(new URL("./native_run.json", import.meta.url), `${JSON.stringify(out, null, 1)}\n`);
