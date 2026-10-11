// Generates crates/config/fixtures/config.json from the TypeScript product in this checkout.
// Drives SettingsManager, AuthStorage, ProjectTrustStore and the agent-dir helpers through scripted operations in
// temp directories (HOME and ULTRON_CODING_AGENT_DIR point into them, so the real ~/.ultron is never read) and
// records every API result and the resulting file bytes. Temp paths are written as <root>.
// Usage: node crates/config/fixtures/generate.mjs
import { execFileSync, spawn } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync, realpathSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const repo = resolve(here, "../../..");
const root = realpathSync(mkdtempSync(join(tmpdir(), "config-fixture-")));
process.env.HOME = join(root, "home");
process.env.ULTRON_CODING_AGENT_DIR = join(root, "agent");
mkdirSync(process.env.HOME, { recursive: true });
const src = (path) => import(pathToFileURL(join(repo, "packages/coding-agent/src", path)).href);
const { SettingsManager } = await src("core/settings-manager.ts");
const { AuthStorage, ReadOnlyAuthStorage, readStoredCredential } = await src("core/auth-storage.ts");
const { FileModelsStore } = await src("core/models-store.ts");
const { resolveConfigValue } = await src("core/resolve-config-value.ts");
const trust = await src("core/trust-manager.ts");
const config = await src("config.ts");

const rel = (v) => JSON.parse(JSON.stringify(v === undefined ? null : v).split(root).join("<root>"));
const read = (path) => (existsSync(path) ? readFileSync(path, "utf-8").split(root).join("<root>") : null);
const errorText = (e) => (e instanceof Error ? e.message : String(e)).split(root).join("<root>");

