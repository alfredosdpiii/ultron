// Generates crates/cli/fixtures/package-dir.json: config.ts `getPackageDir()` under PI_PACKAGE_DIR (`normalizePath`:
// `~` expanded) and the paths read from it (`getDocsPath`, `getChangelogPath`, `getRlmRuntimePath`) over a temp
// package directory with src/ultron/rlm/runtime.py, and the missing-runtime error. HOME is a temp dir (<home>).
// crates/cli/tests/package_dir.rs replays it.
// Usage: node crates/cli/fixtures/generate-package-dir.mjs
import "./offline-guard.mjs";
import { mkdirSync, mkdtempSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const tmp = realpathSync(mkdtempSync(join(tmpdir(), "ultron-package-dir-")));
const home = join(tmp, "home");
const pkg = join(home, "pkg");
mkdirSync(join(pkg, "src/ultron/rlm"), { recursive: true });
writeFileSync(join(pkg, "src/ultron/rlm/runtime.py"), "");
mkdirSync(join(home, "empty"), { recursive: true });
process.env.HOME = home;
const config = await import(pathToFileURL(join(here, "../../../packages/coding-agent/src/config.ts")).href);
const rel = (v) => v.split(home).join("<home>");
const cases = [];
for (const value of ["~/pkg", join(home, "pkg"), join(home, "pkg/../pkg"), "~/empty"]) {
	process.env.PI_PACKAGE_DIR = value;
	let runtime;
	try {
		runtime = { value: rel(config.getRlmRuntimePath()) };
	} catch (error) {
		runtime = { error: error.message };
	}
	cases.push({
		value: rel(value),
		packageDir: rel(config.getPackageDir()),
		docs: rel(config.getDocsPath()),
		changelog: rel(config.getChangelogPath()),
		runtime,
	});
}
rmSync(tmp, { recursive: true, force: true });
writeFileSync(join(here, "package-dir.json"), `${JSON.stringify({ cases }, null, 1)}\n`);
console.log(`wrote ${cases.length} cases`);
