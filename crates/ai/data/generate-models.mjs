// Generates crates/ai/data/models.json, the built-in model catalog, from packages/ai/src/models.generated.ts.
// Shape: { provider: { modelId: Model } } in the generated catalog's order.
// Usage: node crates/ai/data/generate-models.mjs
import { writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const { MODELS } = await import(pathToFileURL(join(here, "../../../packages/ai/src/models.generated.ts")).href);
writeFileSync(join(here, "models.json"), `${JSON.stringify(MODELS)}\n`);
console.log(`wrote ${Object.keys(MODELS).length} providers`);
