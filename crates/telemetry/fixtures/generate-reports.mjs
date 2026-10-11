// Generates crates/telemetry/fixtures/reports.json from this checkout's TypeScript sources:
// packages/coding-agent/src/core/bug-report.ts (redaction, metadata, diagnostics, report files, zip archive),
// packages/coding-agent/src/core/crash-log.ts (crash records on disk, extension stack attribution) and
// packages/coding-agent/src/core/provider-attribution.ts (telemetry-gated attribution headers).
// The clock, time zone, host details and environment are pinned so two runs produce identical output; nothing touches
// the network or the real agent directory (crash logs live in a private temp dir).
// Usage: node crates/telemetry/fixtures/generate-reports.mjs
import { mkdtempSync, readFileSync, rmSync, writeFileSync, existsSync } from "node:fs";
import { syncBuiltinESMExports } from "node:module";
import os from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, "../../..");
const scratch = mkdtempSync(join(os.tmpdir(), "telemetry-fixture-"));
process.env.ULTRON_CODING_AGENT_DIR = join(scratch, "agent");
process.env.ULTRON_SERVER_DIR = join(scratch, "server");
process.env.TZ = "UTC";

// Pinned clock: `new Date()` and `Date.now()` read `clock`.
let clock = Date.UTC(2026, 3, 5, 14, 7, 9, 321);
const RealDate = Date;
globalThis.Date = class extends RealDate {
	constructor(...args) {
		if (args.length === 0) super(clock);
		else super(...args);
	}
	static now() {
		return clock;
	}
};

// Pinned host.
os.release = () => "6.1.0-test";
os.version = () => "#1 SMP Test";
syncBuiltinESMExports();
Object.defineProperty(process, "version", { value: "v24.0.0" });
Object.defineProperty(process, "platform", { value: "linux" });
Object.defineProperty(process, "arch", { value: "x64" });

const load = (path) => import(pathToFileURL(join(root, path)).href);
const bug = await load("packages/coding-agent/src/core/bug-report.ts");
const crash = await load("packages/coding-agent/src/core/crash-log.ts");
const attribution = await load("packages/coding-agent/src/core/provider-attribution.ts");
const { VERSION } = await load("packages/coding-agent/src/config.ts");

const setEnv = (env) => {
	for (const name of Object.keys(process.env)) delete process.env[name];
	Object.assign(process.env, { TZ: "UTC" }, env);
};
const clone = (value) => (value === undefined ? null : JSON.parse(JSON.stringify(value)));

// ---- redaction ----
const urls = [
	"https://user:pass@proxy.example.com:8080/",
	"git:https://pat@github.com/org/repo",
	"https://api.example/v1?api-key=abc&model=x",
	"https://api.example/v1?model=x",
	"https://u:p@x.com:443/a?b=1#frag",
	"https://user@münchen.de/path",
	"https://HOST.example.COM/?Password=1&q=a b&r=%41%20c",
	"https://h.example/?token",
	"https://h.example/?token=a&other=1&token=b&TOKEN=c",
	"https://h.example/?myToken=1&tokens=2&token_id=3&x-api-key=4&apikey=5&client_secret=6&passwd=7&cookie_jar=8",
	"https://h.example/?accessToken=1&AuthorizationHeader=2&set-cookie=3&credentials=4&secretive=5",
	"https://h.example/?authToken=x#token=y",
	"ssh://git@github.com/org/repo.git",
	"git+ssh://git@github.com:org/repo.git",
	"npm:@scope/pkg",
	"mailto:someone@example.com",
	"file:///home/user/x.ts",
	"http://[::1]:8080/?password=p",
	"not a url",
	"",
	"https://",
	"a:b:https://u:p@x.com/",
	"git:https://pat@github.com/org/repo\nmore",
	"x:https://ok.example/",
	"https://ok.example/path?q=1",
	"HTTPS://Alice:pw@Example.com",
];
const redactUrlCases = urls.map((input) => ({ input, output: bug.redactUrl(input) }));

