// Generates crates/session-file/fixtures/diff.json: jsdiff's `diffLines` and the TypeScript edit tool's
// `generateDiffString` (packages/coding-agent/src/core/tools/edit-diff.ts, this checkout) over fixed text pairs,
// with context 4 (the tool's default) and 2 (the rlm cell's compact view). tests/diff.rs replays every case.
// Usage: node crates/session-file/fixtures/generate-diff.mjs   (deterministic; output is written next to this file)
import { assertOffline } from "../../cli/fixtures/offline-guard.mjs";
import { writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const { generateDiffString } = await import(join(here, "../../../packages/coding-agent/src/core/tools/edit-diff.ts"));
const Diff = await import(join(here, "../../../node_modules/diff/libesm/index.js"));

const numbered = (n, from = 1) => Array.from({ length: n }, (_, i) => `line ${i + from}`).join("\n");
const long = numbered(30);
const pairs = [
	["", ""],
	["", "new file\nsecond line\n"],
	["only line", ""],
	["a\nb\nc\n", "a\nb\nc\n"],
	["a\nb\nc", "a\nB\nc"],
	["a\nb\nc\n", "a\nb\nc"],
	["a\r\nb\r\n", "a\r\nc\r\n"],
	["\n\n\n", "\n\nx\n"],
	["const x = 1;\nconst y = 2;\n", "const x = 10;\nconst y = 2;\n"],
	["\tindented\tline\n", "\tindented\tLINE\n"],
	[long, long.replace("line 15", "line fifteen")],
	[long, long.replace("line 3", "three").replace("line 27", "twenty-seven")],
	[long, long.replace("line 5", "five").replace("line 8", "eight")],
	[long, `${long}\nline 31\nline 32`],
	[long, long.replace("line 1\n", "")],
	[numbered(120), numbered(120).replace("line 60", "sixty").replace("line 100", "hundred")],
	["fn main() {\n    println!(\"hi\");\n}\n", "fn main() {\n    let name = \"x\";\n    println!(\"hi {name}\");\n}\n"],
	["alpha beta gamma\ndelta\n", "delta\nalpha beta gamma\n"],
	["héllo wörld\n日本語\n", "héllo welt\n日本語\nemoji 🎉\n"],
	["x\n".repeat(12), `${"x\n".repeat(6)}y\n${"x\n".repeat(6)}`],
];

const cases = pairs.map(([old, now]) => ({
	old,
	new: now,
	lines: Diff.diffLines(old, now).map((c) => ({ value: c.value, added: !!c.added, removed: !!c.removed })),
	context4: generateDiffString(old, now),
	context2: generateDiffString(old, now, 2),
}));

assertOffline();
writeFileSync(join(here, "diff.json"), `${JSON.stringify({ cases }, null, "\t")}\n`);
