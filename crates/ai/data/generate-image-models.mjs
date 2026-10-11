// Generates crates/ai/data/image-models.json, the built-in image model catalog, from packages/ai/src/image-models.generated.ts.
// Shape: { provider: { modelId: ImagesModel } } in the generated catalog's order.
// Usage: node crates/ai/data/generate-image-models.mjs
import { writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const { IMAGE_MODELS } = await import(
	pathToFileURL(join(here, "../../../packages/ai/src/image-models.generated.ts")).href
);
writeFileSync(join(here, "image-models.json"), `${JSON.stringify(IMAGE_MODELS)}\n`);
console.log(`wrote ${Object.keys(IMAGE_MODELS).length} providers`);