const jsonInputs = [
	{
		apiKey: "sk-123",
		headers: { Authorization: "Bearer x", "X-Trace": "1" },
		compaction: { reserveTokens: 16_384, keepRecentTokens: 20_000 },
		baseUrl: "https://me:secret@example.com/",
	},
	{
		nested: [{ password: { deep: 1 } }, "https://a:b@c.example/", 3, null, true, ["x", { token: null }]],
		myApiKey: "k",
		api_key: 0,
		"secret-store": false,
		SecretValue: [1, 2],
		tokenizer: "kept",
		maxTokens: 4096,
		authorizationUrl: "https://ok.example/?code=1&token=2",
		cookie: "",
		ratio: 0.75,
		v2Secret: "digit before an uppercase letter splits",
	},
	"https://u:p@root.example/",
	[{ credential: "c" }, "plain"],
	42,
	null,
];
const redactJsonCases = jsonInputs.map((input) => ({ input, output: clone(bug.redactJsonValue(input)) }));

// ---- metadata ----
const model = {
	id: "gpt-x",
	name: "GPT X",
	api: "openai-responses",
	provider: "proxy",
	baseUrl: "https://key:secret@proxy.example.com/v1?api_key=abc",
	reasoning: true,
	input: ["text", "image"],
	cost: { input: 1, output: 2, cacheRead: 0, cacheWrite: 0 },
	contextWindow: 200000,
	maxTokens: 32000,
	headers: { "X-Secret": "s", Authorization: "a", "A-First": "b" },
	samplingParams: { temperature: 0.2, apiKey: "nested" },
	compat: { supportsStore: false, authHeader: "https://u:p@c.example/" },
	thinkingLevelMap: { low: "minimal", high: "high" },
};
const bareModel = {
	id: "bare",
	name: "Bare",
	api: "anthropic-messages",
	provider: "missing",
	baseUrl: "https://api.example.com",
	reasoning: false,
	contextWindow: 0,
	maxTokens: 0,
};
const provider = {
	id: "proxy",
	name: "Proxy",
	baseUrl: "https://u:p@proxy.example.com/",
	headers: { "Z-Last": "1", "A-First": "2" },
	auth: { apiKey: { env: "PROXY_KEY" }, oauth: {} },
};
const providerMinimal = { id: "proxy", name: "Proxy", auth: {} };
const runtimeFor = (providerValue, status, oauth, registered) => ({
	getProvider: (id) => (providerValue && id === providerValue.id ? providerValue : undefined),
	getProviderAuthStatus: () => status,
	isUsingOAuth: () => oauth,
	getRegisteredProviderIds: () => (registered ? ["other", "proxy"] : ["other"]),
});
const extension = (path, source, origin, extra = {}) => ({
	path,
	resolvedPath: path,
	sourceInfo: { path, source, scope: "user", origin, baseDir: dirname(path) },
	...extra,
});
const extensions = [
	extension("/ext/a/index.ts", "git:https://pat@github.com/org/a", "package", { hidden: true }),
	extension("/ext/b.ts", "local", "top-level", { hidden: false }),
	extension("/ext/c.ts", "https://u:p@example.com/c.ts?token=1", "package"),
];
const globalSettings = {
	defaultProvider: "proxy",
	trackingId: "track-me",
	enableInstallTelemetry: true,
	compaction: { enabled: true, reserveTokens: 16384 },
	packages: ["npm:pkg", "git:https://tok@github.com/o/r"],
	providers: { proxy: { apiKey: "secret", baseUrl: "https://a:b@x.example/" } },
};
const projectSettings = { trackingId: "project-track", theme: "dark" };
const envs = [
	{
		SHELL: "/usr/bin/zsh",
		TERM: "xterm-256color",
		TERM_PROGRAM: "ghostty",
		TERM_PROGRAM_VERSION: "1.2",
		COLORTERM: "truecolor",
		TMUX: "/tmp/tmux",
		SSH_TTY: "/dev/pts/1",
		CI: "",
		PI_TELEMETRY: "0",
		PI_OFFLINE: "1",
		PI_CODING_AGENT_DIR: "/x",
		PIE: "no",
		pi_lower: "no",
	},
	{ SHELL: "C:\\Program Files\\Git\\bin\\bash.exe", TERM: "", SSH_CLIENT: "1.2.3.4", CI: "true" },
	{ SHELL: "/bin/", SSH_CONNECTION: "x" },
	{},
];
const metadataCases = [];
const metadataInputs = [
	{
		env: envs[0],
		options: { id: "report-1", hint: "  It broke\n  ", sessionId: "s1", cwd: "/work", includeSession: true, includeSummary: false, messageCount: 7, model, thinkingLevel: "high" },
		provider,
		status: { configured: true, source: "stored" },
		oauth: false,
		registered: true,
	},
	{
		env: envs[1],
		options: { id: "report-2", hint: "   ", sessionId: "s2", cwd: "/secret/cwd", includeSession: false, includeSummary: true, messageCount: 0, model, thinkingLevel: "off" },
		provider: providerMinimal,
		status: { configured: false },
		oauth: true,
		registered: false,
	},
	{
		env: envs[2],
		options: { id: "report-3", sessionId: "s3", cwd: "/w", includeSession: true, includeSummary: true, messageCount: 2, model: bareModel, thinkingLevel: "medium" },
		provider,
		status: { configured: true, source: "environment", label: "PROXY_KEY" },
		oauth: false,
		registered: false,
	},
	{
		env: envs[3],
		options: { id: "report-4", hint: "", sessionId: "s4", cwd: "/w", includeSession: false, includeSummary: false, messageCount: 1, thinkingLevel: "low" },
		provider,
		status: { configured: false },
		oauth: false,
		registered: false,
	},
];
for (const input of metadataInputs) {
	setEnv(input.env);
	const runtime = runtimeFor(input.provider, input.status, input.oauth, input.registered);
	const options = {
		...input.options,
		modelRuntime: runtime,
		extensions,
		extensionErrors: [{ path: "/ext/bad.ts", error: "SyntaxError: nope", extra: "dropped" }],
		globalSettings,
		projectSettings,
	};
	const output = bug.collectBugReportMetadata(options);
	metadataCases.push({
		env: input.env,
		options: { ...input.options },
		provider: runtime.getProvider(input.options.model?.provider ?? "") ?? null,
		status: input.status,
		usingOAuth: input.oauth,
		registered: input.registered,
		output: clone(output),
	});
}
const host = {
	version: VERSION,
	runtime: `node/${process.version}`,
	platform: process.platform,
	arch: process.arch,
	osRelease: os.release(),
	osVersion: os.version(),
	createdAt: new Date().toISOString(),
};

