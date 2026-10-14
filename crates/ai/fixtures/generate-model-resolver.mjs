// Generates crates/ai/fixtures/model-resolver.json from this worktree's TypeScript model resolver
// (`coding-agent/src/core/model-resolver.ts`): `parseModelPattern`, `resolveModelScopeFromModels` (globs through
// minimatch), `resolveCliModel` (fuzzy ids, `:<thinking>` suffixes, provider inference, ambiguity, the custom-id
// fallback) and `findInitialModel`, over the model lists of `model-resolver.test.ts` plus the built-in catalog.
// The runtimes are the same stand-ins the TypeScript tests use (`getModels`, `hasConfiguredAuth`, ...); the catalog
// list comes from an offline `ModelRuntime` with no credentials (PI_OFFLINE=1, PATH=/usr/bin:/bin), so nothing
// contacts a provider. Also records raw `minimatch(text, pattern, { nocase: true })` answers for the glob port.
// Usage: node crates/ai/fixtures/generate-model-resolver.mjs
// Every socket must stay on loopback (offline-guard.mjs): checked at the end.
import { assertOffline } from "../../cli/fixtures/offline-guard.mjs";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, "../../..");
const scratch = mkdtempSync(join(tmpdir(), "ai-model-resolver-"));
const home = join(scratch, "home");
mkdirSync(home);
for (const key of Object.keys(process.env)) delete process.env[key];
Object.assign(process.env, {
	HOME: home,
	PATH: "/usr/bin:/bin",
	PI_OFFLINE: "1",
	ULTRON_CODING_AGENT_DIR: join(scratch, "agent"),
	ULTRON_SERVER_DIR: join(scratch, "server"),
});

const core = (file) => import(pathToFileURL(join(root, "packages/coding-agent/src/core", file)).href);
const { ModelRuntime } = await core("model-runtime.ts");
const { findInitialModel, parseModelPattern, resolveCliModel, resolveModelScopeFromModels } = await core(
	"model-resolver.ts",
);
const { minimatch } = await import("minimatch");
const { CLAUDE_CODE_MODELS } = await import(pathToFileURL(join(root, "packages/ai/src/providers/claude-code.ts")).href);

const cost = { input: 1, output: 2, cacheRead: 0.1, cacheWrite: 1 };
const model = (provider, id, name, extra = {}) => ({
	id,
	name,
	api: "anthropic-messages",
	provider,
	baseUrl: `https://${provider}.invalid`,
	reasoning: false,
	input: ["text"],
	cost,
	contextWindow: 128000,
	maxTokens: 8192,
	...extra,
});

// model-resolver.test.ts's mocks.
const mockModels = [
	model("anthropic", "claude-sonnet-4-5", "Claude Sonnet 4.5", { reasoning: true, maxTokens: 8192 }),
	model("openai", "gpt-4o", "GPT-4o", { maxTokens: 4096 }),
];
const mockOpenRouterModels = [
	model("openrouter", "qwen/qwen3-coder:exacto", "Qwen3 Coder Exacto", { reasoning: true }),
	model("openrouter", "openai/gpt-4o:extended", "GPT-4o Extended"),
];
const allModels = [...mockModels, ...mockOpenRouterModels];
const claudeCode = CLAUDE_CODE_MODELS.map((m) => ({ ...m }));
const sol = (provider) => ({ ...mockModels[1], id: "gpt-5.6-sol", name: "GPT 5.6 Sol", provider });

