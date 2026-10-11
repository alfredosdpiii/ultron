// Generates crates/ai/fixtures/model-config.json from this worktree's TypeScript `ModelConfig.load`
// (packages/coding-agent/src/core/model-config.ts), `InMemoryModelsStore` and `flattenModelCatalog` (packages/ai).
// Each models.json case is written to a fresh temp dir (bytes as given; `content` is UTF-8 text, `hex` raw bytes) and
// loaded; the error (temp dir replaced by `<dir>`, home by `<home>`), provider ids and providers are recorded.
// Usage: node crates/ai/fixtures/generate-model-config.mjs
import { chmodSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, "../../..");
const scratch = mkdtempSync(join(tmpdir(), "ai-model-config-"));
const home = join(scratch, "home");
mkdirSync(home);
process.env.HOME = home;
process.env.ULTRON_CODING_AGENT_DIR = join(scratch, "agent");
process.env.ULTRON_SERVER_DIR = join(scratch, "server");

const { ModelConfig } = await import(pathToFileURL(join(root, "packages/coding-agent/src/core/model-config.ts")).href);
const { InMemoryModelsStore } = await import(pathToFileURL(join(root, "packages/ai/src/models-store.ts")).href);
const { flattenModelCatalog } = await import(pathToFileURL(join(root, "packages/ai/src/model-catalog.ts")).href);

const P = (provider) => JSON.stringify({ providers: { a: provider } });
const cost = { input: 1, output: 2, cacheRead: 0.1, cacheWrite: 1.25 };
const full = {
	providers: {
		local: {
			name: "Local",
			baseUrl: "http://localhost:8080/v1",
			apiKey: "LOCAL_KEY",
			api: "openai-completions",
			headers: { "x-a": "1" },
			authHeader: true,
			compat: { supportsStore: false, maxTokensField: "max_tokens", thinkingFormat: "qwen" },
			models: [
				{
					id: "m1",
					name: "Model 1",
					reasoning: true,
					thinkingLevelMap: { off: null, high: "high" },
					input: ["text", "image"],
					inputLimits: { maxRequestBytes: 1000, images: { resize: { maxWidth: 10, jpegQuality: 100 }, maxPerMessage: 2 } },
					cost: { ...cost, tiers: [{ inputTokensAbove: 200000, ...cost }] },
					promptCache: { short: 300, long: 3600 },
					contextWindow: 128000,
					maxTokens: 1.0,
					samplingParams: { top_k: 20, nested: { x: [1, 2] } },
					unknownField: 1,
				},
				{ id: "m2" },
			],
			modelOverrides: { m1: { name: "Renamed", cost: { input: 3 } } },
		},
		"2": { oauth: "radius" },
		"10": {},
		zeta: { compat: { allowedFallbackModels: [{ provider: "p", model: "m", cost }] } },
	},
	extra: true,
};