// ---- diagnostics ----
const entries = [
	{ type: "session", id: "h", timestamp: "t0" },
	{ type: "message", id: "u1", timestamp: "t1", message: { role: "user", content: "hi" } },
	{ type: "message", id: "a1", timestamp: "t2", message: { role: "assistant", provider: "p", model: "m", api: "x", stopReason: "stop", content: [] } },
	{ type: "message", id: "a2", timestamp: "t3", message: { role: "assistant", provider: "p", model: "m", api: "x", stopReason: "error", errorMessage: "boom", content: [] } },
	{ type: "message", id: "a3", timestamp: "t4", message: { role: "assistant", provider: "p", model: "m", api: "x", stopReason: "aborted", rawStopReason: "cancelled", content: [] } },
	{ type: "message", id: "a4", timestamp: "t5", message: { role: "assistant", provider: "p", model: "m", api: "x", stopReason: "stop", diagnostics: [{ type: "retry", attempt: 1 }], content: [] } },
	{ type: "message", id: "a5", timestamp: "t6", message: { role: "assistant", provider: "p", model: "m", api: "x", stopReason: "stop", errorMessage: "", diagnostics: [], content: [] } },
	{ type: "message", id: "a6", timestamp: "t7", message: { role: "assistant", stopReason: "length", errorMessage: "partial", rawStopReason: null, content: [] } },
	{ type: "custom", id: "c1", timestamp: "t8", customType: "x", data: {} },
	{ type: "message", id: "t1", timestamp: "t9", message: { role: "toolResult", content: [] } },
];
const crashes = [
	{ timestamp: "2026-04-01T00:00:00.000Z", version: "1", kind: "fatal_error", message: "m", stack: null, sessionFile: null, cwd: "/", notified: true },
	{ timestamp: "2026-04-02T00:00:00.000Z", version: "1", kind: "uncaught_exception", message: "n", stack: "s", sessionFile: "/f", cwd: "/w" },
];
const sessionManager = { getEntries: () => entries, getSessionId: () => "session-9" };
const diagnosticsCases = [
	{ entries, crashes, output: clone(bug.collectBugReportDiagnostics(sessionManager, crashes)) },
	{ entries: [], crashes: [], output: clone(bug.collectBugReportDiagnostics({ getEntries: () => [], getSessionId: () => "empty" })) },
];