// ---------------------------------------------------------------- settings
const SETTINGS_SCENARIOS = [
	{ name: "autoreview keys from two stale workers both survive (port of the bcdf2bbea test)", steps: [
		{ op: "write", file: "global", text: JSON.stringify({ autoreview: { model: "p/m" } }) },
		{ op: "create", name: "picker" }, { op: "create", name: "stale" },
		{ op: "setAutoreviewSettings", name: "picker", patch: [["enabled", true], ["accounts", ["github.com/me"]]] },
		{ op: "setAutoreviewSettings", name: "stale", patch: [["ack", false]] },
		{ op: "setAutoreviewSettings", name: "picker", patch: [["accounts"]] },
		{ op: "view", name: "picker" }, { op: "view", name: "stale" }, { op: "reload", name: "stale" }, { op: "view", name: "stale" },
	] },
	{ name: "legacy keys migrate on load and the migrated form is written back", steps: [
		{ op: "write", file: "global", text: JSON.stringify({ queueMode: "all", websockets: true, skills: { enableSkillCommands: false, customDirectories: ["~/s"] }, retry: { maxDelayMs: 5000, maxRetries: 7 }, theme: "light" }) },
		{ op: "create", name: "a" }, { op: "view", name: "a" }, { op: "setTheme", name: "a", value: "dark" }, { op: "view", name: "a" },
	] },
	{ name: "legacy migration edge cases", steps: [
		{ op: "write", file: "global", text: JSON.stringify({ queueMode: "all", steeringMode: "one-at-a-time", websockets: false, transport: "sse", skills: { customDirectories: [] }, retry: { maxDelayMs: 9, provider: { maxRetryDelayMs: 1 } } }) },
		{ op: "create", name: "a" }, { op: "setSteeringMode", name: "a", value: "all" }, { op: "view", name: "a" },
	] },
	{ name: "unrelated external edits survive and in-memory wins for the same key", steps: [
		{ op: "write", file: "global", text: JSON.stringify({ theme: "dark", enabledModels: ["a/b"] }) },
		{ op: "create", name: "a" },
		{ op: "write", file: "global", text: JSON.stringify({ theme: "light", enabledModels: ["a/b"], packages: ["npm:x", { source: "git:y", skills: [] }], extensions: ["./ext"] }) },
		{ op: "setDefaultThinkingLevel", name: "a", value: "high" }, { op: "setTheme", name: "a", value: "solarized" },
		{ op: "setDefaultModelAndProvider", name: "a", provider: "anthropic", model: "claude" }, { op: "view", name: "a" },
		{ op: "write", file: "global", text: JSON.stringify({ theme: "external", defaultModel: "ext-model", extra: 1 }) },
		{ op: "setSteeringMode", name: "a", value: "all" }, { op: "view", name: "a" },
	] },
	{ name: "an invalid file keeps previous settings, reports its path and is never overwritten", steps: [
		{ op: "write", file: "global", text: JSON.stringify({ theme: "dark" }) },
		{ op: "create", name: "a" }, { op: "write", file: "global", text: "{ invalid json" },
		{ op: "reload", name: "a" }, { op: "view", name: "a" }, { op: "setTheme", name: "a", value: "light" }, { op: "view", name: "a" },
		{ op: "create", name: "b" }, { op: "view", name: "b" },
	] },
	{ name: "untrusted projects are skipped, refuse writes, and load once trusted", steps: [
		{ op: "write", file: "global", text: JSON.stringify({ theme: "dark", compaction: { enabled: true, reserveTokens: 1 } }) },
		{ op: "write", file: "project", text: JSON.stringify({ theme: "project", compaction: { keepRecentTokens: 2 } }) },
		{ op: "create", name: "a", trusted: false }, { op: "view", name: "a" },
		{ op: "setProjectPackages", name: "a", value: ["npm:p"] },
		{ op: "setProjectTrusted", name: "a", value: true }, { op: "view", name: "a" },
		{ op: "setProjectPackages", name: "a", value: ["npm:p"] }, { op: "view", name: "a" },
		{ op: "setProjectTrusted", name: "a", value: false }, { op: "view", name: "a" },
	] },
	{ name: "reading project settings does not create .pi; writing does", steps: [
		{ op: "create", name: "a" }, { op: "view", name: "a" }, { op: "setProjectPackages", name: "a", value: [] }, { op: "view", name: "a" },
	] },
	{ name: "nested setters merge one key into the file's current object", steps: [
		{ op: "create", name: "a" },
		{ op: "write", file: "global", text: JSON.stringify({ compaction: { reserveTokens: 100 }, retry: { maxRetries: 9 } }) },
		{ op: "setCompactionEnabled", name: "a", value: false }, { op: "setRetryEnabled", name: "a", value: false },
		{ op: "view", name: "a" },
	] },
	{ name: "model thinking levels, loki, extensions and default trust", steps: [
		{ op: "write", file: "global", text: JSON.stringify({ loki: { mode: "bogus", autoInit: true, ignoreRepos: ["a", " ", 3] }, defaultProjectTrust: "bogus" }) },
		{ op: "create", name: "a" }, { op: "view", name: "a" },
		{ op: "setModelThinkingLevel", name: "a", provider: "openai", model: "gpt", value: "high" },
		{ op: "setModelThinkingLevel", name: "a", provider: "x", model: "y", value: "low" },
		{ op: "removeModelThinkingLevel", name: "a", provider: "openai", model: "gpt" },
		{ op: "removeModelThinkingLevel", name: "a", provider: "x", model: "y" },
		{ op: "setLokiSettings", name: "a", patch: [["mode", "advise"], ["autoCommit", false]] },
		{ op: "setExtensionPaths", name: "a", value: ["./e1", "./e2"] },
		{ op: "setDefaultProjectTrust", name: "a", value: "never" }, { op: "view", name: "a" },
	] },
	{ name: "typed reads validate and default", steps: [
		{ op: "write", file: "global", text: JSON.stringify({
			steeringMode: "", followUpMode: "all", transport: null, theme: "light/dark", sessionDir: "~/sessions",
			defaultProjectTrust: "always", branchSummary: { reserveTokens: 5 }, retry: "nope",
			autoreview: { enabled: "yes", accounts: [" a ", "", 3, "b"], pollSeconds: 2.7, concurrency: -1, budget: 1e400,
				model: " p/m ", verifyModel: "bad", deepModel: "a/b c", planModel: "a/b/c", thinking: "max", verifyThinking: "huge",
				deadlineSeconds: 0, frameTimeoutSeconds: "5", mode: "hybrid", modelConcurrency: { "a/b": 2.5, c: 3, "d/e": 0 },
				testEnv: { "Org/Repo": " /x ", other: "", "org/repo": "/y" }, testImage: " img ", mise: "  ", ackArt: "  art  ",
				maxComments: 0, blockAt: "nit", planStyle: "cell", referenceView: "none", skipBots: false, ackLines: [] },
		}) },
		{ op: "write", file: "project", text: JSON.stringify({ theme: "proj", defaultProjectTrust: "never", autoreview: { enabled: true }, loki: { mode: "off" } }) },
		{ op: "create", name: "a" }, { op: "view", name: "a" },
	] },
	{ name: "numbers and integer-like keys are written as JavaScript writes them", steps: [
		{ op: "write", file: "global", text: '{"z":1.0,"big":1.5e300,"small":1e-7,"tenth":0.1,"neg":-0.0,"exp":1e21,"keys":{"10":1,"2":2,"b":3,"a":4,"01":5},"s":"\\u001b\\t\\u2028/\\"","empty":{},"list":[]}' },
		{ op: "create", name: "a" }, { op: "setTheme", name: "a", value: "dark" },
	] },
	{ name: "a BOM and an empty file load", steps: [
		{ op: "write", file: "global", text: "﻿" + JSON.stringify({ theme: "bom" }) },
		{ op: "write", file: "project", text: "" },
		{ op: "create", name: "a" }, { op: "view", name: "a" }, { op: "setProjectPackages", name: "a", value: ["x"] },
	] },
];

