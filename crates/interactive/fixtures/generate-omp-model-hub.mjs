// Generates crates/interactive/fixtures/omp-model-hub-parity.json from omp's model browser ordering (oh-my-pi
// packages/tui/src/overlays/model-browser.ts: extractVersionNumber, computeModelRank, modelItemComparator,
// compactModelSearchText and modelSearchTier, sliced out because the module imports the whole TUI).
//
// Role assignments are empty (Ultron has no model roles), so computeModelRank ties and the comparator orders by
// MRU, provider, priority (unset), version and recency. Sort cases list their input order and the sorted order.
//
// Usage: OMP_SRC=<oh-my-pi checkout> node crates/interactive/fixtures/generate-omp-model-hub.mjs
//        (deterministic; default OMP_SRC is ../omp-reference-src/oh-my-pi beside the worktree's parent)
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const omp = resolve(process.env.OMP_SRC ?? join(here, "../../../../../omp-reference-src/oh-my-pi"));
const source = readFileSync(join(omp, "packages/tui/src/overlays/model-browser.ts"), "utf8");
const slice = (start, end) => {
	const a = source.indexOf(start);
	const b = source.indexOf(end, a + 1);
	if (a < 0 || b < 0) throw new Error(`slice not found: ${start}`);
	return source.slice(a, b);
};
const dir = mkdtempSync(join(tmpdir(), "omp-model-hub-"));
writeFileSync(
	join(dir, "browser.ts"),
	`type Model = any; type ModelBrowserItem = any; type RoleAssignments = any; type SortModelItemsOptions = any; type ModelRole = string;
function modelsAreEqual(a: any, b: any) { return a.provider === b.provider && a.id === b.id; }
${slice("export const MODEL_ROLE_IDS", "export const CHAT_MODEL_ROLE_IDS")}
${slice("/** Extract the first version number", "/** Options for {@link sortModelItems}")}
${slice("export function sortModelItems", "/** Picker candidates and ordering inputs")}
${slice("/** Collapse punctuation", "/** Inputs to {@link rankModelItems}")}
export { extractVersionNumber, compactModelSearchText, modelSearchTier };
`,
);
const B = await import(pathToFileURL(join(dir, "browser.ts")).href);
rmSync(dir, { recursive: true });

const ids = [
	"gemini-2.5-pro", "claude-sonnet-4-6", "gpt-4o", "llama-3-1-8b", "o3", "o3-mini", "claude-3-5-sonnet-20241022",
	"claude-3-5-sonnet-latest", "x-123-4", "v2_10-11", "model", "4-6", "qwen3:14b", "deepseek-r1-0528", "a-1.2.3",
	"gpt-5.5-codex", "claude-opus-4-1-20250805", "mistral-large-latest", "grok-4-0709", "foo-12-345", "_7-8",
];
const versions = ids.map((id) => [id, B.extractVersionNumber(id)]);

const item = (provider, id) => ({ provider, id, selector: `${provider}/${id}`, model: { provider, id } });
const pool = [
	item("anthropic", "claude-sonnet-4-6"), item("anthropic", "claude-opus-4-1-20250805"),
	item("anthropic", "claude-3-5-sonnet-latest"), item("anthropic", "claude-3-5-sonnet-20241022"),
	item("anthropic", "claude-3-5-sonnet-20240620"), item("openai", "gpt-4o"), item("openai", "gpt-5.5-codex"),
	item("openai", "o3"), item("openai", "o3-mini"), item("google", "gemini-2.5-pro"), item("google", "gemini-2.5-flash"),
	item("ollama", "zeta"), item("ollama", "alpha"), item("xai", "grok-4-0709"), item("xai", "grok-4-latest"),
];
const sorts = [];
for (const mru of [[], ["openai/o3", "google/gemini-2.5-flash"], ["xai/grok-4-latest", "missing/model", "anthropic/claude-3-5-sonnet-20240620"]]) {
	for (const rotate of [0, 5, 11]) {
		const input = [...pool.slice(rotate), ...pool.slice(0, rotate)];
		const sorted = [...input];
		B.sortModelItems(sorted, { roles: {}, mruOrder: mru });
		sorts.push({ mru, input: input.map((i) => i.selector), sorted: sorted.map((i) => i.selector) });
	}
}
const compact = ["GPT-4o", "claude sonnet 4.6", "o3/mini", "Ünïcode-Модель_7", "  "].map((s) => [s, B.compactModelSearchText(s)]);
const tiers = [];
for (const q of ["", "gpt4o", "openaigpt4o", "gpt", "4o", "o3", "zzz"])
	for (const [id, sel] of [["gpt4o", "openaigpt4o"], ["o3mini", "openaio3mini"]]) tiers.push([q, id, sel, B.modelSearchTier(q, id, sel)]);

writeFileSync(join(here, "omp-model-hub-parity.json"), `${JSON.stringify({ versions, sorts, compact, tiers })}\n`);