// ---- report files and archive ----
const bundles = [
	{ metadata: metadataCases[0].output, diagnostics: diagnosticsCases[0].output, sessionJsonl: '{"a":1}\n', summary: "## Summary\nok" },
	{ metadata: { a: [], b: {}, c: "é\u2028\u0001" }, diagnostics: { x: 1.5 }, summary: "ends\n" },
	{ metadata: { a: 1 }, diagnostics: { b: 2 }, sessionJsonl: "" },
];
const fileCases = bundles.map((bundle) => ({ bundle, files: bug.bugReportFiles(bundle) }));
const archivePath = join(scratch, "report.zip");
await bug.writeBugReportArchive(bundles[0], archivePath);
const archive = readFileSync(archivePath).toString("base64");
const archiveName = bug.bugReportArchiveFileName("abc");

// ---- crash log ----
const crashPath = join(scratch, "nested", "dir", "crashes.json");
const read = () => (existsSync(crashPath) ? readFileSync(crashPath, "utf8") : null);
const makeError = (spec) => {
	if (spec.value !== undefined) return spec.value;
	const error = new Error(spec.message);
	error.name = spec.name;
	error.stack = spec.stack ?? "";
	return error;
};
const crashOps = [
	["take", Date.UTC(2026, 3, 5)],
	["record", { kind: "uncaught_exception", error: { name: "TypeError", message: "x is undefined", stack: "TypeError: x\n    at f (/a.js:1:1)" }, sessionFile: "/s/1.jsonl", cwd: "/w" }],
	["record", { kind: "fatal_error", error: { name: "RangeError", message: "", stack: "" }, cwd: "/w" }],
	["record", { kind: "fatal_error", error: { value: "plain string" }, cwd: "/w" }],
	["take", Date.UTC(2026, 3, 5, 15)],
	["take", Date.UTC(2026, 3, 5, 15)],
	["record", { kind: "uncaught_exception", error: { name: "Error", message: "four" }, cwd: "/w" }],
	["record", { kind: "uncaught_exception", error: { name: "Error", message: "five" }, cwd: "/w" }],
	["record", { kind: "uncaught_exception", error: { name: "Error", message: "six" }, cwd: "/w" }],
	["read"],
	["take", Date.UTC(2026, 3, 20)],
	["take", Date.UTC(2026, 3, 12, 14, 7, 9, 321)],
	["clear"],
	["read"],
	["clear"],
	["raw", "not json"],
	["read"],
	["raw", '{"timestamp":"x"}'],
	["read"],
	["raw", JSON.stringify([
		{ timestamp: "2026-04-05T10:00:00Z", message: "ok", extra: [1], notified: false },
		{ timestamp: 5, message: "bad" },
		null,
		"str",
		{ timestamp: "2026-04-05", message: "date only", notified: 0 },
		{ timestamp: "garbage", message: "unparseable" },
		{ timestamp: "2026-04-05T11:00:00.000+02:00", message: "offset" },
		{ message: "no timestamp" },
	])],
	["read"],
	["take", Date.UTC(2026, 3, 5, 12)],
	["take", Date.UTC(2026, 3, 5, 12)],
	["record", { kind: "fatal_error", error: { name: "Error", message: "after raw", stack: "Error: after raw" }, cwd: "/w" }],
	// Exactly seven days old is still recent; one millisecond more is not.
	["take", "sevenDays+1"],
	["take", "sevenDays"],
	["raw", JSON.stringify([{ timestamp: "2026-04-05T10:00:00Z", message: "truthy", notified: "yes" }, { timestamp: "2026-04-05T10:00:00Z", message: "falsy", notified: "" }])],
	["take", Date.UTC(2026, 3, 5, 12)],
];
const crashSteps = [];
for (const op of crashOps) {
	let result;
	const takeAt = op[0] !== "take" ? undefined : op[1] === "sevenDays" ? clock + 604_800_000 : op[1] === "sevenDays+1" ? clock + 604_800_001 : op[1];
	if (op[0] === "take") result = crash.takeUnnotifiedCrash(crashPath, takeAt);
	else if (op[0] === "record") {
		clock += 60_000;
		result = crash.recordCrash({ kind: op[1].kind, error: makeError(op[1].error), sessionFile: op[1].sessionFile, cwd: op[1].cwd }, crashPath);
	} else if (op[0] === "read") result = crash.readCrashLog(crashPath);
	else if (op[0] === "clear") result = crash.clearCrashLog(crashPath);
	else if (op[0] === "raw") result = writeFileSync(crashPath, op[1]);
	crashSteps.push({ op, now: clock, ...(takeAt === undefined ? {} : { takeAt }), result: clone(result), file: read() });
}

