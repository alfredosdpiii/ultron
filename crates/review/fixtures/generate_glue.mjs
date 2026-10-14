// Generates crates/review/fixtures/glue.json from the Python deep pass, structural reference, test session and
// compiled interpreter in this checkout (packages/coding-agent/src/ultron/rlm/autoreview_{deep,reference,tests,
// compiled}.py) working together, by running fixtures/record_glue.py over a git repository it builds in a temp dir.
// tests/glue.rs replays every recorded output exactly.
// Usage: node crates/review/fixtures/generate_glue.mjs
import { execFileSync } from "node:child_process";
import { writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, "../../..");
const python = process.env.ULTRON_PYTHON || "/usr/bin/python3";
const rlm = join(root, "packages/coding-agent/src/ultron/rlm");
const out = execFileSync(python, ["-I", join(here, "record_glue.py"), rlm], {
	encoding: "utf8",
	maxBuffer: 64 << 20,
	env: { ...process.env, PYTHONDONTWRITEBYTECODE: "1", PYTHONHASHSEED: "0" },
});
writeFileSync(join(here, "glue.json"), out);