const cases = [
	["full valid config", { content: JSON.stringify(full, null, 2) }],
	["empty providers", { content: '{"providers":{}}' }],
	["comments, trailing commas and BOM", { content: '﻿{\n  // comment\n  "providers": {\n    "a": {"name": "x // not a comment", "baseUrl": "http://h/,}", },\n  }, // tail\n}\n' }],
	["escaped quotes inside strings", { content: '{"providers":{"a":{"name":"q\\"// x,}"}}}' }],
	["comment at end of file without newline", { content: '{"providers":{}} // done' }],
	["trailing comma before bracket with whitespace", { content: '{"providers":{"a":{"models":[{"id":"x"} ,\n\t]}}}' }],
	["line comment ends at LF only", { content: '{"providers":{} // c\r\n}' }],
	["duplicate keys keep first position", { content: '{"providers":{"b":{"name":"one"},"a":{},"b":{"name":"two"}}}' }],
	["invalid utf-8 bytes", { hex: Buffer.from('{"providers":{"a":{"name":"X"}}}').toString("hex").replace("58", "ff") }],
	["empty object", { content: "{}" }],
	["array root", { content: "[]" }],
	["null root", { content: "null" }],
	["number root", { content: "5" }],
	["providers not object", { content: '{"providers":5}' }],
	["providers array", { content: '{"providers":[]}' }],
	["empty id", { content: P({ models: [{ id: "" }] }) }],
	["missing id", { content: P({ models: [{}] }) }],
	["id wrong type", { content: P({ models: [{ id: 5 }] }) }],
	["oauth literal mismatch", { content: P({ oauth: "x", x: 1 }) }],
	["oauth literal wrong type", { content: P({ oauth: 5 }) }],
	["provider not object and empty name", { content: JSON.stringify({ providers: { a: 5, b: { name: "" } }, x: 1 }) }],
	["headers value and models type", { content: P({ headers: { x: 1 }, models: {} }) }],
	["integers, minimum, maximum, exclusiveMinimum, literal items", { content: P({ models: [{ id: "m", inputLimits: { maxRequestBytes: 1.5, images: { resize: { jpegQuality: 101, maxWidth: 0, maxHeight: 0.5 }, maxPerRequest: 0 } }, promptCache: { short: 0, long: -1 }, input: ["audio", 3] }] }) }],
	["integer and limits alone", { content: P({ models: [{ id: "m", inputLimits: { maxRequestBytes: 1.5, images: { resize: { jpegQuality: 100.5, maxWidth: 0, maxHeight: 0.5 }, maxPerRequest: "1" } } }] }) }],
	["exclusive minimum boundary", { content: P({ models: [{ id: "m", promptCache: { short: 0, long: 0.001 } }], modelOverrides: { m: { promptCache: { long: -1 } } } }) }],
	["cost missing all fields", { content: P({ models: [{ id: "m", cost: {} }] }) }],
	["cost wrong type and empty tier", { content: P({ models: [{ id: "m", cost: { input: "1", tiers: [{}] } }] }) }],
	["compat passes when one branch matches", { content: P({ compat: { supportsStore: 1, allowedFallbackModels: [1, 2, 3, 4] } }) }],
	["compat field failing every branch", { content: P({ compat: { supportsLongCacheRetention: 1 } }) }],
	["compat not an object", { content: P({ compat: 5 }) }],
	["compat branch errors capped", { content: P({ compat: { allowedFallbackModels: [1, 2, 3, 4], supportsStrictMode: 1, supportsLongCacheRetention: "x" } }) }],
	["compat maxItems and anyOf", { content: P({ compat: { allowedFallbackModels: [{ provider: "p", model: "m", cost }, 2, 3, 4], supportsLongCacheRetention: 1 } }) }],
	["compat fallback model fields", { content: P({ compat: { supportsLongCacheRetention: 1, allowedFallbackModels: [{ provider: "", model: 1, cost: {} }] } }) }],
	["compat deep openrouter routing", { content: P({ compat: { supportsLongCacheRetention: 0, openRouterRouting: { sort: 5, max_price: { prompt: true }, preferred_min_throughput: { p50: "x" }, data_collection: "maybe", order: [1] } } }) }],
	["compat chat template kwargs", { content: P({ compat: { supportsDeveloperRole: 1, sessionAffinityFormat: "x", chatTemplateKwargs: { a: { $var: "x" }, b: [1], c: null } } }) }],
	["thinkingLevelMap union and unknown keys", { content: P({ models: [{ id: 5, thinkingLevelMap: { off: 1, zz: 2 }, samplingParams: { a: {} } }] }) }],
	["model overrides", { content: P({ modelOverrides: { m: { cost: { input: "x" }, name: 1 }, n: 5 } }) }],
	["more than eight errors are capped", { content: P({ headers: Object.fromEntries(Array.from({ length: 12 }, (_, i) => [`h${i}`, i])) }) }],
	["record keys in JS order", { content: '{"providers":{"b":{"headers":{"z":1,"10":2,"2":3,"-1":4,"01":5}},"1":5,"4294967295":5,"4294967294":5}}' }],
	["record key with newline is not matched", { content: P({ headers: { "a\nb": 1, "c/d": 2, "": 3 } }) }],
	["booleans and numbers", { content: P({ authHeader: "yes", models: [{ id: "m", reasoning: 1, contextWindow: "big", maxTokens: null }] }) }],
	["string array items", { content: P({ compat: { supportsLongCacheRetention: 1, openRouterRouting: { only: ["a", 2] } } }) }],
	["empty file", { content: "" }],
	["whitespace file", { content: "  \n" }],
	["invalid json token", { content: "{providers}" }],
	["truncated json", { content: '{"providers":{' }],
	["comment only", { content: "// nothing" }],
	["missing file", { missing: true }],
	["directory instead of file", { directory: true }],
	["unreadable file", { content: "{}", mode: 0o000 }],
	["tilde path", { content: '{"providers":{"h":{}}}', tilde: true }],
	["no path", { noPath: true }],
];

const out = [];
for (const [name, spec] of cases) {
	const dir = join(scratch, `case-${out.length}`);
	mkdirSync(dir);
	let path = join(dir, "models.json");
	if (spec.tilde) {
		writeFileSync(join(home, "models.json"), spec.content);
		path = "~/models.json";
	} else if (spec.directory) {
		mkdirSync(path);
	} else if (spec.hex !== undefined) {
		writeFileSync(path, Buffer.from(spec.hex, "hex"));
	} else if (spec.content !== undefined) {
		writeFileSync(path, spec.content);
		if (spec.mode !== undefined) chmodSync(path, spec.mode);
	}
	const config = await ModelConfig.load(spec.noPath ? undefined : path);
	const error = config.getError()?.replaceAll(dir, "<dir>").replaceAll(home, "<home>");
	const ids = config.getProviderIds();
	out.push({
		name,
		...spec,
		error: error ?? null,
		providerIds: ids,
		providers: ids.map((id) => [id, config.getProvider(id)]),
	});
	if (spec.mode !== undefined) chmodSync(path, 0o600);
}

// InMemoryModelsStore: entries are cloned on write and read.
const store = new InMemoryModelsStore();
const model = { id: "m", name: "M", api: "openai-completions", provider: "p", baseUrl: "u", reasoning: false, input: ["text"], cost, contextWindow: 1, maxTokens: 1 };
const entry = { models: [model], lastModified: 1700000000, etag: '"abc"' };
await store.write("p", entry);
entry.models[0].name = "mutated after write";
const read1 = await store.read("p");
read1.models[0].name = "mutated after read";
const read2 = await store.read("p");
const missing = (await store.read("q")) ?? null;
await store.delete("p");
const afterDelete = (await store.read("p")) ?? null;
const storeOut = { read: read2, missing, afterDelete };

const groups = {
	"openai-completions": { a: { id: "a", api: "openai-completions" }, b: { id: "b", api: "openai-completions" } },
	"anthropic-messages": { c: { id: "c", api: "anthropic-messages" }, a: { id: "a", api: "anthropic-messages" } },
};
const catalog = flattenModelCatalog("p", groups);

rmSync(scratch, { recursive: true, force: true });
writeFileSync(
	join(here, "model-config.json"),
	`${JSON.stringify({ cases: out, store: storeOut, catalog: { groups, flat: catalog } }, null, "\t")}\n`,
);
