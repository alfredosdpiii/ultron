// Generates crates/mcp/fixtures/runtime.json from the TypeScript MCP extension: config value resolution
// (resolveConfigValueOrThrow, resolveHeadersOrThrow), createDefaultTransport's stdio options (~ expansion, env),
// convertMcpResult with a recording saver, isMcpAppResource/listed, and consumeSseStream over chunked input.
// Usage: node crates/mcp/fixtures/generate-runtime.mjs   (from the repository root)
import { writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, "../../..");
const imp = (p) => import(pathToFileURL(join(root, p)).href);
const { resolveConfigValueOrThrow, resolveHeadersOrThrow } = await imp("packages/coding-agent/src/core/resolve-config-value.ts");
const { createDefaultTransport } = await imp("packages/coding-agent/src/extensions/mcp/runtime.ts");
const { convertMcpResult } = await imp("packages/coding-agent/src/extensions/mcp/tools.ts");
const { isMcpAppResource, listed } = await imp("packages/coding-agent/src/extensions/mcp/resources.ts");
const { consumeSseStream } = await imp("packages/mcp/src/transports/streamable-http.ts");

process.env.HOME = "/home/fixture";
const ENV = { MCPRT_A: "alpha", MCPRT_B: "beta", MCPRT_EMPTY: "" };
for (const [k, v] of Object.entries(ENV)) process.env[k] = v;
const attempt = (f) => {
	try {
		return { ok: f() ?? null };
	} catch (e) {
		return { err: e.message };
	}
};

const values = [
	"plain", "$MCPRT_A", "${MCPRT_A}-${MCPRT_B}", "x$MCPRT_A.y", "$$MCPRT_A", "$!bang", "cost $5", "${", "${not-a-name}",
	"$", "a$", "$MCPRT_MISSING", "$MCPRT_MISSING_1 $MCPRT_MISSING_2 $MCPRT_MISSING_1", "$MCPRT_EMPTY", "${MCPRT_A",
	"!echo  hello  ", "!printf ''", "!exit 3", "!echo $MCPRT_B", "!echo out; echo err >&2", "", "$_u", "${_X9}",
];
const resolveCases = values.map((value) => ({ value, ...attempt(() => resolveConfigValueOrThrow(value, "thing")) }));
const headerCases = [
	{ headers: { Authorization: "Bearer $MCPRT_A", "X-Key": "${MCPRT_B}" } },
	{ headers: {} },
	{ headers: { "X-Ok": "v", "X-Bad": "$MCPRT_NOPE" } },
].map((c) => ({ ...c, ...attempt(() => resolveHeadersOrThrow(c.headers, 'MCP server "s"')) }));

const stdioEntries = [
	{ command: "~/bin/server", args: ["~", "~/data", "x~/y", "~user"], cwd: "~/work", env: { TOKEN: "$MCPRT_A" } },
	{ command: "server", inheritEnv: false },
	{ command: "/abs/server", cwd: "sub/dir", env: { A: "lit" } },
	{ command: "~" },
	{ command: "s", cwd: "../up/./x" },
	{ command: "s", env: { BAD: "${MCPRT_NONE}" } },
];
const transportCases = stdioEntries.map((config) => ({
	config,
	...attempt(() => {
		const { options } = createDefaultTransport({ name: "srv", config }, "/proj/root", undefined);
		return JSON.parse(JSON.stringify(options));
	}),
}));