// ---- extension stack attribution ----
const pkg = (source, baseDir, entry = "extensions/index.ts") => {
	const resolvedPath = `${baseDir.replace(/\\/g, "/")}/${entry}`;
	return { path: resolvedPath, resolvedPath, sourceInfo: { path: resolvedPath, source, scope: "user", origin: "package", baseDir } };
};
const local = (path, extra = {}) => ({ path, resolvedPath: path, sourceInfo: { path, source: "local", scope: "user", origin: "top-level", baseDir: dirname(path) }, ...extra });
const single = (name) => ({ path: `/plugins/${name}.ts`, resolvedPath: `/plugins/${name}.ts`, sourceInfo: { path: `/plugins/${name}.ts`, source: `/plugins/${name}.ts`, scope: "user", origin: "package", baseDir: "/plugins" } });
const stackCases = [
	{
		stack: "TypeError: Cannot read properties of undefined (reading 'runtime')\n    at streamSimple (file:///home/fedora/.local/lib/node_modules/@earendil-works/pi-coding-agent/dist/bundle/chunks/chunk-CMRUVXTE.js:1093:16944)\n    at /home/fedora/.pi/agent/npm/node_modules/pi-observational-memory/src/agents/worker-stream.ts:43:45",
		extensions: [pkg("npm:pi-observational-memory", "/home/fedora/.pi/agent/npm/node_modules/pi-observational-memory"), pkg("npm:unrelated", "/home/fedora/.pi/agent/npm/node_modules/unrelated")],
	},
	{
		stack: "Error: broken\n    at run (c:\\users\\reporter\\.pi\\agent\\npm\\node_modules\\@scope\\memory\\src\\worker.ts:4:2)",
		extensions: [
			pkg("npm:@scope/memory", "C:\\Users\\reporter\\.pi\\agent\\npm\\node_modules\\@scope\\memory", "extensions/first.ts"),
			pkg("npm:@scope/memory", "C:\\Users\\reporter\\.pi\\agent\\npm\\node_modules\\@scope\\memory", "extensions/second.ts"),
		],
	},
	{ stack: "Error: broken\n    at run (file:///plugins/b.ts:4:2)", extensions: [single("a"), single("b")] },
	{ stack: "Error: Failed to read /tmp/node_modules/memory/extensions/index.ts\n    at run (file:///opt/pi/dist/core.js:4:2)", extensions: [pkg("npm:memory", "/tmp/node_modules/memory")] },
	{
		stack: "Error: progress 100%\n    at run (file:///Users/reporter/.pi/agent/extensions/local%20memory/worker.ts:4:2)",
		extensions: [local("/Users/reporter/.pi/agent/extensions/local memory/index.ts")],
	},
	{
		stack: "Error\n    at a (/x/one.ts:1:1)\n    at b (/x/one.tsx:2:2)\n    at c /x/two.ts\n  at d (/x/three.ts)\n\tat e (/x/four.ts\tmore)\n    at f (/x/bad%E0%A4%A.ts:1:1)\n    at g (/x/five.ts%3A1)\n    not a frame /x/six.ts",
		extensions: [local("/x/one.ts"), local("/x/two.ts"), local("/x/three.ts"), local("/x/four.ts"), local("/x/five.ts"), local("/x/six.ts"), local("/x/bad%E0%A4%A.ts"), local("/x/one.tsx", { path: "label-tsx" }), local("<inline>"), local("")],
	},
	{ stack: "Error\n    at (/pkg/root/lib/x.js:1:1)", extensions: [pkg("", "/pkg/root"), { ...pkg("npm:nobase", "/nb"), sourceInfo: { ...pkg("npm:nobase", "/nb").sourceInfo, baseDir: "" } }] },
	{ stack: "Error\n    at (/pkg/root/lib/x.js:1:1)\n    at (/nb/extensions/index.ts:2:2)", extensions: [{ ...pkg("npm:nobase", "/nb"), sourceInfo: { ...pkg("npm:nobase", "/nb").sourceInfo, baseDir: undefined } }] },
	{ stack: "Error\n    at (/dir/sub/file.ts:1:1)\n    at (/w/single.mjs:1:1)", extensions: [
		{ path: "/dir/index.js", resolvedPath: "/dir/index.js", sourceInfo: { path: "/dir/index.js", source: "./dir", scope: "project", origin: "top-level" } },
		{ path: "/w/single.mjs", resolvedPath: "/w/single.mjs", sourceInfo: { path: "/w/single.mjs", source: "npm:single.mjs", scope: "user", origin: "package", baseDir: "/w" } },
		{ path: "/w/x/y.js", resolvedPath: "/w/x/y.js", sourceInfo: { path: "/w/x/y.js", source: "https://ex.com/y.js", scope: "user", origin: "package", baseDir: "/w" } },
	] },
	{ stack: "Error\n    at (D:/Proj/Ext.ts:1:1)", extensions: [local("d:/proj/ext.ts"), local("D:/Proj/Ext.ts/")] },
	{ stack: "", extensions: [local("/x.ts")] },
	{ stack: "    at message (/msg/a.ts:1:1)\n    at frame (/frame/b.ts:2:2)", extensions: [local("/msg/a.ts"), local("/frame/b.ts")] },
	{ stack: "only one line /x.ts", extensions: [local("/x.ts")] },
	{ stack: "Error\n    at x (/a%2Fb/c.ts:1:1)\n    at y (/q%23r/i.ts:1)", extensions: [local("/a%2Fb/c.ts"), local("/q%23r/i.ts")] },
];
for (const c of stackCases) c.matches = crash.findExtensionStackMatches(c.stack, c.extensions);

