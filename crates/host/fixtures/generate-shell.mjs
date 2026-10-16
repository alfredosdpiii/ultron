// Generates crates/host/fixtures/shell.json: which shell the TypeScript worker runs commands with (utils/shell.ts
// `getShellConfig`, through `createLocalBashOperations({ shellPath })` as experimental/session-worker.ts builds it for
// the REPL's `bash` and the user's `!`), and what a `shellCommandPrefix` does to a user `!` command
// (experimental/services/session-control-provider.ts `bash`: `${prefix}\n${command}` through
// `executeBashWithOperations`). A fake shell (a script that logs its arguments, then runs /bin/bash with them) stands
// in for a custom `shellPath`. crates/host/tests/shell.rs replays it. Temp paths are written as <dir>.
// Usage: node crates/host/fixtures/generate-shell.mjs
import "../../cli/fixtures/offline-guard.mjs";
import { chmodSync, mkdtempSync, readFileSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const src = join(resolve(here, "../../.."), "packages/coding-agent/src");
const load = (path) => import(pathToFileURL(join(src, path)).href);
const { getShellConfig } = await load("utils/shell.ts");
const { runHostBash } = await load("ultron/rlm/host-bash.ts");
const { createLocalBashOperations } = await load("core/tools/bash.ts");
const { executeBashWithOperations } = await load("core/bash-executor.ts");

const dir = realpathSync(mkdtempSync(join(tmpdir(), "ultron-shell-")));
const rel = (v) => JSON.parse(JSON.stringify(v ?? null).split(dir).join("<dir>"));
const fake = join(dir, "fake-shell");
const log = join(dir, "argv.log");
writeFileSync(fake, `#!/bin/bash\nfor a in "$@"; do printf '[%s]\\n' "$a"; done >> "${log}"\nexec /bin/bash "$@"\n`);
chmodSync(fake, 0o755);
const takeLog = () => {
	let text = "";
	try {
		text = readFileSync(log, "utf-8");
	} catch {}
	writeFileSync(log, "");
	return rel(text);
};
const attempt = async (f) => {
	try {
		return { value: rel(await f()) };
	} catch (error) {
		return { error: rel(error instanceof Error ? error.message : String(error)) };
	}
};

// getShellConfig: a custom path that exists, one that does not, none (the machine default; /bin/bash here).
const configs = [];
for (const custom of [fake, join(dir, "missing-shell"), "", undefined]) {
	configs.push({ custom: rel(custom), result: await attempt(() => getShellConfig(custom)) });
}

// The REPL's blocking bash (runHostBash) under a custom shell, and with a missing one.
const host = [];
for (const [shellPath, command] of [
	[fake, "echo hi; echo err >&2; exit 3"],
	[fake, "printf '%s' \"$0\""],
	[join(dir, "missing-shell"), "echo never"],
]) {
	const result = await attempt(async () => {
		const r = await runHostBash({ command }, dir, createLocalBashOperations({ shellPath }));
		return { output: r.output, exit_code: r.exit_code };
	});
	host.push({ shellPath: rel(shellPath), command, result, argv: takeLog() });
}

// The user's `!` command with a shellCommandPrefix, as session-control-provider.ts composes it.
const user = [];
for (const [prefix, command] of [
	["shopt -s expand_aliases\nalias greet='echo hello from alias'", "greet"],
	["set -e", "false; echo unreachable"],
	["", "echo plain"],
	[undefined, "echo none"],
]) {
	const result = await attempt(async () => {
		const r = await executeBashWithOperations(
			prefix ? `${prefix}\n${command}` : command,
			dir,
			createLocalBashOperations({ shellPath: fake }),
		);
		return { output: r.output, exitCode: r.exitCode };
	});
	user.push({ prefix: prefix ?? null, command, result, argv: takeLog() });
}

rmSync(dir, { recursive: true, force: true });
writeFileSync(join(here, "shell.json"), `${JSON.stringify({ configs, host, user }, null, 1)}\n`);
console.log(`wrote ${configs.length} configs, ${host.length} host runs, ${user.length} user runs`);
