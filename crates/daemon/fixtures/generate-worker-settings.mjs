// Records crates/daemon/fixtures/worker-settings.json from the TypeScript Session worker's settings side
// (experimental/services/worker-settings.ts `readWorkerSettings`, `applyWorkerSetting`, `modelEnvOverrides`;
// session-control-provider.ts `debugEnvironment`) over a recording harness and lane, in private temp profile
// directories (`HOME`, `ULTRON_CODING_AGENT_DIR` and `ULTRON_SERVER_DIR` point into them). Paths are recorded with the
// temp root as `<root>`. crates/daemon/tests/worker_settings.rs replays it.
// Usage: node --import packages/coding-agent/src/experimental/source-resolver.ts \
//   crates/daemon/fixtures/generate-worker-settings.mjs
import "../../cli/fixtures/offline-guard.mjs";
import { mkdirSync, mkdtempSync, readFileSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, "../../..");
const tmp = realpathSync(mkdtempSync(join(tmpdir(), "ultron-worker-settings-")));
for (const name of Object.keys(process.env)) {
	if (/(_API_KEY|_TOKEN|_SECRET)$/u.test(name) || name.startsWith("ULTRON_") || name.startsWith("PI_")) {
		delete process.env[name];
	}
}
Object.assign(process.env, {
	HOME: join(tmp, "home"),
	ULTRON_CODING_AGENT_DIR: join(tmp, "agent"),
	ULTRON_SERVER_DIR: join(tmp, "server"),
	TZ: "UTC",
});
const load = (path) => import(pathToFileURL(join(root, path)).href);
const { readWorkerSettings, applyWorkerSetting, modelEnvOverrides } = await load(
	"packages/coding-agent/src/experimental/services/worker-settings.ts",
);
const { debugEnvironment } = await load(
	"packages/coding-agent/src/experimental/services/session-control-provider.ts",
);
const { SettingsManager } = await load("packages/coding-agent/src/core/settings-manager.ts");
const { ProjectTrustStore } = await load("packages/coding-agent/src/core/trust-manager.ts");

const norm = (value) => JSON.parse(JSON.stringify(value).replaceAll(tmp, "<root>"));
let counter = 0;
/** A fresh profile and project directory with the given files. */
function dirs({ global, project, trust }) {
	counter += 1;
	const base = join(tmp, `case-${counter}`);
	const agentDir = join(base, "agent");
	const cwd = join(base, "project");
	mkdirSync(agentDir, { recursive: true });
	mkdirSync(join(cwd, ".pi"), { recursive: true });
	if (global !== undefined) writeFileSync(join(agentDir, "settings.json"), JSON.stringify(global, null, 2));
	if (project !== undefined) writeFileSync(join(cwd, ".pi", "settings.json"), JSON.stringify(project, null, 2));
	if (trust !== undefined) {
		new ProjectTrustStore(agentDir).setMany(
			trust.map(({ path, decision }) => ({ path: path === "." ? cwd : path === ".." ? base : path, decision })),
		);
	}
	return { agentDir, cwd };
}

/** The live harness and lane the worker settings act on; every change is recorded as `<what> <value>`. */
function live(initial = {}) {
	const calls = [];
	let compaction = { enabled: initial.compaction ?? true, reserveTokens: 16384, keepRecentTokens: 20000 };
	let retry = { enabled: initial.retry ?? true, maxRetries: 3 };
	let steering = initial.steeringMode ?? "one-at-a-time";
	let followUp = initial.followUpMode ?? "one-at-a-time";
	let stream = {};
	const harness = {
		getCompactionSettings: async () => ({ ...compaction }),
		setCompactionSettings: async (next) => {
			compaction = next;
			calls.push(`compaction ${next.enabled}`);
		},
		getRetryPolicy: async () => ({ ...retry }),
		setRetryPolicy: async (next) => {
			retry = next;
			calls.push(`retry ${next.enabled}`);
		},
		getSteeringMode: async () => steering,
		setSteeringMode: async (mode) => {
			steering = mode;
			calls.push(`steeringMode ${mode}`);
		},
		getFollowUpMode: async () => followUp,
		setFollowUpMode: async (mode) => {
			followUp = mode;
			calls.push(`followUpMode ${mode}`);
		},
		getStreamOptions: async () => ({ ...stream }),
		setStreamOptions: async (next) => {
			for (const key of Object.keys(next)) {
				if (next[key] !== stream[key]) calls.push(`${key === "timeoutMs" ? "streamTimeout" : key} ${next[key]}`);
			}
			stream = next;
		},
	};
	const lane = {
		getModel: async () => initial.model,
		setThinkingLevel: async (level) => calls.push(`thinking ${level}`),
	};
	return { harness, lane, calls, configureHttpIdleTimeout: (ms) => calls.push(`httpIdleTimeout ${ms}`) };
}