const lists = {
	mock: allModels,
	claudeCode: [...claudeCode, ...allModels],
	bracketed: [...allModels, model("custom", "bracketed-model[1m]", "Bracketed Model", { reasoning: true })],
	ambiguous: [sol("azure-openai-responses"), sol("openai-codex")],
	gateway: [
		...allModels,
		model("zai", "glm-5", "GLM-5", { reasoning: true }),
		model("vercel-ai-gateway", "zai/glm-5", "GLM-5", { reasoning: true }),
	],
	commandcode: [
		...allModels,
		model("commandcode", "xiaomi/mimo-v2.5-pro", "Xiaomi MiMo via Commandcode"),
		model("xiaomi", "mimo-v2.5-pro", "Xiaomi MiMo"),
	],
	neuralwatt: [...allModels, model("neuralwatt", "some-base-model", "Some Base Model")],
	// Aliases against dated ids, names, case and punctuation for the alias ordering (localeCompare).
	dated: [
		model("anthropic", "claude-sonnet-4-5-20250929", "Claude Sonnet 4.5"),
		model("anthropic", "claude-sonnet-4-5", "Claude Sonnet 4.5 (latest)"),
		model("anthropic", "claude-sonnet-4-20250514", "Claude Sonnet 4"),
		model("anthropic", "claude-3-5-sonnet-20241022", "Claude Sonnet 3.5 v2"),
		model("anthropic", "claude-3-5-sonnet-20240620", "Claude Sonnet 3.5"),
		model("anthropic", "claude-3-7-sonnet-latest", "Claude Sonnet 3.7 (latest)"),
		model("anthropic", "claude-haiku-20240307", "Claude Haiku"),
		model("anthropic", "claude-haiku-20250101", "Claude Haiku Next"),
		model("acme", "Kimi-K2.6", "Moonshot Kimi"),
		model("acme", "kimi-k2.6", "kimi lower"),
		model("acme", "kimi_k2.6", "kimi underscore"),
		model("acme", "kimi.k2", "kimi dot"),
		model("acme", "kimi-k2-thinking", "Kimi K2 Thinking"),
		model("acme", "kimi-k10", "Kimi K10"),
		model("acme", "kimi-k9", "Kimi K9"),
		model("acme", "anything", "Uses Sonnet In Its Name"),
		model("other", "gpt-x", "GPT X"),
		model("other", "gpt-x-mini", "GPT X Mini"),
		model("other2", "gpt-x", "GPT X again"),
	],
	empty: [],
};

// The built-in catalog as resolveCliModel sees it (`getModels()`); claude-code models are left out (the Rust build
// lists them from its own provider).
const runtime = await ModelRuntime.create({ authPath: join(scratch, "auth.json"), modelsPath: join(scratch, "models.json") });
lists.catalog = runtime.getModels().filter((m) => m.provider !== "claude-code");

const ref = (m) => (m ? `${m.provider}/${m.id}` : null);
const known = (listName, m) => lists[listName].some((x) => x.provider === m.provider && x.id === m.id && x === m);
const out = (listName, m) => {
	if (!m) return null;
	// A model not taken from the list (the custom-id fallback) is recorded in full.
	return known(listName, m) ? { ref: ref(m) } : { ref: ref(m), model: JSON.parse(JSON.stringify(m)) };
};
const fake = (listName, auth) => {
	const models = lists[listName];
	return {
		getModels: () => models,
		getModel: (provider, id) => models.find((m) => m.provider === provider && m.id === id),
		hasConfiguredAuth: (provider) => auth.includes(provider),
		getAvailableSnapshot: () => models.filter((m) => auth.includes(m.provider)),
	};
};

const parse = [
	["mock", "claude-sonnet-4-5", true],
	["mock", "sonnet", true],
	["mock", "nonexistent", true],
	["mock", "sonnet:high", true],
	["mock", "gpt-4o:medium", true],
	...["off", "minimal", "low", "medium", "high", "xhigh", "max"].map((level) => ["mock", `sonnet:${level}`, true]),
	["mock", "sonnet:random", true],
	["mock", "gpt-4o:invalid", true],
	["mock", "sonnet:random", false],
	["mock", "qwen3-coder:exacto", true],
	["mock", "openrouter/qwen/qwen3-coder:exacto", true],
	["mock", "qwen3-coder:exacto:high", true],
	["mock", "openrouter/qwen/qwen3-coder:exacto:high", true],
	["mock", "gpt-4o:extended", true],
	["mock", "qwen3-coder:exacto:random", true],
	["mock", "qwen3-coder:exacto:high:random", true],
	["mock", "", true],
	["mock", "sonnet:", true],
	["mock", "GPT-4O", true],
	["mock", "OpenAI/GPT-4o", true],
	["mock", "  openai/gpt-4o  ", true],
	["mock", "Claude Sonnet", true],
	["claudeCode", "sonnet", true],
	["claudeCode", "haiku", true],
	["claudeCode", "claude-code/haiku", true],
	["claudeCode", "opus", true],
	["dated", "sonnet", true],
	["dated", "claude-sonnet-4", true],
	["dated", "3-5-sonnet", true],
	["dated", "haiku", true],
	["dated", "kimi", true],
	["dated", "KIMI-K2", true],
	["dated", "gpt-x", true],
	["dated", "other/gpt-x", true],
	["dated", "x mini", true],
	["dated", "kimi:high", true],
	["dated", "kimi:high:low", true],
	["dated", "kimi:bogus:high", true],
	["ambiguous", "gpt-5.6-sol", true],
	["ambiguous", "sol", true],
].map(([list, pattern, allow]) => {
	const result = parseModelPattern(pattern, lists[list], { allowInvalidThinkingLevelFallback: allow });
	return {
		list,
		pattern,
		allowInvalidThinkingLevelFallback: allow,
		model: out(list, result.model),
		thinkingLevel: result.thinkingLevel ?? null,
		warning: result.warning ?? null,
	};
});

