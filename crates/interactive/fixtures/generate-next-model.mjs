// Generates crates/interactive/fixtures/next-model.json from this worktree's TypeScript Ctrl+P cycling
// (`experimental/client-tui-actions.ts` `nextModel`): the model Ctrl+P (1) or Shift+Ctrl+P (-1) moves to over the
// available models, in or out of the `enabledModels` scope (resolveModelScopeFromModels), or the status it shows.
// Model lists reuse crates/ai/fixtures/model-resolver.json's, plus the built-in catalog from an offline runtime
// (PI_OFFLINE=1, no credentials); summaries carry the full record as the TUI's catalog does. No network.
// Usage: node crates/interactive/fixtures/generate-next-model.mjs
// Every socket must stay on loopback (offline-guard.mjs): checked at the end.
import { assertOffline } from "../../cli/fixtures/offline-guard.mjs";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, "../../..");
const { nextModel } = await import(
	pathToFileURL(join(root, "packages/coding-agent/src/experimental/client-tui-actions.ts")).href
);
const lists = JSON.parse(readFileSync(join(root, "crates/ai/fixtures/model-resolver.json"), "utf8")).lists;
// The built-in catalog as the TUI lists it (an offline runtime without credentials; claude-code models left out).
for (const key of Object.keys(process.env)) delete process.env[key];
const scratch = mkdtempSync(join(tmpdir(), "interactive-next-model-"));
Object.assign(process.env, {
	HOME: scratch,
	PATH: "/usr/bin:/bin",
	PI_OFFLINE: "1",
	ULTRON_CODING_AGENT_DIR: join(scratch, "agent"),
	ULTRON_SERVER_DIR: join(scratch, "server"),
});
const { ModelRuntime } = await import(pathToFileURL(join(root, "packages/coding-agent/src/core/model-runtime.ts")).href);
const runtime = await ModelRuntime.create({ authPath: join(scratch, "auth.json"), modelsPath: join(scratch, "models.json") });
lists.catalog = runtime.getModels().filter((m) => m.provider !== "claude-code");
rmSync(scratch, { recursive: true, force: true });
const summaries = (name) =>
	lists[name].map((model) => ({
		provider: model.provider,
		modelId: model.id,
		name: model.name,
		reasoning: model.reasoning,
		model,
	}));

const cases = [
	["mock", "anthropic/claude-sonnet-4-5", 1, undefined],
	["mock", "anthropic/claude-sonnet-4-5", -1, undefined],
	["mock", "openrouter/openai/gpt-4o:extended", 1, []],
	["mock", "nobody/none", 1, undefined],
	["mock", "anthropic/claude-sonnet-4-5", 1, ["openai/*", "anthropic/*"]],
	["mock", "openai/gpt-4o", 1, ["openrouter/**", "sonnet"]],
	["mock", "anthropic/claude-sonnet-4-5", 1, ["sonnet:high", "gpt-4o:bogus"]],
	["mock", "anthropic/claude-sonnet-4-5", -1, ["*:high"]],
	["mock", "anthropic/claude-sonnet-4-5", 1, ["sonnet"]],
	["mock", "anthropic/claude-sonnet-4-5", 1, ["missing", "x/y"]],
	["mock", "anthropic/claude-sonnet-4-5", 1, ["!*sonnet*"]],
	["mock", "anthropic/claude-sonnet-4-5", 1, ["{openai,openrouter}/*"]],
	["claudeCode", "claude-code/opus", 1, ["*sonnet*", "claude-code/*"]],
	["claudeCode", "anthropic/claude-sonnet-4-5", 1, ["sonnet", "haiku", "opus"]],
	["claudeCode", "claude-code/haiku", -1, ["claude-code/haiku", "claude-code/sonnet"]],
	["dated", "anthropic/claude-sonnet-4-5", 1, ["sonnet", "kimi", "gpt-x"]],
	["dated", "acme/kimi-k2.6", -1, ["acme/*:low"]],
	["dated", "other2/gpt-x", 1, ["gpt-x"]],
	["dated", "other/gpt-x-mini", 1, ["*gpt*"]],
	["empty", "a/b", 1, ["*"]],
	["catalog", "anthropic/claude-opus-5-5", 1, ["anthropic/claude-*-5-5", "openai/gpt-5.5"]],
	["catalog", "anthropic/claude-opus-5-5", -1, ["opus", "sonnet", "haiku"]],
];

const out = cases.map(([list, current, direction, enabled]) => {
	const [provider, ...rest] = current.split("/");
	const result = nextModel(summaries(list), { provider, modelId: rest.join("/") }, direction, enabled);
	return {
		list,
		current,
		direction,
		enabled: enabled ?? null,
		next: "error" in result ? null : `${result.model.provider}/${result.model.modelId}`,
		error: "error" in result ? result.error : null,
	};
});
const used = [...new Set(cases.map(([list]) => list))];
const fixtureLists = Object.fromEntries(
	used.map((name) => [name, summaries(name).map((m) => [m.provider, m.modelId, m.name, m.reasoning])]),
);
writeFileSync(join(here, "next-model.json"), `${JSON.stringify({ lists: fixtureLists, cases: out })}\n`);
assertOffline();
