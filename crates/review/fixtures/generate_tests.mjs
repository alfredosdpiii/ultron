// Generates crates/review/fixtures/tests.json from the Python implementation in this checkout
// (packages/coding-agent/src/ultron/rlm/autoreview_tests.py), by running fixtures/record_tests.py: sandbox command
// lines, environments, detection, self-check, doctor and the command line, environment discovery over trees built
// in a temp dir, runner detection, outcomes, test sessions with a scripted executor, summaries, real processes and
// a real `git archive` export. The Rust tests (tests/review_tests.rs) compare every recorded output exactly.
// Usage: node crates/review/fixtures/generate_tests.mjs
import { execFileSync } from "node:child_process";
import { writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, "../../..");
const python = process.env.ULTRON_PYTHON || "/usr/bin/python3";
const rlm = join(root, "packages/coding-agent/src/ultron/rlm");
const out = execFileSync(python, ["-I", join(here, "record_tests.py"), rlm], { encoding: "utf8", maxBuffer: 64 << 20 });
writeFileSync(join(here, "tests.json"), out);
