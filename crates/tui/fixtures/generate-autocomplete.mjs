// Generates crates/tui/fixtures/autocomplete-parity.json from packages/tui/src/autocomplete.ts (this checkout):
// CombinedAutocompleteProvider slash-command suggestions (names, argument hints, skill: prefixes, argument
// completions) and applyCompletion over slash, @-attachment, argument and plain path cases.
// Usage: node crates/tui/fixtures/generate-autocomplete.mjs   (deterministic; output is written next to this file)
import { writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, "../../..");
const { CombinedAutocompleteProvider } = await import(join(root, "packages/tui/src/autocomplete.ts"));

const argValues = ["alpha", "beta", "gamma"];
const commands = [
	{ name: "model", description: "Switch model", argumentHint: "<name>",
		getArgumentCompletions: (p) => argValues.filter((v) => v.startsWith(p)).map((v) => ({ value: v, label: v })) },
	{ name: "settings", description: "Open settings" },
	{ name: "skill:review", description: "Review code" },
	{ name: "session", argumentHint: "[id]" },
	{ value: "quit", label: "quit", description: "Exit" },
	{ name: "export" },
];
const provider = new CombinedAutocompleteProvider(commands, "/nonexistent-base", null);
const signal = new AbortController().signal;

const slashInputs = ["/", "/m", "/mod", "/s", "/se", "/sk", "/skill:", "/rev", "/q", "/xyz", "/model ", "/model a", "/model z",
	"/settings ", "/unknown arg", "/ex", "/ssn", "/model b extra"];
const suggestions = [];
for (const text of slashInputs) {
	suggestions.push({ text, result: await provider.getSuggestions([text], 0, text.length, { signal }) });
}

const applyCases = [
	{ line: "/mo", col: 3, item: { value: "model", label: "model" }, prefix: "/mo" },
	{ line: "  /mo tail", col: 5, item: { value: "model", label: "model" }, prefix: "/mo" },
	{ line: "/model al", col: 9, item: { value: "alpha", label: "alpha" }, prefix: "al" },
	{ line: "see @src", col: 8, item: { value: "@src/", label: "src/" }, prefix: "@src" },
	{ line: "see @ma rest", col: 7, item: { value: "@main.ts", label: "main.ts" }, prefix: "@ma" },
	{ line: 'x @"my d"', col: 8, item: { value: '@"my dir/"', label: "my dir/" }, prefix: '@"my d' },
	{ line: 'x @"my f" y', col: 8, item: { value: '@"my file.ts"', label: "my file.ts" }, prefix: '@"my f' },
	{ line: 'x @"my f" y', col: 8, item: { value: '@"my file.ts', label: "my file.ts" }, prefix: '@"my f' },
	{ line: "open ./sr", col: 9, item: { value: "./src/", label: "src/" }, prefix: "./sr" },
	{ line: 'open "./a b', col: 11, item: { value: '"./a b/"', label: "a b/" }, prefix: '"./a b' },
	{ line: "/a/b", col: 4, item: { value: "/a/bin/", label: "bin/" }, prefix: "/a/b" },
	{ line: "日本 /mo", col: 6, item: { value: "model", label: "model" }, prefix: "/mo" },
];
const applied = applyCases.map((c) => ({ ...c, result: provider.applyCompletion(["before", c.line, "after"], 1, c.col, c.item, c.prefix) }));

writeFileSync(join(here, "autocomplete-parity.json"), `${JSON.stringify({ suggestions, applied })}\n`);
console.log(`suggestions=${suggestions.length} applied=${applied.length}`);