const scope = [
	["mock", ["sonnet:high", "gpt-4o:invalid", "missing"]],
	["mock", ["missing"]],
	["mock", ["*"]],
	["mock", ["*:high"]],
	["mock", ["openrouter/*"]],
	["mock", ["openrouter/**"]],
	["mock", ["*gpt*", "gpt-4o", "*:low"]],
	["mock", ["?pt-4o", "GPT-4?", "[go]pt-4o"]],
	["mock", ["[!g]*"]],
	["mock", ["{openai,anthropic}/*"]],
	["mock", ["*/qwen*"]],
	["mock", ["openrouter/qwen/*:xhigh"]],
	["mock", ["*:bogus"]],
	["mock", ["!*sonnet*"]],
	["mock", ["#comment*"]],
	["mock", ["anthropic/claude-sonnet-4-5:max", "anthropic/*"]],
	["claudeCode", ["*sonnet*", "claude-code/*"]],
	["claudeCode", ["*opus*"]],
	["bracketed", ["custom/bracketed-model[1m]"]],
	["bracketed", ["custom/bracketed-model[1m]:high"]],
	["bracketed", ["bracketed-model[1m]"]],
	["bracketed", ["*[1m]"]],
	["dated", ["sonnet", "*sonnet*", "acme/kimi*:low", "other*/gpt-x"]],
	["dated", ["kimi-k[0-9]*", "*-2025????"]],
	["dated", ["gpt-x", "gpt-x"]],
	["catalog", ["anthropic/*", "*sonnet*:high"]],
	["catalog", ["opus", "gpt-5.5", "*/kimi-k2*", "claude-opus-4-8:max"]],
	["catalog", ["openrouter/anthropic/*"]],
	["catalog", ["openrouter/*/*opus*"]],
].map(([list, patterns]) => {
	const result = resolveModelScopeFromModels(patterns, lists[list]);
	return {
		list,
		patterns,
		scoped: result.scopedModels.map((s) => ({ ref: ref(s.model), thinkingLevel: s.thinkingLevel ?? null })),
		diagnostics: result.diagnostics,
	};
});

