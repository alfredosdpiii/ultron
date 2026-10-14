// Generates crates/review/fixtures/compiled.json from the Python compiled mode in this checkout
// (packages/coding-agent/src/ultron/rlm/autoreview_compiled.py), by running fixtures/record_compiled.py: the program
// language and validation, coverage and catalogue shapes, result reading, the interpreter's checks over recorded
// results, for_each expansion, candidate shapes and template programs, and the planner views.
// tests/compiled.rs replays every recorded output exactly.
// Usage: node crates/review/fixtures/generate_compiled.mjs
import { execFileSync } from "node:child_process";
import { writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, "../../..");
const python = process.env.ULTRON_PYTHON || "/usr/bin/python3";
const rlm = join(root, "packages/coding-agent/src/ultron/rlm");
const out = execFileSync(python, ["-I", join(here, "record_compiled.py"), rlm], {
	encoding: "utf8",
	maxBuffer: 64 << 20,
	env: { ...process.env, PYTHONDONTWRITEBYTECODE: "1", PYTHONHASHSEED: "0" },
});
writeFileSync(join(here, "compiled.json"), out);