function settingsView(m) {
	const errors = m.drainErrors().map((e) => ({ scope: e.scope, path: e.path ?? null }));
	return rel({
		settings: m.settings, global: m.getGlobalSettings(), project: m.getProjectSettings(), trusted: m.isProjectTrusted(),
		theme: m.getTheme() ?? null, themeSetting: m.getThemeSetting() ?? null, steeringMode: m.getSteeringMode(),
		followUpMode: m.getFollowUpMode(), transport: m.getTransport(), compactionEnabled: m.getCompactionEnabled(),
		retry: m.getRetrySettings(), branchSummary: m.getBranchSummarySettings(), sessionDir: m.getSessionDir() ?? null,
		defaultProjectTrust: m.getDefaultProjectTrust(), autoreview: m.getAutoreviewSettings(), loki: m.getLokiSettings(),
		defaultProvider: m.getDefaultProvider() ?? null, defaultModel: m.getDefaultModel() ?? null, errors,
		projectDirExists: existsSync(join(dirname(dirname(m.settingsPaths?.project ?? "")), ".pi")),
	});
}

const patchObject = (patch) => Object.fromEntries(patch.map(([k, v]) => [k, v]));
async function runSettings(scenario, index) {
	const base = join(root, `settings-${index}`);
	const agentDir = join(base, "agent");
	const cwd = join(base, "project");
	mkdirSync(agentDir, { recursive: true });
	mkdirSync(cwd, { recursive: true });
	const files = { global: join(agentDir, "settings.json"), project: join(cwd, ".pi", "settings.json") };
	const managers = {};
	const steps = [];
	for (const step of scenario.steps) {
		const m = managers[step.name];
		let result = null;
		try {
			switch (step.op) {
				case "write": mkdirSync(dirname(files[step.file]), { recursive: true }); writeFileSync(files[step.file], step.text); break;
				case "create": managers[step.name] = SettingsManager.create(cwd, agentDir, { projectTrusted: step.trusted ?? true }); break;
				case "view": result = settingsView(m); break;
				case "reload": await m.reload(); break;
				case "setAutoreviewSettings": m.setAutoreviewSettings(patchObject(step.patch)); break;
				case "setLokiSettings": m.setLokiSettings(patchObject(step.patch)); break;
				case "setDefaultModelAndProvider": m.setDefaultModelAndProvider(step.provider, step.model); break;
				case "setModelThinkingLevel": m.setModelThinkingLevel(step.provider, step.model, step.value); break;
				case "removeModelThinkingLevel": m.removeModelThinkingLevel(step.provider, step.model); break;
				default: m[step.op](step.value);
			}
			if (m) await m.flush();
		} catch (e) {
			result = { error: errorText(e) };
		}
		steps.push({ ...step, result, files: { global: read(files.global), project: read(files.project) } });
	}
	return { name: scenario.name, steps };
}

