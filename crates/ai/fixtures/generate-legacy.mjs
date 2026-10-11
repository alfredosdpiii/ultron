// Generates crates/ai/fixtures/legacy.json: every export of packages/ai/src/legacy-api-aliases.ts with the API whose
// streams it aliases and whether it is the simple variant. The module is imported for its export list; the alias
// targets are read from its `const xStreams = xApi()` / `export const name = xStreams.stream` lines and the lazy
// wrappers' import paths. Usage: node crates/ai/fixtures/generate-legacy.mjs
import { readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const srcDir = join(here, "../../../packages/ai/src");
const file = join(srcDir, "legacy-api-aliases.ts");
const exports = Object.keys(await import(pathToFileURL(file).href)).sort();
const text = readFileSync(file, "utf8");
const lazyPath = {};
for (const [, fn, path] of text.matchAll(/import \{ (\w+) \} from "\.\/api\/([\w-]+)\.lazy\.ts";/g)) lazyPath[fn] = path;
const streams = {};
for (const [, name, fn] of text.matchAll(/const (\w+) = (\w+)\(\);/g)) streams[name] = lazyPath[fn];
const aliases = {};
for (const [, name, holder, kind] of text.matchAll(/export const (\w+) = (\w+)\.(stream|streamSimple) as/g)) {
	aliases[name] = { api: streams[holder], simple: kind === "streamSimple" };
}
const out = exports.map((name) => ({ name, ...aliases[name] }));
writeFileSync(join(here, "legacy.json"), `${JSON.stringify(out, null, "\t")}\n`);
