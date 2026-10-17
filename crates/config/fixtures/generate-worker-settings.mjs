// Generates crates/config/fixtures/worker-settings.json: what a session worker reads from settings that the Rust
// build applies since wave 35. `getters`: settings-manager.ts getRlmModelSettings, getReviewModel,
// getWorktreeSettings, getProviderRetrySettings, getHttpIdleTimeoutMs, getTransport, getShowHardwareCursor and
// getClearOnShrink (with and without PI_HARDWARE_CURSOR / PI_CLEAR_ON_SHRINK), and session-worker.ts
// providerRequestOptions, over global settings. `trust`: worker-settings.ts workerProjectTrusted and the worker's
// SettingsManager.create(cwd, agentDir, {projectTrusted}) over a profile and a project with `.pi/settings.json`,
// for each stored decision / defaultProjectTrust. crates/config/tests/worker_settings.rs and
// crates/session-core/tests/worker_trust.rs replay it. HOME points into a temp dir; no path is recorded.
// Usage: node crates/config/fixtures/generate-worker-settings.mjs
import "../../cli/fixtures/offline-guard.mjs";
import { mkdirSync, mkdtempSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const repo = resolve(here, "../../..");
const root = realpathSync(mkdtempSync(join(tmpdir(), "worker-settings-")));
const home = join(root, "fakehome");
mkdirSync(home, { recursive: true });
process.env.HOME = home;
process.env.ULTRON_CODING_AGENT_DIR = join(root, "agent");
for (const key of Object.keys(process.env)) {
	if (key.startsWith("PI_") || key.startsWith("ULTRON_")) {
		if (key !== "ULTRON_CODING_AGENT_DIR" && key !== "ULTRON_GUARD_LOG") delete process.env[key];
	}
}
const src = (path) => pathToFileURL(join(repo, "packages/coding-agent/src", path)).href;
const { SettingsManager } = await import(src("core/settings-manager.ts"));
const { workerProjectTrusted } = await import(src("experimental/services/worker-settings.ts"));
const { providerRequestOptions } = await import(src("experimental/session-worker.ts"));

const clean = (v) => JSON.parse(JSON.stringify(v === undefined ? null : v));
const attempt = (f) => {
	try {
		return { value: clean(f()) };
	} catch (error) {
		return { error: error instanceof Error ? error.message : String(error) };
	}
};
const withEnv = (vars, f) => {
	const saved = {};
	for (const [k, v] of Object.entries(vars)) {
		saved[k] = process.env[k];
		if (v === null) delete process.env[k];
		else process.env[k] = v;
	}
	try {
		return f();
	} finally {
		for (const [k, v] of Object.entries(saved)) {
			if (v === undefined) delete process.env[k];
			else process.env[k] = v;
		}
	}
};

const SETTINGS = [
	{},
	{ rlm: { frameModel: "openai/gpt-5", childModel: "anthropic/claude-sonnet-4-5", frameThinking: "high" } },
	{ rlm: { childModel: "claude-code/sonnet", frameThinking: "max" } },
	{ rlm: { frameModel: " openai/gpt-5 ", childModel: "nope", frameThinking: "bogus" } },
	{ rlm: { frameModel: "a/b c", childModel: "x/y/z" } },
	{ rlm: "openai/gpt-5" },
	{ review: { model: "openai/gpt-5-codex" } },
	{ review: { model: "bad" } },
	{ review: "openai/gpt-5" },
	{ reviewModel: "openai/gpt-5" },
	{ worktrees: { link: ["node_modules", " ", 3, " .venv"], copy: [], setupCommand: "  npm ci  " } },
	{ worktrees: { setupCommand: "   ", copy: [".env.local"] } },
	{ worktrees: { link: "node_modules", setupCommand: 7 } },
	{ worktrees: "x" },
	{ retry: { provider: { timeoutMs: 1000, maxRetries: 2, maxRetryDelayMs: 5000 } } },
	{ retry: { provider: { maxRetries: 0 } } },
	{ retry: { provider: { timeoutMs: null, maxRetries: null, maxRetryDelayMs: null } } },
	{ retry: { maxDelayMs: 7000 } },
	{ retry: { provider: "x" } },
	{ httpIdleTimeoutMs: 0 },
	{ httpIdleTimeoutMs: "disabled" },
	{ httpIdleTimeoutMs: " 45000 " },
	{ httpIdleTimeoutMs: 12.7 },
	{ httpIdleTimeoutMs: "soon" },
	{ httpIdleTimeoutMs: -1 },
	{ httpIdleTimeoutMs: 9000, retry: { provider: { timeoutMs: 1 } } },
	{ httpIdleTimeoutMs: 0, retry: { provider: { maxRetries: 4 } } },
	{ transport: "sse" },
	{ transport: "websocket-cached" },
	{ websockets: true },
	{ websockets: false },
	{ showHardwareCursor: false, terminal: { clearOnShrink: false } },
	{ showHardwareCursor: true, terminal: { clearOnShrink: true } },
];

const ENVS = [{}, { PI_HARDWARE_CURSOR: "1", PI_CLEAR_ON_SHRINK: "1" }, { PI_HARDWARE_CURSOR: "true", PI_CLEAR_ON_SHRINK: "yes" }];

const getters = SETTINGS.map((settings) => {
	const m = SettingsManager.inMemory(settings);
	return {
		settings,
		rlmModelSettings: attempt(() => m.getRlmModelSettings()),
		reviewModel: attempt(() => m.getReviewModel()),
		worktreeSettings: attempt(() => m.getWorktreeSettings()),
		providerRetrySettings: attempt(() => m.getProviderRetrySettings()),
		httpIdleTimeoutMs: attempt(() => m.getHttpIdleTimeoutMs()),
		providerRequestOptions: attempt(() => providerRequestOptions(m)),
		transport: attempt(() => m.getTransport()),
		terminal: ENVS.map((env) =>
			withEnv(
				{ PI_HARDWARE_CURSOR: null, PI_CLEAR_ON_SHRINK: null, ...env },
				() => ({
					env,
					showHardwareCursor: attempt(() => m.getShowHardwareCursor()),
					clearOnShrink: attempt(() => m.getClearOnShrink()),
				}),
			),
		),
	};
});

// The worker's trust over a profile and a project that both configure the session.
const GLOBAL = {
	defaultModel: "global-model",
	rlm: { childModel: "global/child", frameModel: "global/frame" },
	review: { model: "global/review" },
	compaction: { enabled: true, reserveTokens: 1000 },
};
const PROJECT = {
	defaultProvider: "project-provider",
	defaultModel: "project-model",
	defaultThinkingLevel: "high",
	compaction: { enabled: false, keepRecentTokens: 777 },
	retry: { maxRetries: 7, baseDelayMs: 10, provider: { timeoutMs: 1234, maxRetries: 1 } },
	httpIdleTimeoutMs: 4000,
	thinkingBudgets: { low: 5 },
	transport: "sse",
	rlm: { childModel: "project/child" },
	review: { model: "project/review" },
	worktrees: { setupCommand: "make setup", link: ["deps"] },
};
const SCENARIOS = [
	{ name: "undecided-ask", trust: null, defaultProjectTrust: null, projectFiles: true },
	{ name: "distrusted", trust: false, defaultProjectTrust: null, projectFiles: true },
	{ name: "default-never", trust: null, defaultProjectTrust: "never", projectFiles: true },
	{ name: "trusted-over-never", trust: true, defaultProjectTrust: "never", projectFiles: true },
	{ name: "parent-distrusted", trust: "parent-false", defaultProjectTrust: null, projectFiles: true },
	{ name: "nothing-to-trust", trust: false, defaultProjectTrust: "never", projectFiles: false },
];
const trust = [];
for (const scenario of SCENARIOS) {
	const dir = realpathSync(mkdtempSync(join(root, "t-")));
	const agent = join(dir, "agent");
	const cwd = join(dir, "work", "proj");
	mkdirSync(agent, { recursive: true });
	mkdirSync(cwd, { recursive: true });
	const global = scenario.defaultProjectTrust ? { ...GLOBAL, defaultProjectTrust: scenario.defaultProjectTrust } : GLOBAL;
	writeFileSync(join(agent, "settings.json"), JSON.stringify(global));
	if (scenario.projectFiles) {
		mkdirSync(join(cwd, ".pi"), { recursive: true });
		writeFileSync(join(cwd, ".pi", "settings.json"), JSON.stringify(PROJECT));
	}
	if (scenario.trust === "parent-false") writeFileSync(join(agent, "trust.json"), JSON.stringify({ [join(dir, "work")]: false }));
	else if (scenario.trust !== null) writeFileSync(join(agent, "trust.json"), JSON.stringify({ [cwd]: scenario.trust }));
	const trusted = workerProjectTrusted(cwd, agent);
	const m = SettingsManager.create(cwd, agent, { projectTrusted: trusted });
	trust.push({
		...scenario,
		trusted,
		compaction: attempt(() => m.getCompactionSettings()),
		retry: attempt(() => m.getRetrySettings()),
		defaultProvider: attempt(() => m.getDefaultProvider()),
		defaultModel: attempt(() => m.getDefaultModel()),
		defaultThinkingLevel: attempt(() => m.getDefaultThinkingLevel()),
		httpIdleTimeoutMs: attempt(() => m.getHttpIdleTimeoutMs()),
		thinkingBudgets: attempt(() => m.getThinkingBudgets()),
		transport: attempt(() => m.getTransport()),
		providerRequestOptions: attempt(() => providerRequestOptions(m)),
		rlmModelSettings: attempt(() => m.getRlmModelSettings()),
		reviewModel: attempt(() => m.getReviewModel()),
		worktreeSettings: attempt(() => m.getWorktreeSettings()),
	});
}

rmSync(root, { recursive: true, force: true });
writeFileSync(
	join(here, "worker-settings.json"),
	`${JSON.stringify({ global: GLOBAL, project: PROJECT, getters, trust }, null, 1)}\n`,
);
console.log(`wrote ${getters.length} getter cases, ${trust.length} trust scenarios`);
process.exit(0);