const target = (d, l, settingsManager, env = {}) => ({
	harness: l.harness,
	lane: l.lane,
	settingsManager,
	cwd: d.cwd,
	agentDir: d.agentDir,
	configureHttpIdleTimeout: l.configureHttpIdleTimeout,
	env,
});

// ── readWorkerSettings ─────────────────────────────────────────────────────
const READS = [
	{ name: "empty", files: {} },
	{
		name: "everything-set",
		files: {
			global: {
				transport: "websocket",
				httpIdleTimeoutMs: "disabled",
				cacheWarming: "idle",
				modelThinkingLevels: { "anthropic/claude-sonnet-4-5": "high" },
				defaultThinkingLevel: "low",
				defaultProvider: "openai",
				defaultModel: "gpt-5",
				theme: "light",
				hideThinkingBlock: true,
				terminal: { showImages: false, imageWidthCells: 12.7, clearOnShrink: true, showTerminalProgress: true },
				images: { autoResize: false, blockImages: true },
				enableSkillCommands: false,
				markdown: { mermaid: "final" },
				showCacheMissNotices: true,
				collapseChangelog: true,
				enableInstallTelemetry: false,
				quietStartup: true,
				rlmPaneAutoOpen: false,
				autoreview: { enabled: false, ack: false, ackArt: "custom-art" },
				defaultProjectTrust: "never",
				doubleEscapeAction: "fork",
				treeFilterMode: "user-only",
				showHardwareCursor: true,
				editorPaddingX: 2,
				outputPad: 0,
				autocompleteMaxVisible: 9,
				tuiMode: "fullscreen",
				fullscreenExitOutput: "resume-hint",
				fullscreenScrollbar: "always",
				fullscreenCopyOnSelect: false,
				warnings: { anthropicExtraUsage: false },
				enabledModels: ["anthropic/*", "gpt-5"],
				rlm: { frameModel: " openai/gpt-5 ", childModel: "claude-code/opus", frameThinking: "xhigh" },
				review: { model: "anthropic/claude-opus-4-1" },
				claudeCode: { frameModel: "claude-code/opus", childModel: " sonnet ", ultronChildModel: "openai/gpt-5", model: "" },
			},
		},
		live: { compaction: false, retry: false, steeringMode: "all", followUpMode: "all" },
	},
	{
		name: "invalid-values-fall-back",
		files: {
			global: {
				cacheWarming: "sometimes",
				theme: 7,
				terminal: { imageWidthCells: "wide" },
				markdown: { mermaid: "always" },
				autoreview: { ackArt: false },
				defaultProjectTrust: "maybe",
				treeFilterMode: "weird",
				outputPad: 3,
				tuiMode: "big",
				fullscreenExitOutput: "x",
				fullscreenScrollbar: "x",
				rlm: { frameModel: "no-slash", frameThinking: "huge" },
				review: "openai/gpt-5",
				claudeCode: "opus",
			},
		},
	},
	{
		name: "ack-art-logo-and-negative-width",
		files: { global: { autoreview: { ackArt: "logo" }, terminal: { imageWidthCells: -4 }, httpIdleTimeoutMs: 1234.9 } },
	},
	{
		name: "project-overrides-when-trusted",
		files: {
			global: { theme: "dark", defaultProjectTrust: "always", hideThinkingBlock: false },
			project: { theme: "light", hideThinkingBlock: true, defaultProjectTrust: "never", cacheWarming: "off" },
			trust: [{ path: ".", decision: true }],
		},
		projectTrusted: true,
	},
	{
		name: "untrusted-project-ignored-inherited-decision",
		files: { global: {}, project: { theme: "light" }, trust: [{ path: "..", decision: false }] },
		projectTrusted: false,
	},
	{
		name: "env-overrides",
		files: {},
		env: {
			ULTRON_RLM_FRAME_MODEL: " openai/gpt-5 ",
			ULTRON_REVIEW_MODEL: "not-a-model",
			ULTRON_CLAUDE_FRAME_MODEL: "opus",
			ULTRON_CLAUDE_CHILD_MODEL: " sonnet ",
			ULTRON_CLAUDE_MODEL: "   ",
		},
	},
];