const blocks = [
	{ type: "text", text: "hi" },
	{ type: "resource_link", uri: "file:///a.txt", name: "a", mimeType: "text/plain", size: 2048, description: "the a" },
	{ type: "resource_link", uri: "x://b", name: "b", title: "Bee" },
	{ type: "resource_link", uri: "x://c", name: "c", title: null, size: 5, mimeType: "" },
	{ type: "resource", resource: { uri: "file:///doc.json", mimeType: "application/json; charset=utf-8", blob: Buffer.from('{"a":1}').toString("base64") } },
	{ type: "resource", resource: { uri: "file:///img.png", mimeType: "image/png", blob: "iVBO" } },
	{ type: "resource", resource: { uri: "https://h/dir/file.tar.gz?q=1", mimeType: "application/gzip", blob: Buffer.alloc(3000, 7).toString("base64") } },
	{ type: "resource", resource: { uri: "mem://noext", blob: "AAEC" } },
	{ type: "resource", resource: { uri: "mem://h/file.abcdefghij", blob: "AAEC" } },
	{ type: "resource", resource: { uri: "mem://x.verylongext", mimeType: "application/octet-stream", blob: "AAE" } },
	{ type: "resource", resource: { uri: "file:///t", mimeType: "text/markdown", text: "# md" } },
	{ type: "resource", resource: { uri: "file:///s.svg", mimeType: "image/svg+xml", blob: "PHN2Zy8+" } },
	{ type: "resource", resource: { uri: "file:///a.xml", mimeType: "application/atom+xml", blob: "PGEvPg==" } },
	{ type: "audio", data: "x", mimeType: "audio/wav" },
];
const results = [
	{ content: blocks },
	{ content: [], structuredContent: { ok: true, n: [1, 2] } },
	{ content: [], isError: true },
	{ content: [{ type: "image", data: "AA==", mimeType: "image/png" }], isError: true },
	{ content: [{ type: "text", text: "bad" }], isError: true },
	{ content: [{ type: "resource", resource: { uri: "file:///f.bin", mimeType: "application/zip", blob: "UEsD" } }] },
];
const convertCases = [];
for (const [i, result] of results.entries()) {
	for (const readable of [false, true]) {
		const saved = [];
		const failing = i === 5 && readable;
		const saveOutput = async (data, extension) => {
			if (failing) throw new Error("disk full");
			saved.push({ data: Buffer.from(data).toString("base64"), extension });
			return `/saved/${saved.length}${extension}`;
		};
		const out = await convertMcpResult("srv", "tool", result, { saveOutput, readableResources: readable });
		convertCases.push({ result, readable, failing, out, saved });
	}
}

const appItems = [
	{ uri: "ui://widget" }, { uriTemplate: "ui://t/{id}" }, { uri: "file:///a", mimeType: "text/html;profile=mcp-app" },
	{ uri: "file:///a", mimeType: 'text/html; Profile = "mcp-app"' }, { uri: "file:///a", mimeType: "text/html" },
	{ uri: "file:///a", mimeType: "text/html; profile=other" }, { uri: "file:///ui://" }, {},
];
const appCases = appItems.map((item) => ({ item, app: isMcpAppResource(item) }));
const listedCases = [
	{ uri: "a", name: "n", _meta: { x: 1 }, icons: [], mimeType: "t" },
	{ server: "other", uriTemplate: "t/{x}" },
].map((item) => ({ item, out: listed("srv", item) }));

const sseInputs = [
	["data: {\"a\":1}\n\n"],
	["event: message\nid: 7\ndata: one\ndata: two\n\n", "retry: 1500\nretry: x\n: comment\n\n"],
	["da", "ta: split\r\n\r", "\n"],
	["id: 1\n\n", "event: ping\ndata:nospace\n\nfield-only\ndata\n\n"],
	["data: tail-without-blank"],
	["data: t"],
	["data:  two\n\n"],
	["data: a\n", "data: b"],
	["id: a\0b\ndata: z\n\n"],
	["data: " + "x".repeat(20) + "\n\n"],
	["data: 12345\ndata: 12345\n\n"],
	["data: é\n", "\n"],
];
const sseCases = [];
for (const chunks of sseInputs) {
	const events = [];
	const encoder = new TextEncoder();
	const stream = new ReadableStream({
		start(controller) {
			for (const chunk of chunks) controller.enqueue(encoder.encode(chunk));
			controller.close();
		},
	});
	let error = null;
	try {
		await consumeSseStream(stream, {
			maxEventBytes: 12,
			onEvent: (e) => events.push({ kind: "event", ...e }),
			onId: (id) => events.push({ kind: "id", id }),
			onRetry: (ms) => events.push({ kind: "retry", ms }),
		});
	} catch (e) {
		error = e.message;
	}
	sseCases.push({ chunks, events, error });
}

const fixture = { env: ENV, home: "/home/fixture", resolveCases, headerCases, transportCases, convertCases, appCases, listedCases, sseCases };
writeFileSync(join(here, "runtime.json"), `${JSON.stringify(fixture, null, "\t")}\n`);