const cli = [
	["mock", undefined, "openai/gpt-4o", undefined, []],
	["mock", "openai", "4o", undefined, []],
	["mock", undefined, "sonnet:high", undefined, []],
	["mock", undefined, "openai/gpt-4o:extended", undefined, []],
	["mock", "openai", "gpt-4o:extended", undefined, []],
	["mock", "openrouter", "openrouter/openai/ghost-model", undefined, []],
	["empty", "openai", "gpt-4o", undefined, []],
	["empty", undefined, undefined, undefined, []],
	["ambiguous", undefined, "gpt-5.6-sol", undefined, ["openai-codex"]],
	["ambiguous", undefined, "gpt-5.6-sol", undefined, []],
	["ambiguous", undefined, "gpt-5.6-sol", undefined, ["openai-codex", "azure-openai-responses"]],
	["gateway", undefined, "zai/glm-5", undefined, ["zai", "vercel-ai-gateway"]],
	["commandcode", undefined, "xiaomi/mimo-v2.5-pro", undefined, ["commandcode"]],
	["commandcode", undefined, "xiaomi/mimo-v2.5-pro", undefined, []],
	["commandcode", undefined, "xiaomi/mimo-v2.5-pro", undefined, ["xiaomi", "commandcode"]],
	["mock", undefined, "openrouter/qwen", undefined, []],
	["neuralwatt", undefined, "neuralwatt/zai-org/GLM-5.1-FP8:high", undefined, []],
	["neuralwatt", undefined, "neuralwatt/zai-org/GLM-5.1-FP8", undefined, []],
	...["off", "minimal", "low", "medium", "high", "xhigh", "max"].map((level) => [
		"neuralwatt",
		undefined,
		`neuralwatt/zai-org/GLM-5.1-FP8:${level}`,
		undefined,
		[],
	]),
	["neuralwatt", undefined, "neuralwatt/zai-org/GLM-5.1-FP8:banana", undefined, []],
	["neuralwatt", "neuralwatt", "zai-org/GLM-5.1-FP8:high", undefined, []],
	["neuralwatt", undefined, "neuralwatt/zai-org/GLM-5.1-FP8:high", "medium", []],
	["neuralwatt", undefined, "neuralwatt/zai-org/GLM-5.1-FP8:off", undefined, []],
	["mock", "nosuch", "gpt-4o", undefined, []],
	["mock", "OPENAI", "OpenAI/GPT-4O", undefined, []],
	["mock", undefined, "nosuch/model", undefined, []],
	["mock", undefined, "nosuchmodel", undefined, []],
	["mock", undefined, "sonnet:random", undefined, []],
	["mock", undefined, "anthropic/sonnet:random", undefined, []],
	["mock", undefined, "GPT-4o", undefined, []],
	["mock", undefined, "gpt", undefined, []],
	["mock", "anthropic", "gpt-4o", undefined, []],
	["mock", "anthropic", "brand-new:low", undefined, []],
	["claudeCode", undefined, "haiku", undefined, ["claude-code"]],
	["claudeCode", undefined, "claude-code/haiku", undefined, []],
	["claudeCode", undefined, "sonnet", undefined, ["claude-code"]],
	["dated", undefined, "kimi-k2.6", undefined, []],
	["dated", undefined, "kimi-k2.6", undefined, ["acme"]],
	["dated", undefined, "gpt-x", undefined, ["other"]],
	["dated", undefined, "gpt-x", undefined, []],
	["dated", undefined, "anthropic/sonnet", undefined, []],
	["dated", undefined, "anthropic/claude-sonnet-5", undefined, []],
	["dated", "acme", "nope:high", "low", []],
	["catalog", undefined, "sonnet", undefined, []],
	["catalog", undefined, "opus:high", undefined, []],
	["catalog", undefined, "anthropic/opus", undefined, []],
	["catalog", undefined, "gpt-5.5", undefined, []],
	["catalog", undefined, "gpt-5.5", undefined, ["openai"]],
	["catalog", undefined, "kimi", undefined, []],
	["catalog", undefined, "openrouter/moonshotai/kimi-k2.6", undefined, []],
	["catalog", undefined, "moonshotai/kimi-k2.6", undefined, []],
	["catalog", undefined, "zai/glm-5.1", undefined, []],
	["catalog", "openrouter", "qwen", undefined, []],
	["catalog", "openai", "my-finetune:high", undefined, []],
	["catalog", undefined, "deepseek/deepseek-v9", undefined, []],
	["catalog", undefined, "claude-opus-4-8", undefined, []],
	["catalog", undefined, "haiku", undefined, []],
].map(([list, cliProvider, cliModel, cliThinking, auth]) => {
	const result = resolveCliModel({ cliProvider, cliModel, cliThinking, modelRuntime: fake(list, auth) });
	return {
		list,
		cliProvider: cliProvider ?? null,
		cliModel: cliModel ?? null,
		cliThinking: cliThinking ?? null,
		auth,
		model: out(list, result.model),
		thinkingLevel: result.thinkingLevel ?? null,
		warning: result.warning ?? null,
		error: result.error ?? null,
	};
});

