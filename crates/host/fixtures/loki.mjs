// Generates crates/host/fixtures/loki.json: the TypeScript host with Loki enabled serving the real runtime.py. The
// session worker's routing (experimental/session-worker.ts: `files.before_write` to FileHooks.beforeWrite, shell
// requests to FileHooks.beforeShell first) over ultron/file-hooks.ts and ultron/loki.ts's LokiGuard, which runs the
// Python engine (loki-engine/loki.py) as the product does today. Each session is a scripted list of cells in a fresh
// temp Git repository; results are normalized (the temp dir) so the Rust host replaying the same cells must match.
// Also records `mayWriteFiles` and `lokiMode` over input tables.
// Usage: node crates/host/fixtures/loki.mjs   Python: $ULTRON_PYTHON, else /usr/bin/python3. Run twice, same sha256.
import { spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, "../../..");
const src = join(root, "packages/coding-agent/src");
const load = (path) => import(pathToFileURL(join(src, path)).href);
const { RlmKernel } = await load("ultron/rlm/kernel.ts");
const { runHostBash } = await load("ultron/rlm/host-bash.ts");
const { ShellJobs } = await load("ultron/rlm/shell-jobs.ts");
const { createLocalBashOperations } = await load("core/tools/bash.ts");
const { BEFORE_WRITE_REQUEST, FileHooks, SHELL_REQUESTS, mayWriteFiles } = await load("ultron/file-hooks.ts");
const { LokiGuard, gitRoot, lokiMode } = await load("ultron/loki.ts");
const python = process.env.ULTRON_PYTHON || "/usr/bin/python3";
const engine = join(src, "ultron/loki-engine/loki.py");

/** Every session's repository holds these files (plus its own); credential-shaped values are built in the cells. */
const base = {
	".gitignore": "build/\n",
	"notes.txt": "hello\n",
	"src/app.c": "int main(void) { return 0; }\n",
};
const KEY = "key = 'AKIA' + 'Q7XRJ2M4PL3ZWNVB'";

const sessions = [
	{
		name: "on",
		mode: "on",
		cells: [
			"await write('clean.txt', 'just text\\n')",
			`${KEY}\nawait write('config.env', f'KEY={key}\\n')`,
			"import os; os.path.exists('config.env')",
			"await write('.loki/loki.json', '{}')",
			"await write('.github/workflows/loki.yml', 'x')",
			"await edit('notes.txt', 'hello', 'token ' + key)",
			"open('notes.txt').read()",
			"await edit('notes.txt', 'hello', 'hello again')",
			"await write('../outside.txt', key)",
			"await write('merge.txt', '<<<<<<< ours\\na\\n=======\\nb\\n>>>>>>> theirs\\n')",
			"await write('src', 'x')",
			"await write('.github/workflows/ci.yml', 'on: pull_request\\njobs:\\n  b:\\n    steps:\\n      - run: echo ${{ github.head_ref }}\\n')",
			"await write('placeholder.env', 'K=AKIA' + 'EXAMPLEXAMPLEXAMPL\\n')",
			"import asyncio; await asyncio.gather(write('x.txt', 'a'), write('y.env', key), return_exceptions=True)",
			"await write('deep/new/dir/file.txt', 'nested\\n')",
			"await bash('echo hi > out.py')",
			"os.path.exists('out.py')",
			"str(await bash('echo hi > notes.md; cat notes.md'))",
			"str(await bash('mkdir -p build && echo x > build/out.js && cat build/out.js'))",
			"await bash('cp notes.txt .loki/x')",
			"await bash('sed -i s/a/b/ src/app.c')",
			"await bash('cat notes.txt | tee copy.cfg')",
			"str(await bash('echo quiet 2>/dev/null'))",
			"str(await bash('ls src > /dev/null && echo listed'))",
			"await bash('echo x > app.ts', yield_after=5)",
			"await rlm.host_request('shell.run', {'command': 'echo x > run.ts'})",
			"await rlm.host_request('bash', {'command': 'echo x >> src/app.c'})",
			"await rlm.host_request('files.before_write', {'writes': []})",
			"await rlm.host_request('files.before_write', {'writes': [{'path': 'a.txt'}]})",
			"await rlm.host_request('files.before_write', {'writes': [{'path': 'a.txt', 'content': 'x'}, {'path': '.loki/a', 'content': 'x'}]})",
		],
	},
	{
		name: "advise",
		mode: "advise",
		cells: [
			`${KEY}\nawait write('config.env', f'KEY={key}\\n')`,
			"open('config.env').read() == f'KEY={key}\\n'",
			"await write('.loki/loki.json', '{}')",
			"str(await bash('echo hi > out.py && cat out.py'))",
			"await write('fine.txt', 'ok')",
		],
	},
	{
		name: "protected-extra",
		mode: "on",
		files: { ".loki/loki.json": JSON.stringify({ protected_extra: ["secrets/**", "*.lock"] }) },
		cells: [
			"await write('secrets/a.txt', 'x')",
			"await write('deps.lock', 'x')",
			"await bash('echo x > secrets/b.txt')",
			"await write('public.txt', 'x')",
		],
	},
	{
		name: "invalid-policy",
		mode: "on",
		files: { ".loki/loki.json": JSON.stringify({ zeta: 1 }) },
		cells: ["await write('a.txt', 'x')", "str(await bash('ls > listing.txt && echo ok'))", "await bash('echo x > a.c')"],
	},
	{
		name: "off",
		mode: "off",
		cells: [`${KEY}\nawait write('config.env', f'KEY={key}\\n')`, "str(await bash('echo hi > out.py && cat out.py'))"],
	},
	{
		name: "subdirectory-cwd",
		mode: "on",
		cwd: "src",
		cells: [
			"await write('lib.txt', 'x')",
			"await write('../.loki/x', 'x')",
			`${KEY}\nawait write('../top.env', key)`,
			"await bash('echo x > here.c')",
			"str(await bash('echo x > here.md && echo ok'))",
		],
	},
];

