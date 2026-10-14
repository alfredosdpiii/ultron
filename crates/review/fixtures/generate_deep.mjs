// Generates crates/review/fixtures/deep.json from the Python deep pass in this checkout
// (packages/coding-agent/src/ultron/rlm/autoreview_deep.py), by running fixtures/record_deep.py: the read-only
// repository over local git repositories it builds in a temp dir, request serving, the map, citation checks,
// merging, shapes, regression findings, shared views, the tests block, proofs and the assurance.
// The Rust tests (tests/deep.rs) rebuild the same repositories and compare every recorded output exactly.
// Usage: node crates/review/fixtures/generate_deep.mjs
import { execFileSync } from "node:child_process";
import { writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, "../../..");
const python = process.env.ULTRON_PYTHON || "/usr/bin/python3";
const rlm = join(root, "packages/coding-agent/src/ultron/rlm");
const out = execFileSync(python, ["-I", join(here, "record_deep.py"), rlm], {
	encoding: "utf8",
	maxBuffer: 256 << 20,
	env: { ...process.env, PYTHONDONTWRITEBYTECODE: "1" },
});
writeFileSync(join(here, "deep.json"), out);
