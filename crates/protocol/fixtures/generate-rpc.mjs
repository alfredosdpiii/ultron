// Generates crates/protocol/fixtures/rpc.json from the TypeScript RPC mode (packages/coding-agent/src/modes/rpc):
// serializeJsonLine output for one literal of every RpcCommand variant, response and extension UI response shape
// (as rpc-mode.ts builds them), plus attachJsonlLineReader line splits over scripted byte chunks.
// Usage: node crates/protocol/fixtures/generate-rpc.mjs [output.json]
import { writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { PassThrough } from "node:stream";
import { fileURLToPath, pathToFileURL } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "../../..");
const { serializeJsonLine, attachJsonlLineReader } = await import(
	pathToFileURL(join(root, "packages/coding-agent/src/modes/rpc/jsonl.ts")).href
);

const image = { type: "image", data: "aGk=", mimeType: "image/png" };
const commands = [
	{ id: "1", type: "prompt", message: "hi there", images: [image], streamingBehavior: "steer" },
	{ type: "prompt", message: "plain" },
	{ type: "prompt", message: "q", streamingBehavior: "followUp" },
	{ id: "s", type: "steer", message: "go", images: [] },
	{ type: "follow_up", message: "later" },
	{ type: "abort" }, { id: "c", type: "clear_queue" }, { type: "new_session", parentSession: "/tmp/p.jsonl" }, { type: "new_session" },
	{ type: "get_state" }, { type: "set_model", provider: "anthropic", modelId: "claude" }, { type: "cycle_model" },
	{ type: "get_available_models" }, { type: "set_thinking_level", level: "high" }, { type: "cycle_thinking_level" },
	{ type: "get_available_thinking_levels" }, { type: "set_steering_mode", mode: "all" },
	{ type: "set_follow_up_mode", mode: "one-at-a-time" }, { type: "compact", customInstructions: "short" }, { type: "compact" },
	{ type: "set_auto_compaction", enabled: true }, { type: "set_auto_retry", enabled: false }, { type: "abort_retry" },
	{ id: "b", type: "bash", command: "echo \"x\"\ttab", excludeFromContext: true }, { type: "bash", command: "ls" }, { type: "abort_bash" },
	{ type: "get_session_stats" }, { type: "export_html", outputPath: "/tmp/o.html" }, { type: "export_html" },
	{ type: "switch_session", sessionPath: "/tmp/s.jsonl" }, { type: "fork", entryId: "e1" }, { type: "clone" },
	{ type: "get_fork_messages" }, { type: "get_entries", since: "e0" }, { type: "get_entries" }, { type: "get_tree" },
	{ type: "get_last_assistant_text" }, { type: "set_session_name", name: "名前 \u0001" }, { type: "get_messages" }, { type: "get_commands" },
];
const responses = [
	{ id: "1", type: "response", command: "prompt", success: true },
	{ type: "response", command: "clear_queue", success: true, data: { steering: ["a"], followUp: [] } },
	{ id: "x", type: "response", command: "fork", success: true, data: { text: "t", cancelled: false } },
	{ id: "y", type: "response", command: "nope", success: false, error: "Unknown command: nope" },
	{ type: "response", command: "export_html", success: true, data: { path: "/tmp/o.html" } },
];
const uiResponses = [
	{ type: "extension_ui_response", id: "u1", value: "picked" },
	{ type: "extension_ui_response", id: "u2", confirmed: true },
	{ type: "extension_ui_response", id: "u3", cancelled: true },
];

async function readLines(chunks) {
	const stream = new PassThrough();
	const lines = [];
	attachJsonlLineReader(stream, (line) => lines.push(line));
	const done = new Promise((resolve) => stream.on("end", resolve));
	for (const chunk of chunks) stream.write(Buffer.from(chunk, "hex"));
	stream.end();
	stream.resume();
	await done;
	await new Promise((resolve) => setImmediate(resolve));
	return lines;
}
const h = (s) => Buffer.from(s, "utf8").toString("hex");
const euro = h("€");
const readerScripts = [
	[h('{"a":1}\n{"b":2}\n')],
	[h('{"a":1}\r\n'), h("tail")],
	[h("x y z\n")],
	[h("pre") + euro.slice(0, 2), euro.slice(2) + h("post\n")],
	[h("\n\n"), h("\r\n")],
	[h("no newline at end")],
	[h("a\rb\n")],
	[h("ok\n") + "ff" + h("\n")],
	[],
];
const reader = [];
for (const chunks of readerScripts) reader.push({ chunks, lines: await readLines(chunks) });

const text = `${JSON.stringify({
	source: "packages/coding-agent/src/modes/rpc",
	commands: commands.map(serializeJsonLine),
	responses: responses.map(serializeJsonLine),
	uiResponses: uiResponses.map(serializeJsonLine),
	reader,
}, null, 1)}\n`;
writeFileSync(process.argv[2] ?? join(root, "crates/protocol/fixtures/rpc.json"), text);
