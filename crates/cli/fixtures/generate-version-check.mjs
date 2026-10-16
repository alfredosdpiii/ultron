// Generates crates/cli/fixtures/version-check.json: utils/version-check.ts `checkForNewVersion("1.4.0")` (the
// interactive startup update check) over registry answers and the variables that turn it off (PI_OFFLINE,
// PI_SKIP_VERSION_CHECK, ULTRON_SKIP_VERSION_CHECK). The registry is never contacted: offline-guard.mjs answers the
// `latest` document from stubs (ULTRON_FETCH_STUBS) and logs each request. crates/cli/tests/version_check.rs replays
// the cases against a 127.0.0.1 fake registry.
// Usage: node crates/cli/fixtures/generate-version-check.mjs
import { mkdtempSync, readFileSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const tmp = realpathSync(mkdtempSync(join(tmpdir(), "ultron-version-check-")));
const URL = "https://registry.npmjs.org/ultron-agent/latest";
const CASES = [
	{ env: {}, reply: { status: 200, body: { version: "1.5.0" } } },
	{ env: {}, reply: { status: 200, body: { version: " 1.10.0 " } } },
	{ env: {}, reply: { status: 200, body: { version: "1.4.0" } } },
	{ env: {}, reply: { status: 200, body: { version: "1.3.9" } } },
	{ env: {}, reply: { status: 200, body: { version: "1.5.0-beta.1" } } },
	{ env: {}, reply: { status: 200, body: { version: "not-semver" } } },
	{ env: {}, reply: { status: 200, body: { name: "ultron-agent" } } },
	{ env: {}, reply: { status: 500, body: { error: "down" } } },
	{ env: {}, reply: { status: 404, body: "Not found" } },
	{ env: {}, reply: { status: 200, body: "{not json" } },
	{ env: { PI_OFFLINE: "1" }, reply: { status: 200, body: { version: "9.0.0" } } },
	{ env: { PI_SKIP_VERSION_CHECK: "1" }, reply: { status: 200, body: { version: "9.0.0" } } },
	{ env: { PI_SKIP_VERSION_CHECK: "0" }, reply: { status: 200, body: { version: "9.0.0" } } },
	{ env: { PI_SKIP_VERSION_CHECK: "false" }, reply: { status: 200, body: { version: "9.0.0" } } },
	{ env: { PI_SKIP_VERSION_CHECK: " " }, reply: { status: 200, body: { version: "9.0.0" } } },
	{ env: { ULTRON_SKIP_VERSION_CHECK: "yes" }, reply: { status: 200, body: { version: "9.0.0" } } },
	{ env: { ULTRON_SKIP_VERSION_CHECK: "No" }, reply: { status: 200, body: { version: "9.0.0" } } },
];
const disabled = (env) =>
	Boolean(env.PI_OFFLINE) ||
	[env.PI_SKIP_VERSION_CHECK, env.ULTRON_SKIP_VERSION_CHECK].some((v) => v !== undefined && v.trim() !== "" && !/^(0|false|no)$/i.test(v.trim()));
// One stubbed answer per request, in order (only checks that are on ask).
const stubs = { [`GET ${URL}`]: CASES.filter((c) => !disabled(c.env)).map((c) => c.reply) };
writeFileSync(join(tmp, "stubs.json"), JSON.stringify(stubs));
const log = join(tmp, "fetch.log");
writeFileSync(log, "");
for (const key of Object.keys(process.env)) delete process.env[key];
Object.assign(process.env, { HOME: join(tmp, "home"), PATH: "/usr/bin:/bin", ULTRON_FETCH_STUBS: join(tmp, "stubs.json"), ULTRON_FETCH_LOG: log });
const { assertOffline } = await import("./offline-guard.mjs");
const { checkForNewVersion } = await import(pathToFileURL(join(here, "../../../packages/coding-agent/src/utils/version-check.ts")).href);
const cases = [];
for (const c of CASES) {
	for (const key of ["PI_OFFLINE", "PI_SKIP_VERSION_CHECK", "ULTRON_SKIP_VERSION_CHECK"]) delete process.env[key];
	Object.assign(process.env, c.env);
	writeFileSync(log, "");
	const release = await checkForNewVersion("1.4.0");
	const requests = readFileSync(log, "utf-8").split("\n").filter(Boolean).map((line) => {
		const r = JSON.parse(line);
		return { method: r.method, url: r.url, accept: r.headers.accept ?? null };
	});
	cases.push({ ...c, newer: release?.version ?? null, requests });
}
assertOffline();
rmSync(tmp, { recursive: true, force: true });
writeFileSync(join(here, "version-check.json"), `${JSON.stringify({ current: "1.4.0", cases }, null, 1)}\n`);
console.log(`wrote ${cases.length} cases`);
