// Records `ultron auth check` of the TypeScript CLI for the `claude-code` provider (claudeCodeCliAuth: configured
// when a `claude` executable exists, `ULTRON_CLAUDE_CODE_BIN` first, else PATH; its credential is none). Each case
// runs `node packages/coding-agent/src/cli.ts auth check ...` in a private temp HOME / agent dir whose PATH is
// `<tmp>/bin:/usr/bin:/bin` (a fake `claude` placed in `<tmp>/bin` only when the case says so) and nothing else of
// the environment (PI_OFFLINE=1). The fake `claude` never runs: if executed it writes `<tmp>/ran` and exits 1, and
// the generator fails when that file exists. offline-guard.mjs is loaded into the process: no socket leaves loopback.
// Recorded: the case, stdout, stderr and exit code.
// Usage: node crates/cli/fixtures/claude_code_auth.mjs   (deterministic: run twice, same sha256)
import { spawn } from "node:child_process";
import { chmodSync, existsSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { assertOffline, guardNodeOptions } from "./offline-guard.mjs";

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, "../../..");

// cli: "path" puts bin/claude on PATH; "configured" sets ULTRON_CLAUDE_CODE_BIN to <tmp>/tools/claude (created);
// "configured-missing" sets it to a path that does not exist (with bin/claude on PATH too); "none" has no claude.
const cases = [
	{ args: ["auth", "check", "--provider", "claude-code"], cli: "path" },
	{ args: ["auth", "check", "--provider", "claude-code", "--json"], cli: "path" },
	{ args: ["auth", "check", "--provider", "claude-code", "--json", "--no-refresh"], cli: "path" },
	{ args: ["auth", "check", "--provider", "claude-code", "--json", "--credentials"], cli: "path" },
	{ args: ["auth", "check", "--provider", "claude-code"], cli: "none" },
	{ args: ["auth", "check", "--provider", "claude-code", "--json"], cli: "none" },
	{ args: ["auth", "check", "--provider", "claude-code", "--json", "--credentials"], cli: "none" },
	{ args: ["auth", "check", "--provider", "claude-code", "--json"], cli: "configured" },
	{ args: ["auth", "check", "--provider", "claude-code", "--json"], cli: "configured-missing" },
	{ args: ["auth", "check", "--model", "claude-code/sonnet", "--json"], cli: "path" },
	{ args: ["auth", "check", "--model", "claude-code/claude-opus-5-5", "--json"], cli: "none" },
];

const FAKE = (tmp) => `#!/bin/sh\necho ran > '${join(tmp, "ran")}'\nexit 1\n`;

const out = [];
for (const spec of cases) {
	const tmp = mkdtempSync(join(tmpdir(), "claude-code-auth-"));
	const agent = join(tmp, "agent");
	mkdirSync(agent, { recursive: true });
	const bin = join(tmp, "bin");
	mkdirSync(bin);
	const env = {};
	const place = (dir) => {
		mkdirSync(dir, { recursive: true });
		const file = join(dir, "claude");
		writeFileSync(file, FAKE(tmp));
		chmodSync(file, 0o755);
		return file;
	};
	if (spec.cli === "path" || spec.cli === "configured-missing") place(bin);
	if (spec.cli === "configured") env.ULTRON_CLAUDE_CODE_BIN = place(join(tmp, "tools"));
	if (spec.cli === "configured-missing") env.ULTRON_CLAUDE_CODE_BIN = join(tmp, "tools", "claude");
	const result = await new Promise((resolve) => {
		const proc = spawn(process.execPath, [join(root, "packages/coding-agent/src/cli.ts"), ...spec.args], {
			cwd: tmp,
			env: {
				PATH: `${bin}:/usr/bin:/bin`,
				HOME: tmp,
				PI_OFFLINE: "1",
				NODE_OPTIONS: guardNodeOptions,
				ULTRON_GUARD_LOG: join(tmp, "guard.log"),
				ULTRON_CODING_AGENT_DIR: agent,
				ULTRON_SERVER_DIR: join(tmp, "server"),
				...env,
			},
			stdio: ["ignore", "pipe", "pipe"],
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
	});
	assertOffline(join(tmp, "guard.log"));
	if (existsSync(join(tmp, "ran"))) throw new Error(`the fake claude ran for ${spec.args.join(" ")}`);
	rmSync(tmp, { recursive: true, force: true });
	out.push({ args: spec.args, cli: spec.cli, stdout: result.stdout, stderr: result.stderr, code: result.code });
}
writeFileSync(join(here, "claude_code_auth.json"), `${JSON.stringify({ cases: out }, null, "\t")}\n`);