// ---- provider attribution ----
const attributionCases = [];
const providerModels = [
	{ provider: "openrouter", baseUrl: "https://example.com" },
	{ provider: "custom", baseUrl: "https://openrouter.ai/api/v1" },
	{ provider: "custom", baseUrl: "https://notopenrouter.ai.example/x" },
	{ provider: "nvidia", baseUrl: "" },
	{ provider: "custom", baseUrl: "https://integrate.api.nvidia.com/v1" },
	{ provider: "custom", baseUrl: "https://INTEGRATE.api.nvidia.com:8443/v1" },
	{ provider: "cloudflare-workers-ai", baseUrl: "x" },
	{ provider: "cloudflare-ai-gateway", baseUrl: "x" },
	{ provider: "custom", baseUrl: "https://api.cloudflare.com/client" },
	{ provider: "custom", baseUrl: "https://gateway.ai.cloudflare.com/v1" },
	{ provider: "opencode", baseUrl: "x" },
	{ provider: "opencode-go", baseUrl: "x" },
	{ provider: "custom", baseUrl: "https://opencode.ai/zen" },
	{ provider: "custom", baseUrl: "not a url" },
];
for (const m of providerModels) {
	for (const enabled of [true, false]) {
		for (const sessionId of [undefined, "", "sess-1"]) {
			const sources = [[], [undefined, { "User-Agent": "custom", "x-opencode-client": null, "X-Extra": "1" }]];
			for (const headerSources of sources) {
				const settingsManager = { getEnableInstallTelemetry: () => enabled };
				setEnv({});
				const output = attribution.mergeProviderAttributionHeaders({ ...m, id: "m" }, settingsManager, sessionId, ...headerSources);
				attributionCases.push({ model: m, enabled, sessionId: sessionId ?? null, headerSources: headerSources.map((h) => h ?? null), output: output ?? null });
			}
		}
	}
}
// A set PI_TELEMETRY overrides the setting.
setEnv({ PI_TELEMETRY: "0" });
const envOverride = attribution.mergeProviderAttributionHeaders({ provider: "openrouter", baseUrl: "x" }, { getEnableInstallTelemetry: () => true }, undefined) ?? null;

rmSync(scratch, { recursive: true, force: true });
const fixture = {
	host,
	redactUrlCases,
	redactJsonCases,
	extensions,
	globalSettings,
	projectSettings,
	metadataCases,
	diagnosticsCases,
	fileCases,
	archive: { bundle: bundles[0], base64: archive, name: archiveName },
	crashSteps,
	stackCases,
	attributionCases,
	envOverride,
};
writeFileSync(join(here, "reports.json"), `${JSON.stringify(fixture, null, "\t")}\n`);