const reads = [];
for (const c of READS) {
	const d = dirs(c.files);
	const l = live(c.live);
	const settings = SettingsManager.create(d.cwd, d.agentDir, { projectTrusted: c.projectTrusted ?? false });
	const read = await readWorkerSettings(target(d, l, settings, c.env ?? {}), {});
	reads.push(
		norm({
			name: c.name,
			files: c.files,
			projectTrusted: c.projectTrusted ?? false,
			live: {
				compaction: c.live?.compaction ?? true,
				retry: c.live?.retry ?? true,
				steeringMode: c.live?.steeringMode ?? "one-at-a-time",
				followUpMode: c.live?.followUpMode ?? "one-at-a-time",
			},
			env: c.env ?? {},
			read,
		}),
	);
}

// ── applyWorkerSetting ─────────────────────────────────────────────────────
const MODEL = { provider: "anthropic", id: "claude-sonnet-4-5" };
const APPLIES = [
	{
		name: "live-session-settings",
		steps: [
			["compaction.enabled", false],
			["retry.enabled", false],
			["steeringMode", "all"],
			["followUpMode", "all"],
			["steeringMode", "sometimes"],
			["compaction.enabled", "yes"],
		],
	},
	{
		name: "transport-and-idle-timeout",
		steps: [
			["transport", "websocket"],
			["transport", "carrier-pigeon"],
			["httpIdleTimeoutMs", 0],
			["httpIdleTimeoutMs", 120000.9],
			["httpIdleTimeoutMs", -1],
			["cacheWarming", "off"],
			["cacheWarming", "always"],
		],
	},
	{
		name: "idle-timeout-pinned-by-provider-retry",
		files: { global: { retry: { provider: { timeoutMs: 5000 } } } },
		steps: [["httpIdleTimeoutMs", 60000]],
	},
	{
		name: "model-thinking-levels",
		steps: [
			["modelThinkingLevel", { provider: "anthropic", modelId: "claude-sonnet-4-5", level: "high" }],
			["modelThinkingLevel", { provider: "openai", modelId: "gpt-5", level: "low" }],
			["modelThinkingLevel", { provider: "anthropic", modelId: "claude-sonnet-4-5", level: null }],
			["defaultThinkingLevel", "minimal"],
			["modelThinkingLevel", { provider: "anthropic", modelId: "claude-sonnet-4-5", level: null }],
			["modelThinkingLevel", { provider: "openai", modelId: "gpt-5", level: "extreme" }],
			["modelThinkingLevel", "high"],
			["modelThinkingLevel", { provider: "", modelId: "x", level: "low" }],
			["defaultThinkingLevel", "lots"],
		],
	},
	{
		name: "presentation-settings",
		steps: [
			["theme", "light"],
			["theme", ""],
			["hideThinkingBlock", true],
			["terminal.showImages", false],
			["terminal.imageWidthCells", 0],
			["terminal.imageWidthCells", 33.6],
			["images.autoResize", false],
			["images.blockImages", true],
			["enableSkillCommands", false],
			["markdown.mermaid", "off"],
			["markdown.mermaid", "sometimes"],
			["showCacheMissNotices", true],
			["collapseChangelog", true],
			["enableInstallTelemetry", false],
			["quietStartup", true],
			["rlmPaneAutoOpen", false],
			["doubleEscapeAction", "none"],
			["treeFilterMode", "labeled-only"],
			["showHardwareCursor", true],
			["editorPaddingX", 7.5],
			["editorPaddingX", -1],
			["outputPad", 5],
			["outputPad", 0],
			["autocompleteMaxVisible", 1],
			["autocompleteMaxVisible", 0],
			["autocompleteMaxVisible", 50],
			["terminal.clearOnShrink", true],
			["terminal.showTerminalProgress", true],
			["tuiMode", "fullscreen"],
			["fullscreenExitOutput", "resume-hint"],
			["fullscreenScrollbar", "hidden"],
			["fullscreenCopyOnSelect", false],
			["warnings", { anthropicExtraUsage: false }],
			["warnings", {}],
			["warnings", { anthropicExtraUsage: "no" }],
			["warnings", []],
			["unknownKey", 1],
		],
	},
	{
		name: "autoreview-and-trust",
		files: { global: { autoreview: { accounts: ["octo"], pollSeconds: 30 } } },
		steps: [
			["engineering", true],
			["autoreview.enabled", false],
			["autoreview.ack", false],
			["autoreview.ackArt", "none"],
			["autoreview.ackArt", "logo"],
			["autoreview.ackArt", "custom"],
			["autoreview", { enabled: true, accounts: ["a", "b"] }],
			["autoreview", { enabled: null, accounts: null }],
			["autoreview", { accounts: [1] }],
			["autoreview", { pollSeconds: 5 }],
			["autoreview", "on"],
			["defaultProjectTrust", "always"],
			["defaultProjectTrust", "sometimes"],
		],
	},
	{
		name: "model-settings",
		steps: [
			["rlm.frameModel", " openai/gpt-5 "],
			["rlm.frameModel", "claude-code/opus"],
			["rlm.childModel", "anthropic/claude-haiku-4-5"],
			["rlm.childModel", "claude-code/sonnet"],
			["rlm.frameModel", "gpt-5"],
			["rlm.frameModel", null],
			["rlm.frameThinking", "max"],
			["rlm.frameThinking", "loud"],
			["rlm.frameThinking", null],
			["review.model", "openai/o3"],
			["review.model", 5],
			["claudeCode.frameModel", "claude-code/opus"],
			["claudeCode.ultronChildModel", "openai/gpt-5"],
			["claudeCode.ultronChildModel", "claude-code/opus"],
			["claudeCode.childModel", "claude-code/sonnet"],
			["claudeCode.model", " opus[1m] "],
			["claudeCode.model", "-bad alias"],
			["claudeCode.model", null],
			["enabledModels", ["anthropic/*", "gpt-5"]],
			["enabledModels", ["ok", ""]],
			["enabledModels", "anthropic/*"],
			["enabledModels", null],
		],
	},
	{
		name: "malformed-file-is-not-overwritten",
		files: { raw: "{ not json" },
		steps: [["theme", "light"]],
	},
];

