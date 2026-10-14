// Records `ultron auth check|print-api-key|print-bearer-token` of the TypeScript CLI for a models.json Radius
// provider (`"oauth": "radius"`): no credential, RADIUS_API_KEY, a models.json key, a stored OAuth token, an expired
// one refreshed on the provider's own gateway (a loopback stand-in), a token too short-lived for --min-expiry, and
// --no-refresh. Each case runs `node packages/coding-agent/src/cli.ts auth ...` in a private temp HOME / agent dir
// with only the environment it names (PATH=/usr/bin:/bin, PI_OFFLINE=1), and records stdout, stderr, the exit code,
// the gateway's requests and the credential left in auth.json (expiry normalized). The gateway's port is <port>.
// Usage: node crates/cli/fixtures/radius_auth.mjs
import { spawn } from "node:child_process";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { assertOffline, guardNodeOptions } from "./offline-guard.mjs";

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, "../../..");
const FAR = 4102444800000;

const requests = [];
const server = createServer((req, res) => {
	let body = "";
	req.on("data", (chunk) => {
		body += chunk;
	});
	req.on("end", () => {
		requests.push({ method: req.method, path: req.url, body: Object.fromEntries(new URLSearchParams(body)) });
		res.setHeader("content-type", "application/json");
		res.end(JSON.stringify({ access_token: "refreshed-access", refresh_token: "refreshed-refresh", expires_in: 3600 }));
	});
});
await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
const port = server.address().port;
const gw = (extra = {}) => ({ providers: { gw: { oauth: "radius", baseUrl: `http://127.0.0.1:${port}/v1`, name: "My Radius", ...extra } } });
const oauth = (access, expires) => ({ gw: { type: "oauth", access, refresh: "stored-refresh", expires } });

const cases = [
	{ args: ["auth", "check", "--provider", "gw", "--json"], models: gw() },
	{ args: ["auth", "check", "--provider", "gw", "--json"], models: gw(), env: { RADIUS_API_KEY: "env-key" } },
	{ args: ["auth", "print-api-key", "--provider", "gw"], models: gw(), env: { RADIUS_API_KEY: "env-key" } },
	{ args: ["auth", "print-api-key", "--provider", "gw"], models: gw({ apiKey: "models-key" }) },
	{ args: ["auth", "print-bearer-token", "--provider", "gw"], models: gw(), env: { RADIUS_API_KEY: "env-key" } },
	{ args: ["auth", "check", "--provider", "gw", "--json"], models: gw(), auth: oauth("valid-access", FAR) },
	{ args: ["auth", "check", "--provider", "gw", "--json", "--credentials"], models: gw(), auth: oauth("valid-access", FAR) },
	{ args: ["auth", "print-bearer-token", "--provider", "gw"], models: gw(), auth: oauth("valid-access", FAR) },
	{ args: ["auth", "print-api-key", "--provider", "gw"], models: gw(), auth: oauth("valid-access", FAR) },
	{ args: ["auth", "check", "--provider", "gw", "--json"], models: gw(), auth: oauth("old-access", 1) },
	{ args: ["auth", "print-bearer-token", "--provider", "gw"], models: gw(), auth: oauth("old-access", 1) },
	{ args: ["auth", "check", "--provider", "gw", "--json", "--credentials", "--no-refresh"], models: gw(), auth: oauth("old-access", 1) },
	{ args: ["auth", "print-bearer-token", "--provider", "gw", "--min-expiry", "2h"], models: gw(), auth: oauth("short-access", "+1h") },
	{ args: ["auth", "print-bearer-token", "--provider", "gw", "--min-expiry", "30m"], models: gw(), auth: oauth("short-access", "+1h") },
	{ args: ["auth", "check", "--provider", "nope", "--json"], models: gw() },
];

const out = [];
for (const spec of cases) {
	const tmp = mkdtempSync(join(tmpdir(), "radius-auth-"));
	const agent = join(tmp, "agent");
	mkdirSync(agent, { recursive: true });
	writeFileSync(join(agent, "models.json"), JSON.stringify(spec.models));
	if (spec.auth) {
		const auth = JSON.parse(JSON.stringify(spec.auth));
		if (auth.gw.expires === "+1h") auth.gw.expires = Date.now() + 3_600_000;
		writeFileSync(join(agent, "auth.json"), JSON.stringify(auth));
	}
	requests.length = 0;
	const result = await new Promise((resolve) => {
		// Asynchronous, so this process keeps serving the gateway while the CLI runs.
		{
			const proc = spawn(process.execPath, [join(root, "packages/coding-agent/src/cli.ts"), ...spec.args], {
				cwd: tmp,
				env: {
					PATH: "/usr/bin:/bin",
					HOME: tmp,
					PI_OFFLINE: "1",
					NODE_OPTIONS: guardNodeOptions,
					ULTRON_GUARD_LOG: join(tmp, "guard.log"),
					ULTRON_CODING_AGENT_DIR: agent,
					ULTRON_SERVER_DIR: join(tmp, "server"),
					...(spec.env ?? {}),
				},
			});
			let stdout = "";
			let stderr = "";
			proc.stdout.on("data", (d) => {
				stdout += d;
			});
			proc.stderr.on("data", (d) => {
				stderr += d;
			});
			proc.on("close", (code) => resolve({ stdout, stderr, code }));
		}
	});
	let stored = null;
	try {
		const auth = JSON.parse(readFileSync(join(agent, "auth.json"), "utf8"));
		stored = auth.gw ? { type: auth.gw.type, access: auth.gw.access ?? null, refresh: auth.gw.refresh ?? null } : null;
	} catch {}
	assertOffline(join(tmp, "guard.log"));
	rmSync(tmp, { recursive: true, force: true });
	const unport = (text) => text.split(String(port)).join("<port>");
	out.push({
		args: spec.args,
		models: JSON.parse(unport(JSON.stringify(spec.models))),
		auth: spec.auth ?? null,
		env: spec.env ?? {},
		stdout: unport(result.stdout),
		stderr: unport(result.stderr),
		code: result.code,
		requests: JSON.parse(unport(JSON.stringify(requests))),
		stored,
	});
}
server.close();
writeFileSync(join(here, "radius_auth.json"), `${JSON.stringify({ cases: out }, null, "\t")}\n`);
