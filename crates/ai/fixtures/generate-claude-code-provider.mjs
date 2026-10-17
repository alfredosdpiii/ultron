// Generates crates/ai/fixtures/claude-code-provider.json from this worktree's TypeScript model runtime: the always
// registered `claude-code` provider (packages/ai/src/providers/claude-code.ts) as `ModelRuntime` composes it, with
// and without a `claude` CLI where the provider looks for one (ULTRON_CLAUDE_CODE_BIN, else claude, claude.exe or
// claude.cmd on PATH). Per case: the provider's models (with any models.json overrides), availability
// (`getAvailableSnapshot`, `hasConfiguredAuth`, `isUsingSubscription`), request auth (`getAuth`), `/login`'s status
// (`loginProviderOptions`), `resolveCliModel`, `resolveModelScope`, `findInitialModel` and `listModels` output.
//
// The `claude` files are inert stand-ins written here (`exit 97`): the provider only checks that they exist and
// nothing runs them. Runs behind the offline guard with a scrubbed environment (no provider keys unless a case
// names a dummy one, PATH only what the case lists, PI_OFFLINE=1), so nothing leaves the machine.
// Usage: node crates/ai/fixtures/generate-claude-code-provider.mjs
import { assertOffline } from "../../cli/fixtures/offline-guard.mjs";
import { chmodSync, mkdirSync, mkdtempSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, "../../..");
const scratch = realpathSync(mkdtempSync(join(tmpdir(), "ai-claude-code-provider-")));
const home = join(scratch, "home");
mkdirSync(home);

// The CLI stand-ins: an executable `claude`, a non-executable one, and a `claude.cmd` only.
const stub = (dir, name, mode) => {
	mkdirSync(join(scratch, dir), { recursive: true });
	writeFileSync(join(scratch, dir, name), "#!/bin/sh\nexit 97\n");
	chmodSync(join(scratch, dir, name), mode);
};
stub("bin", "claude", 0o755);
stub("plain", "claude", 0o644);
stub("cmd", "claude.cmd", 0o755);
stub("home/.local/bin", "claude", 0o755);
mkdirSync(join(scratch, "empty"));

const ROOT = "<root>";
const expand = (value) => value.replaceAll(ROOT, scratch);
const baseEnv = { HOME: `${ROOT}/home`, PI_OFFLINE: "1" };

const core = (file) => import(pathToFileURL(join(root, "packages/coding-agent/src/core", file)).href);
const { ModelRuntime } = await core("model-runtime.ts");
const { resolveCliModel, resolveModelScopeWithDiagnostics, findInitialModel } = await core("model-resolver.ts");
const { listModels } = await import(pathToFileURL(join(root, "packages/coding-agent/src/cli/list-models.ts")).href);
const { loginProviderOptions } = await import(
	pathToFileURL(join(root, "packages/coding-agent/src/experimental/client-tui-auth.ts")).href
);
const { CLAUDE_CODE_MODELS } = await import(pathToFileURL(join(root, "packages/ai/src/providers/claude-code.ts")).href);

const resolveRefs = [
	[undefined, "claude-code/claude-haiku-5-5"],
	[undefined, "claude-code/claude-opus-5-5"],
	[undefined, "claude-code/haiku"],
	[undefined, "claude-code/sonnet:high"],
	[undefined, "claude-code/nosuch"],
	[undefined, "haiku"],
	[undefined, "sonnet"],
	[undefined, "claude-haiku-5-5"],
	["claude-code", "opus"],
	["claude-code", "claude-code/haiku"],
	["claude-code", "claude-haiku-5-5:low"],
];
const scopes = [["claude-code/*"], ["*haiku*"], ["haiku"], ["claude-code/haiku:high", "claude-code/opus"]];

