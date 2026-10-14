// Generates crates/ai/data/models.json, the built-in model catalog, from packages/ai/src/models.generated.ts.
// Shape: { provider: { modelId: Model } } in the generated catalog's order. Also writes catalog-meta.json with the
// bundled data's generation time (`getBuiltinModelDataGeneratedAt`, from the data manifest).
// Usage: node crates/ai/data/generate-models.mjs
import { readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const { MODELS } = await import(pathToFileURL(join(here, "../../../packages/ai/src/models.generated.ts")).href);
writeFileSync(join(here, "models.json"), `${JSON.stringify(MODELS)}\n`);
const manifest = JSON.parse(
	readFileSync(join(here, "../../../packages/ai/src/providers/data/.manifest.json"), "utf8"),
);
writeFileSync(join(here, "catalog-meta.json"), `${JSON.stringify({ generatedAt: manifest.generatedAt })}\n`);
console.log(`wrote ${Object.keys(MODELS).length} providers`);
