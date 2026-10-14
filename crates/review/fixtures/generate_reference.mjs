// Generates crates/review/fixtures/reference.json from autoreview_reference.py in this checkout, by running
// fixtures/record_reference.py (the TS scanner, the Python indexer, the YAML and Terraform readers, the module map, and
// whole references over git repositories it builds in a temp dir from recorded steps). tests/reference.rs replays it.
// Usage: node crates/review/fixtures/generate_reference.mjs
import { execFileSync } from "node:child_process";
import { writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, "../../..");
const python = process.env.ULTRON_PYTHON || "/usr/bin/python3";
const rlm = join(root, "packages/coding-agent/src/ultron/rlm");
const out = execFileSync(python, ["-I", join(here, "record_reference.py"), rlm], {
	encoding: "utf8",
	maxBuffer: 64 << 20,
	env: { ...process.env, PYTHONDONTWRITEBYTECODE: "1" },
});
writeFileSync(join(here, "reference.json"), out);