const PATH_CLI = `${ROOT}/bin:/usr/bin:/bin`;
const cases = [
	{ name: "no cli", env: { PATH: "/usr/bin:/bin" }, list: ["claude-code", undefined] },
	{ name: "cli on PATH", env: { PATH: PATH_CLI }, list: ["claude-code", "haiku", undefined] },
	{ name: "non-executable cli on PATH", env: { PATH: `${ROOT}/plain:/usr/bin:/bin` }, list: ["claude-code"] },
	{ name: "claude.cmd on PATH", env: { PATH: `${ROOT}/cmd:/usr/bin:/bin` }, list: ["claude-code"] },
	{ name: "PATH entry with trailing slash", env: { PATH: `${ROOT}/empty:${ROOT}/bin/` }, list: ["claude-code"] },
	{ name: "windows-style PATH", env: { PATH: `${ROOT}/empty;${ROOT}/bin` }, list: ["claude-code"] },
	{ name: "bin env names the cli", env: { PATH: "/usr/bin:/bin", ULTRON_CLAUDE_CODE_BIN: `${ROOT}/bin/claude` }, list: ["claude-code"] },
	{ name: "bin env with spaces", env: { PATH: "/usr/bin:/bin", ULTRON_CLAUDE_CODE_BIN: `  ${ROOT}/bin/claude  ` }, list: ["claude-code"] },
	{ name: "bin env tilde path", env: { PATH: "/usr/bin:/bin", ULTRON_CLAUDE_CODE_BIN: "~/.local/bin/claude" }, list: ["claude-code"] },
	{ name: "bin env missing wins over PATH", env: { PATH: PATH_CLI, ULTRON_CLAUDE_CODE_BIN: `${ROOT}/missing/claude` }, list: ["claude-code"] },
	{ name: "blank bin env falls back to PATH", env: { PATH: PATH_CLI, ULTRON_CLAUDE_CODE_BIN: "   " }, list: ["claude-code"] },
	{ name: "no PATH", env: {}, list: ["claude-code"] },
	{
		name: "another provider has a key",
		env: { PATH: PATH_CLI, ANTHROPIC_API_KEY: "dummy-not-a-key" },
		list: ["haiku-5-5"],
		settings: [{}, { defaultProvider: "claude-code", defaultModelId: "claude-haiku-5-5" }],
	},
	{
		name: "only claude-code available",
		env: { PATH: PATH_CLI },
		list: ["claude-code"],
		settings: [{}, { defaultProvider: "claude-code", defaultModelId: "haiku" }],
	},
	{
		name: "models.json overrides",
		env: { PATH: PATH_CLI },
		models: {
			providers: {
				"claude-code": {
					modelOverrides: { haiku: { name: "Haiku (fast)", contextWindow: 100000, maxTokens: 8000 }, nosuch: { name: "x" } },
					models: [{ id: "claude-sonnet-5-5", name: "Sonnet 5.5 via CLI", contextWindow: 1000000, maxTokens: 64000, reasoning: true, input: ["text"] }],
				},
			},
		},
		list: ["claude-code"],
	},
	{
		name: "models.json key for claude-code",
		env: { PATH: "/usr/bin:/bin", CC_FIXTURE_KEY: "set" },
		models: { providers: { "claude-code": { apiKey: "$CC_FIXTURE_KEY" } } },
		list: ["claude-code"],
	},
];

function captureConsole(run) {
	const out = [];
	const err = [];
	const log = console.log;
	const error = console.error;
	console.log = (...args) => out.push(`${args.join(" ")}\n`);
	console.error = (...args) => err.push(`${args.join(" ")}\n`);
	return run()
		.finally(() => {
			console.log = log;
			console.error = error;
		})
		.then(() => ({ stdout: out.join(""), stderr: err.join("") }));
}

const json = (value) => (value === undefined ? null : JSON.parse(JSON.stringify(value)));
const ref = (m) => `${m.provider}/${m.id}`;
const docs = join(root, "packages/coding-agent/docs");
const unroot = (text) => text.replaceAll(docs, "<docs>").replaceAll(scratch, ROOT);