export function normalize(text, tmp) {
	return text.split(tmp).join("<TMP>");
}

async function runSession(session) {
	const tmp = realpathSync(mkdtempSync(join(tmpdir(), "ultron-loki-fixture-")));
	const repo = join(tmp, "repo");
	const results = [];
	try {
		for (const [path, content] of Object.entries({ ...base, ...(session.files ?? {}) })) {
			mkdirSync(dirname(join(repo, path)), { recursive: true });
			writeFileSync(join(repo, path), content);
		}
		spawnSync("git", ["init", "-q"], { cwd: repo });
		const cwd = session.cwd ? join(repo, session.cwd) : repo;
		const env = { PATH: process.env.PATH, HOME: tmp, GIT_CONFIG_GLOBAL: "/dev/null", GIT_CONFIG_NOSYSTEM: "1" };
		const fileHooks = new FileHooks({ cwd });
		if (session.mode !== "off") {
			fileHooks.add(
				new LokiGuard({
					root: (await gitRoot(cwd)) ?? cwd,
					engine,
					python,
					mode: session.mode,
					timeoutMs: 60_000,
					previewHost: "ultron",
					postWrite: false,
					jsonReports: true,
					env,
				}),
			);
		}
		const jobs = join(tmp, ".jobs");
		let saved;
		const shell = new ShellJobs({
			cwd,
			dir: jobs,
			operations: () => createLocalBashOperations(),
			store: { read: async () => saved, write: async (doc) => { saved = doc; } },
		});
		const caller = { lane: "main" };
		const handler = async (type, payload, signal) => {
			const context = signal ? { abortSignal: signal } : {};
			if (type === "bash" && payload.yield_after !== undefined)
				return shell.module.handle({ type: "shell.bash", payload, caller, context }, {});
			if (type === "bash") return runHostBash(payload, cwd, createLocalBashOperations(), signal);
			if (type.startsWith("shell.")) return shell.module.handle({ type, payload, caller, context }, {});
			throw new Error(`Ultron RLM host request is not available in this host yet: ${type}`);
		};
		// session-worker.ts `checked` and `proposedWrites`.
		const checked = async (type, payload, signal) => {
			if (type === BEFORE_WRITE_REQUEST) {
				const writes = payload.writes;
				if (!Array.isArray(writes) || writes.length === 0 || writes.length > 64)
					throw new Error("files.before_write needs 1 to 64 writes");
				const proposed = writes.map((write) => {
					const path = write?.path;
					const content = write?.content;
					if (typeof path !== "string" || !path || typeof content !== "string")
						throw new Error("files.before_write writes need a path and text content");
					return { path, content };
				});
				return { results: await fileHooks.beforeWrite(proposed, { lane: "main", ...(signal ? { signal } : {}) }) };
			}
			const command = payload?.command;
			if (SHELL_REQUESTS.has(type) && typeof command === "string") {
				const verdict = await fileHooks.beforeShell(command, { lane: "main", ...(signal ? { signal } : {}) });
				if (verdict.blocked) throw new Error(`bash command was not run: ${verdict.reason}`);
			}
			return handler(type, payload, signal);
		};
		const kernel = new RlmKernel(
			{
				cwd,
				runtimePath: join(src, "ultron/rlm/runtime.py"),
				treeMemoryBackend: "off",
				env: { ULTRON_FILE_HOOKS: "1", ULTRON_RLM_SPILL_DIR: "", TMPDIR: tmp, HOME: tmp, GIT_CONFIG_GLOBAL: "/dev/null", GIT_CONFIG_NOSYSTEM: "1" },
			},
			checked,
		);
		try {
			for (const code of session.cells) {
				try {
					const { status, stdout, result: value, error } = await kernel.execute(code);
					const ok = { status, stdout, result: value ?? null, error: error ? `${error.ename}: ${error.evalue}` : null };
					results.push({ code, ok: JSON.parse(normalize(JSON.stringify(ok), tmp)) });
				} catch (error) {
					results.push({ code, error: normalize(error.message, tmp) });
				}
			}
		} finally {
			await kernel.shutdown();
			await shell.close();
		}
	} finally {
		rmSync(tmp, { recursive: true, force: true });
	}
	return { ...session, results };
}