// Two processes write disjoint autoreview keys to one settings.json at once; every key must survive.
async function raceSettings() {
	const agentDir = join(root, "race", "agent");
	const cwd = join(root, "race", "project");
	mkdirSync(agentDir, { recursive: true });
	mkdirSync(cwd, { recursive: true });
	writeFileSync(join(agentDir, "settings.json"), JSON.stringify({ autoreview: { model: "p/m" } }));
	const writes = 15;
	const worker = (tag) => `
		const { SettingsManager } = await import(${JSON.stringify(pathToFileURL(join(repo, "packages/coding-agent/src/core/settings-manager.ts")).href)});
		const m = SettingsManager.create(${JSON.stringify(cwd)}, ${JSON.stringify(agentDir)});
		for (let i = 0; i < ${writes}; i++) { m.setAutoreviewSettings({ ["${tag}" + i]: i }); await m.flush(); }
		const errors = m.drainErrors(); if (errors.length) { console.error(errors); process.exit(1); }`;
	const run = (tag) => new Promise((ok, fail) => {
		const child = spawn(process.execPath, ["--input-type=module", "-e", worker(tag)], { stdio: ["ignore", "inherit", "inherit"] });
		child.on("exit", (code) => (code === 0 ? ok() : fail(new Error(`worker ${tag} exited ${code}`))));
	});
	await Promise.all([run("a"), run("b")]);
	const final = JSON.parse(readFileSync(join(agentDir, "settings.json"), "utf-8"));
	return { writes, keys: Object.keys(final.autoreview).sort(), lockLeft: existsSync(join(agentDir, "settings.json.lock")) };
}

// ---------------------------------------------------------------- auth
const AUTH_STEPS = [
	{ op: "list" },
	{ op: "modify", provider: "anthropic", value: { type: "oauth", access: "acc", refresh: "ref", expires: 1700000000000, extra: { a: 1 } } },
	{ op: "write", text: JSON.stringify({ anthropic: { type: "oauth", access: "acc", refresh: "ref", expires: 1700000000000 }, external: { type: "api_key", key: "ext" } }) },
	{ op: "modify", provider: "openai", value: { type: "api_key", key: "sk-${FOO}-$BAR_X", env: { FOO: "foo", BAR_X: "bar" } } },
	{ op: "read", provider: "openai" }, { op: "read", provider: "anthropic" }, { op: "read", provider: "missing" },
	{ op: "modify", provider: "openai" },
	{ op: "modify", provider: "unset", value: { type: "api_key", key: "$ULTRON_CONFIG_FIXTURE_UNSET" } }, { op: "read", provider: "unset" },
	{ op: "modify", provider: "nokey", value: { type: "api_key", env: { A: "b" } } }, { op: "read", provider: "nokey" },
	{ op: "list" }, { op: "delete", provider: "external" }, { op: "delete", provider: "never-there" }, { op: "list" },
	{ op: "write", text: "{ not json" }, { op: "modify", provider: "x", value: { type: "api_key", key: "k" } }, { op: "delete", provider: "anthropic" },
	{ op: "write", text: "﻿" + JSON.stringify({ bom: { type: "api_key", key: "plain" } }) }, { op: "read", provider: "bom" },
	{ op: "modify", provider: "z", value: { type: "api_key", key: "zz" } },
];