const applies = [];
for (const c of APPLIES) {
	const files = c.files ?? {};
	const d = dirs(files.raw === undefined ? files : {});
	if (files.raw !== undefined) writeFileSync(join(d.agentDir, "settings.json"), files.raw);
	const l = live({ model: MODEL });
	const settings = SettingsManager.create(d.cwd, d.agentDir, { projectTrusted: false });
	const steps = [];
	for (const [key, value] of c.steps) {
		l.calls.length = 0;
		let outcome;
		try {
			outcome = { result: await applyWorkerSetting(target(d, l, settings), key, value, {}) };
		} catch (error) {
			outcome = { error: error instanceof Error ? error.message : String(error) };
		}
		let file = null;
		try {
			file = JSON.parse(readFileSync(join(d.agentDir, "settings.json"), "utf8"));
		} catch {
			file = readFileSync(join(d.agentDir, "settings.json"), "utf8");
		}
		steps.push(norm({ key, value, ...outcome, live: [...l.calls], file }));
	}
	applies.push({ name: c.name, files, model: MODEL, steps });
}

// ── modelEnvOverrides and debugEnvironment ─────────────────────────────────
const envOverrides = [
	{},
	{ ULTRON_RLM_FRAME_MODEL: "openai/gpt-5", ULTRON_REVIEW_MODEL: " a/b/c ", ULTRON_CLAUDE_FRAME_MODEL: "x/y" },
	{ ULTRON_RLM_FRAME_MODEL: "a /b", ULTRON_CLAUDE_MODEL: "opus", ULTRON_CLAUDE_CHILD_MODEL: "" },
].map((env) => ({ env, overrides: modelEnvOverrides(env) }));
const debugEnvironments = [
	{
		PATH: "/usr/bin",
		ULTRON_CODING_AGENT_DIR: "/a",
		PI_OFFLINE: "1",
		ULTRON_API_KEY: "k",
		PI_SESSION_WORKER_SESSION_KEY_BASE64: "x",
		ULTRON_GH_Token: "t",
		ULTRON_PASSWD_FILE: "p",
		pi_lower: "l",
		ULTRON_COOKIE_JAR: "c",
		ULTRON_AUTHOR: "me",
		ULTRON_CREDENTIALS: "c",
		ULTRON_Z: "z",
		ULTRON_a: "lower",
		ULTRON_B: "B",
		PI_SECRETARY: "s",
		ULTRON_PASSWORD_X: "pw",
		ULTRONX: "no",
	},
].map((env) => ({ env, environment: debugEnvironment(env) }));

writeFileSync(
	join(here, "worker-settings.json"),
	`${JSON.stringify({ reads, applies, envOverrides, debugEnvironments }, null, "\t")}\n`,
);
rmSync(tmp, { recursive: true, force: true });