const recorded = [];
for (const session of sessions) recorded.push(await runSession(session));

const mayWriteCommands = [
	"echo hi > a.py", "cat x >> b.txt", "echo a>b", "cmd >| out", "ls | tee out", "sed -i s/a/b/ f",
	"sed -n --in-place s/a/b/ f", "perl -pi -e s/a/b/ f", "cp a b", "(mv a b)", "install -m 644 a b",
	"dd if=/dev/zero of=disk", "echo hi 2>/dev/null > a.c", "ls -la", "npm test 2>&1 | tail -5", "echo hi > /dev/null",
	"cmd 2>>/dev/null", "grep -r foo .", "sed s/a/b/ f", "perl -e 'print 1'", "a <<< b", "x -> y", "cmd &>&1",
	"sed s/a/b/ f | grep -i x", "echo x >/dev/nullx", "echo x 1>&2", "cmd &> log", "a;tee b", "a&&cp b c", "teeth x",
	"xcp a b", "dd of=x", "dd bs=1|of=x", "sed\t-i f", "perl -i f", "perl -ne 'print' f", "echo '>'", "<<EOF > x",
	"cat file|>x", "echo -> x", "ls >&3", "ls >& x", "echo é>/dev/nullé",
];
const mayWrite = mayWriteCommands.map((command) => ({ command, result: mayWriteFiles(command) }));
const modeInputs = [
	[undefined, {}], [undefined, { mode: "advise" }], [undefined, { mode: "off" }], ["", { mode: "off" }],
	["  ", { mode: "advise" }], ["off", {}], ["0", { mode: "on" }], ["FALSE", {}], [" no ", {}], ["none", {}],
	["advise", { mode: "off" }], [" Advise ", {}], ["on", { mode: "off" }], ["yes", { mode: "off" }], ["1", {}],
];
const modes = modeInputs.map(([env, settings]) => ({
	env: env ?? null,
	settings,
	mode: lokiMode(env === undefined ? {} : { ULTRON_LOKI: env }, settings),
}));
writeFileSync(join(here, "loki.json"), `${JSON.stringify({ base, sessions: recorded, mayWrite, modes }, null, "\t")}\n`);
console.log(`recorded ${recorded.length} sessions`);