async function runAuth() {
	const path = join(root, "auth", "nested", "auth.json");
	const storage = AuthStorage.create(path);
	const steps = [];
	for (const step of AUTH_STEPS) {
		let result = null;
		try {
			switch (step.op) {
				case "write": writeFileSync(path, step.text); break;
				case "list": result = await storage.list(); break;
				case "read": result = (await storage.read(step.provider)) ?? null; break;
				case "modify": result = (await storage.modify(step.provider, async () => step.value)) ?? null; break;
				case "delete": await storage.delete(step.provider); break;
			}
		} catch (e) {
			result = { error: true };
		}
		steps.push({ ...step, result: rel(result), file: read(path), lockLeft: existsSync(`${path}.lock`) });
	}
	const mode = (execFileSync("stat", ["-c", "%a", path], { encoding: "utf-8" }).trim());
	const dirMode = (execFileSync("stat", ["-c", "%a", dirname(path)], { encoding: "utf-8" }).trim());
	return { steps, mode, dirMode };
}

const VALIDATION_FILES = [
	{ a: { type: "api_key", key: "k", env: { X: "y" } }, b: { type: "oauth", access: "a", refresh: "r", expires: 1 } },
	{ a: { type: "api_key", key: 1 } }, { a: { type: "api_key", env: { X: 1 } } }, { a: { type: "api_key", env: [] } },
	{ a: { type: "oauth", access: "a", refresh: "r" } }, { a: { type: "oauth", access: "a", refresh: "r", expires: "1" } },
	{ a: { type: "other" } }, { a: "string" }, { a: null }, [], 3,
];
async function runValidation() {
	const out = [];
	for (const [i, data] of VALIDATION_FILES.entries()) {
		const path = join(root, "readonly", `${i}.json`);
		mkdirSync(dirname(path), { recursive: true });
		writeFileSync(path, JSON.stringify(data));
		let result;
		try { result = { ok: await new ReadOnlyAuthStorage(path).list() }; } catch (e) { result = { error: errorText(e) }; }
		out.push({ text: JSON.stringify(data), result, stored: rel(readStoredCredential("a", path)) });
	}
	return out;
}

const TEMPLATES = ["plain", "$A", "${A}", "x-$A-y", "$A$B", "$$A", "$!A", "${not valid}", "${A", "$", "$1A", "a$", "${}", "$A_B9", "$MISSING", "${MISSING}-x", "pre$Bpost", "${A}${B}", "$-x"];
const runTemplates = () => TEMPLATES.map((t) => ({ template: t, resolved: resolveConfigValue(t, { A: "1", B: "two", A_B9: "ab", Bpost: "" }) ?? null }));

// ---------------------------------------------------------------- models store
const MODELS_STEPS = [
	{ op: "read", provider: "openai" },
	{ op: "write", provider: "openai", entry: { models: [{ id: "gpt", contextWindow: 128000, cost: { input: 2.5 } }], fetchedAt: 1 } },
	{ op: "external", text: JSON.stringify({ openai: { models: [] }, other: { models: [{ id: "o" }] } }) },
	{ op: "write", provider: "anthropic", entry: { models: [] } }, { op: "read", provider: "other" },
	{ op: "delete", provider: "openai" }, { op: "read", provider: "openai" },
	{ op: "external", text: "{bad" }, { op: "write", provider: "x", entry: {} }, { op: "read", provider: "x" },
];
async function runModelsStore() {
	const path = join(root, "models", "models-store.json");
	const store = new FileModelsStore(path);
	const steps = [];
	for (const step of MODELS_STEPS) {
		let result = null;
		try {
			switch (step.op) {
				case "external": writeFileSync(path, step.text); break;
				case "read": result = (await store.read(step.provider)) ?? null; break;
				case "write": await store.write(step.provider, step.entry); break;
				case "delete": await store.delete(step.provider); break;
			}
		} catch {
			result = { error: true };
		}
		steps.push({ ...step, result, file: read(path) });
	}
	return steps;
}

