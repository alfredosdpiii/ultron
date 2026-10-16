// Records `ultron mcp list|login|logout` (extensions/mcp/cli.ts) against crates/mcp/fixtures/fake_oauth_server.py: a
// server that needs sign-in, the browser sign-in (a fake `xdg-open` on the run's PATH, the only entry there, follows
// the authorization redirect to the loopback callback), the stored tokens used by `list` and a second `login`, logout
// twice, a sign-in the browser never finishes (`--timeout 1`), an invalid `--timeout`. Every step runs in one profile;
// the server's own log (discovery, registration, authorize, token) is recorded too. offline-guard.mjs is loaded into
// the TypeScript processes; the generator fails if anything tried to leave loopback. crates/cli/tests/mcp_login.rs
// replays it.
// Usage: node crates/cli/fixtures/mcp_login.mjs   (from the repository root; ULTRON_PYTHON or /usr/bin/python3)
import { spawn } from "node:child_process";
import { chmodSync, existsSync, mkdirSync, mkdtempSync, readFileSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createInterface } from "node:readline";
import { assertOffline, guardNodeOptions } from "./offline-guard.mjs";

const root = new URL("../../../", import.meta.url).pathname;
const python = process.env.ULTRON_PYTHON ?? (existsSync("/usr/bin/python3") ? "/usr/bin/python3" : "python3");

/** The fake browser: GET the URL (following redirects) unless FAKE_BROWSER_IGNORE names a file that exists. */
export const FAKE_BROWSER = `#!${python} -I
import os, sys, urllib.request
log = os.path.join(os.path.dirname(os.path.abspath(__file__)), "opened.log")
with open(log, "a") as f:
    f.write("opened\\n")
if os.path.exists(os.path.join(os.path.dirname(os.path.abspath(__file__)), "ignore")):
    sys.exit(0)
urllib.request.urlopen(sys.argv[1]).read()
`;

export const STEPS = [
	{ args: ["mcp", "list"] },
	{ args: ["mcp", "login", "issues", "--timeout", "30"] },
	{ args: ["mcp", "list"] },
	{ args: ["mcp", "list", "--json"] },
	{ args: ["mcp", "login", "issues"] },
	{ args: ["mcp", "logout", "issues"] },
	{ args: ["mcp", "logout", "issues"] },
	{ args: ["mcp", "login", "issues", "--timeout", "0"] },
	{ args: ["mcp", "login", "issues", "--timeout", "1"], ignoreBrowser: true },
];

/** Run-specific values as placeholders: the server origin, the temp dir, the authorization URL's query. */
export function normalizeText(text, origin, tmp) {
	return text
		.split(origin).join("<ORIGIN>")
		.split(tmp).join("<TMP>")
		.replace(/(<ORIGIN>\/authorize)\?\S*/g, "$1?<QUERY>");
}

const base = realpathSync(mkdtempSync(join(tmpdir(), "ultron-mcp-login-")));
const agentDir = join(base, "agent");
const bin = join(base, "bin");
mkdirSync(agentDir, { recursive: true });
mkdirSync(join(base, "cwd"));
mkdirSync(bin);
writeFileSync(join(bin, "xdg-open"), FAKE_BROWSER);
chmodSync(join(bin, "xdg-open"), 0o755);

// `--resources`: the server offers resources but lists none outside MCP Apps, so a signed-in `list` shows zero counts.
const server = spawn(python, ["-I", join(root, "crates/mcp/fixtures/fake_oauth_server.py"), "--resources"], { stdio: ["ignore", "pipe", "inherit"] });
const port = await new Promise((resolve) => createInterface({ input: server.stdout }).once("line", resolve));
const origin = `http://127.0.0.1:${port}`;
writeFileSync(join(agentDir, "mcp.json"), JSON.stringify({ mcpServers: { issues: { url: `${origin}/mcp` } } }));

const out = [];
for (const step of STEPS) {
	if (step.ignoreBrowser) writeFileSync(join(bin, "ignore"), "");
	const result = await new Promise((resolve) => {
		const child = spawn(process.execPath, [join(root, "packages/coding-agent/src/cli.ts"), ...step.args], {
			cwd: join(base, "cwd"),
			env: {
				PATH: bin,
				HOME: base,
				ULTRON_CODING_AGENT_DIR: agentDir,
				ULTRON_SERVER_DIR: join(base, "server"),
				NO_COLOR: "1",
				PI_OFFLINE: "1",
				NODE_OPTIONS: guardNodeOptions,
				ULTRON_GUARD_LOG: join(base, "guard.log"),
			},
			stdio: ["ignore", "pipe", "pipe"],
		});
		let stdout = "";
		let stderr = "";
		child.stdout.on("data", (d) => (stdout += d));
		child.stderr.on("data", (d) => (stderr += d));
		child.on("close", (code) => resolve({ code, stdout, stderr }));
	});
	let opened = 0;
	try {
		opened = readFileSync(join(bin, "opened.log"), "utf8").split("\n").filter(Boolean).length;
	} catch {}
	const auth = existsSync(join(agentDir, "mcp-auth.json")) ? JSON.parse(readFileSync(join(agentDir, "mcp-auth.json"), "utf8")) : null;
	out.push({
		args: step.args,
		ignoreBrowser: step.ignoreBrowser === true,
		code: result.code,
		stdout: normalizeText(result.stdout, origin, base),
		stderr: normalizeText(result.stderr, origin, base),
		browserOpened: opened,
		signedIn: auth !== null && JSON.stringify(auth).includes("access-"),
	});
}
const log = await (await fetch(`${origin}/__log`)).json();
server.kill();
assertOffline(join(base, "guard.log"));
rmSync(base, { recursive: true, force: true });
writeFileSync(new URL("./mcp_login.json", import.meta.url), `${JSON.stringify({ steps: out, serverLog: log.map((l) => normalizeText(l, origin, base)) }, null, 1)}\n`);