const out = [];
for (const spec of cases) {
	const dir = join(scratch, `case-${out.length}`);
	mkdirSync(dir);
	const modelsPath = join(dir, "models.json");
	if (spec.models) writeFileSync(modelsPath, JSON.stringify(spec.models, null, 2));
	for (const key of Object.keys(process.env)) delete process.env[key];
	Object.assign(process.env, Object.fromEntries(Object.entries({ ...baseEnv, ...spec.env }).map(([k, v]) => [k, expand(v)])));
	const runtime = await ModelRuntime.create({ authPath: join(dir, "auth.json"), modelsPath, refreshOnCreate: false, allowModelNetwork: false });
	await runtime.refresh({ allowNetwork: false });
	const available = runtime.getAvailableSnapshot().map(ref);
	const haiku = runtime.getModel("claude-code", "haiku");
	let auth;
	try {
		const resolution = await runtime.getAuth(haiku);
		auth = resolution ? { apiKey: resolution.auth.apiKey ?? null, headers: resolution.auth.headers ?? null } : null;
	} catch (error) {
		auth = { error: error.message };
	}
	const login = loginProviderOptions(runtime)
		.filter((o) => o.id === "claude-code")
		.map((o) => ({
			name: o.name,
			authType: o.authType,
			method: o.method.name ?? null,
			canLogin: typeof o.method.login === "function",
			status: o.status ? { type: o.status.type, source: o.status.source ?? null } : null,
		}));
	const resolve = resolveRefs.map(([cliProvider, cliModel]) => {
		const result = resolveCliModel({ cliProvider, cliModel, modelRuntime: runtime });
		return {
			provider: cliProvider ?? null,
			model: cliModel,
			result: result.model ? ref(result.model) : null,
			thinking: result.thinkingLevel ?? null,
			warning: result.warning ?? null,
			error: result.error ?? null,
		};
	});
	const scoped = [];
	for (const patterns of scopes) {
		const { scopedModels, diagnostics } = await resolveModelScopeWithDiagnostics(patterns, runtime);
		scoped.push({
			patterns,
			models: scopedModels.map((s) => ({ model: ref(s.model), thinking: s.thinkingLevel ?? null })),
			diagnostics: diagnostics.map((d) => d.message),
		});
	}
	const initial = [];
	for (const settings of spec.settings ?? [{}]) {
		const result = await findInitialModel({ scopedModels: [], isContinuing: false, ...settings, modelRuntime: runtime });
		initial.push({ settings, model: result.model ? ref(result.model) : null, thinking: result.thinkingLevel });
	}
	const list = [];
	for (const search of spec.list) {
		const captured = await captureConsole(() => listModels(runtime, search));
		list.push({ search: search ?? null, stdout: unroot(captured.stdout), stderr: unroot(captured.stderr) });
	}
	out.push({
		name: spec.name,
		env: spec.env,
		models: spec.models ?? null,
		error: runtime.getError() ? unroot(runtime.getError()) : null,
		providerModels: json(runtime.getModels("claude-code")),
		available,
		configured: runtime.hasConfiguredAuth("claude-code"),
		subscription: runtime.isUsingSubscription("claude-code"),
		auth,
		login,
		resolve,
		scoped,
		initial,
		list,
	});
}

const providerOrder = (await ModelRuntime.create({ authPath: join(scratch, "auth.json"), modelsPath: join(scratch, "none.json"), refreshOnCreate: false, allowModelNetwork: false }))
	.getProviders()
	.map((p) => p.id);
assertOffline();
rmSync(scratch, { recursive: true, force: true });
writeFileSync(
	join(here, "claude-code-provider.json"),
	`${JSON.stringify({ catalogModels: json(CLAUDE_CODE_MODELS), providerOrder, cases: out }, null, "\t")}\n`,
);
console.log(`wrote ${out.length} cases`);