// ---------------------------------------------------------------- trust
function runTrust() {
	const base = join(root, "trust");
	const agentDir = join(base, "agent");
	const work = join(base, "work");
	for (const dir of ["work/a/b", "work/pi/.pi/extensions", "work/agents/.agents/skills", "work/plain", "home/.agents/skills", "work/link-target"]) mkdirSync(join(base, dir), { recursive: true });
	writeFileSync(join(base, "work/pi2.txt"), "");
	execFileSync("ln", ["-s", join(work, "link-target"), join(work, "link")]);
	mkdirSync(join(base, "work/settings/.pi"), { recursive: true });
	writeFileSync(join(base, "work/settings/.pi/settings.json"), "{}");
	process.env.HOME = join(base, "home");
	const store = new trust.ProjectTrustStore(agentDir);
	const steps = [];
	const ops = [
		{ op: "get", path: "work/a/b" },
		{ op: "setMany", updates: [["work/a", true], ["work/plain", false]] },
		{ op: "get", path: "work/a/b" }, { op: "getEntry", path: "work/a/b" }, { op: "get", path: "work/plain" }, { op: "get", path: "work" },
		{ op: "set", path: "work/a/b", decision: false }, { op: "get", path: "work/a/b" },
		{ op: "set", path: "work/link", decision: true }, { op: "getEntry", path: "work/link-target" },
		{ op: "setMany", updates: [["work", true], ["work/a", null]] }, { op: "getEntry", path: "work/a" },
		{ op: "write", text: JSON.stringify({ "/x": "yes" }) }, { op: "get", path: "work" },
		{ op: "write", text: "nope" }, { op: "get", path: "work" },
		{ op: "write", text: JSON.stringify({ "/z": true, "/a": null, "/m": false }) }, { op: "set", path: "work/plain", decision: true },
	];
	const trustPath = join(agentDir, "trust.json");
	for (const step of ops) {
		let result = null;
		try {
			switch (step.op) {
				case "write": writeFileSync(trustPath, step.text); break;
				case "get": result = store.get(join(base, step.path)); break;
				case "getEntry": result = store.getEntry(join(base, step.path)); break;
				case "set": store.set(join(base, step.path), step.decision); break;
				case "setMany": store.setMany(step.updates.map(([p, d]) => ({ path: join(base, p), decision: d }))); break;
			}
		} catch (e) {
			result = { error: errorText(e) };
		}
		steps.push({ ...step, result: rel(result), file: read(trustPath) });
	}
	const resources = ["work/pi", "work/agents", "work/agents/.agents", "work/plain", "work/settings", "home", "work/a/b"].map((p) => ({ path: p, requires: trust.hasTrustRequiringProjectResources(join(base, p)) }));
	const options = [true, false].map((s) => rel(trust.getProjectTrustOptions(join(base, "work/a"), { includeSessionOnly: s })));
	return { steps, resources, options, rootOptions: rel(trust.getProjectTrustOptions("/", {})) };
}

// ---------------------------------------------------------------- paths
function runPaths() {
	const cases = [undefined, "", "~", "~/custom/agent", "/abs/dir/../agent", "relative/agent", "file:///tmp/a%20b"];
	return cases.map((value) => {
		if (value === undefined) delete process.env.ULTRON_CODING_AGENT_DIR; else process.env.ULTRON_CODING_AGENT_DIR = value;
		return rel({ env: value ?? null, agentDir: config.getAgentDir(), auth: config.getAuthPath(), settings: config.getSettingsPath(),
			models: config.getModelsPath(), themes: config.getCustomThemesDir(), tools: config.getToolsDir(), bin: config.getBinDir(),
			prompts: config.getPromptsDir(), sessions: config.getSessionsDir(), debugLog: config.getDebugLogPath() });
	});
}

const home = process.env.HOME;
const fixture = {
	source: "packages/coding-agent/src (config.ts, core/settings-manager.ts, core/auth-storage.ts, core/resolve-config-value.ts, core/trust-manager.ts)",
	envAgentDir: config.ENV_AGENT_DIR,
	settings: [],
};
for (const [i, s] of SETTINGS_SCENARIOS.entries()) fixture.settings.push(await runSettings(s, i));
fixture.race = await raceSettings();
fixture.auth = await runAuth();
fixture.validation = await runValidation();
fixture.modelsStore = await runModelsStore();
fixture.templates = runTemplates();
process.env.HOME = home;
fixture.paths = runPaths();
fixture.trust = runTrust();
writeFileSync(join(here, "config.json"), `${JSON.stringify(fixture, null, "\t")}\n`);
rmSync(root, { recursive: true, force: true });