const initial = [];
for (const [list, patterns, isContinuing, settings, auth] of [
	["mock", ["gpt-4o", "sonnet"], false, {}, []],
	["mock", ["gpt-4o:high", "sonnet"], false, { defaultThinkingLevel: "low" }, []],
	["mock", ["gpt-4o", "sonnet"], false, { modelThinkingLevels: { "openai/gpt-4o": "minimal" }, defaultThinkingLevel: "low" }, []],
	["mock", ["gpt-4o", "sonnet"], false, { defaultThinkingLevel: "xhigh" }, []],
	["mock", ["gpt-4o"], true, { defaultProvider: "anthropic", defaultModelId: "claude-sonnet-4-5" }, ["anthropic"]],
	["mock", ["gpt-4o"], true, { defaultProvider: "anthropic", defaultModelId: "claude-sonnet-4-5" }, []],
	["mock", [], false, { defaultProvider: "anthropic", defaultModelId: "claude-sonnet-4-5", defaultThinkingLevel: "high", modelThinkingLevels: { "anthropic/claude-sonnet-4-5": "max" } }, ["anthropic"]],
	["mock", [], false, {}, ["openrouter", "openai"]],
	["mock", ["missing"], false, {}, []],
	["claudeCode", [], false, {}, ["claude-code"]],
	["claudeCode", [], false, {}, ["claude-code", "openrouter"]],
	["catalog", [], false, {}, ["openrouter", "anthropic", "openai"]],
	["catalog", [], false, {}, ["deepseek"]],
]) {
	const scoped = resolveModelScopeFromModels(patterns, lists[list]).scopedModels;
	const result = await findInitialModel({ scopedModels: scoped, isContinuing, ...settings, modelRuntime: fake(list, auth) });
	initial.push({
		list,
		patterns,
		isContinuing,
		settings,
		auth,
		model: out(list, result.model),
		thinkingLevel: result.thinkingLevel,
	});
}

const globs = [
	["anthropic/claude-sonnet-4-5", "*sonnet*"],
	["claude-sonnet-4-5", "*sonnet*"],
	["openrouter/qwen/qwen3-coder:exacto", "openrouter/*"],
	["openrouter/qwen/qwen3-coder:exacto", "openrouter/**"],
	["openrouter/qwen/qwen3-coder:exacto", "**/qwen3*"],
	["openrouter/qwen/qwen3-coder:exacto", "openrouter/**/*exacto"],
	["a/b", "a/**/b"],
	["a/.b", "a/*"],
	["a/.b", "a/.*"],
	[".hidden", "*"],
	[".hidden", "?hidden"],
	["a/", "a/*"],
	["a", "a/*"],
	["", "*"],
	["", ""],
	["GPT-4o", "gpt-4?"],
	["gpt-4o", "[gG]pt-[0-9]o"],
	["gpt-4o", "[!a-f]pt-4o"],
	["gpt-4o", "[^g]pt-4o"],
	["gpt-4o", "[]pt-4o"],
	["gpt-4o", "gpt-4[o"],
	["gpt-4[o", "gpt-4[o"],
	["a]b", "a]b"],
	["model[1m]", "model[1m]"],
	["model1", "model[1m]"],
	["model[1m]", "model\\[1m\\]"],
	["x*y", "x\\*y"],
	["xzy", "x\\*y"],
	["openai/gpt-4o", "{openai,anthropic}/*"],
	["anthropic/x", "{openai,anthropic}/*"],
	["mistral/x", "{openai,anthropic}/*"],
	["ab", "a{b,c{d,e}}"],
	["ace", "a{b,c{d,e}}"],
	["a{b", "a{b"],
	["a1", "a{1..3}"],
	["claude-sonnet", "!*opus*"],
	["claude-opus", "!*opus*"],
	["claude-opus", "!!*opus*"],
	["#x", "#*"],
	["abc", "a*b*c"],
	["abcbc", "a*bc"],
	["aXb", "a?b"],
	["a/b", "a?b"],
	["kimi-k2.6", "kimi-k[0-9].[0-9]"],
	["Kimi-K2.6", "KIMI-K[0-9]*"],
	["x", "[[:alpha:]]"],
	["1", "[[:alpha:]]"],
	["a-b", "a[a-z-]b"],
	["a.b", "a[.]b"],
	["foo", "+(f|o)"],
	["foo", "@(foo|bar)"],
	["a", "*(a)"],
	["ab", "a?(b)"],
].map(([text, pattern]) => ({ text, pattern, match: minimatch(text, pattern, { nocase: true }) }));

rmSync(scratch, { recursive: true, force: true });
const fixture = {
	lists: Object.fromEntries(
		Object.entries(lists).map(([name, models]) => [
			name,
			name === "catalog" ? models.map(ref) : JSON.parse(JSON.stringify(models)),
		]),
	),
	parse,
	scope,
	cli,
	initial,
	globs,
};
writeFileSync(join(here, "model-resolver.json"), `${JSON.stringify(fixture, null, "\t")}\n`);
assertOffline();
