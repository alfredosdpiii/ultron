/**
 * Process plumbing shared by the autoreview benchmarks: running a child to completion, a small worker pool, the
 * private profile directories every reviewer and judge process gets, scrubbing of credentials from kept output, and
 * one judge call on the Ultron CLI. Used by `run.mjs` and `blind/run.mjs`.
 */

import { spawn } from "node:child_process";
import { copyFileSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";

/** Everything the benchmarks keep outside the repository: datasets, mirrors, case repositories, evidence. */
export const BENCH_HOME = process.env.ULTRON_AUTOREVIEW_HOME || join(homedir(), ".cache", "ultron-autoreview-bench");
/** Where the reviewer's credentials come from: only `models.json` and `auth.json` are copied, into a temp dir. */
export const USER_PROFILE = process.env.ULTRON_AUTOREVIEW_PROFILE || join(homedir(), ".ultron", "agent");
export const CREDENTIAL_FILES = ["models.json", "auth.json"];
export const JUDGE_TIMEOUT_MS = 5 * 60 * 1000;

/** `{ flags, unknown }` of an argv: `--name value` for value flags, `--name` for switches, anything else unknown. */
export function parseArgs(argv, valueFlags, switchFlags) {
	const flags = {};
	const unknown = [];
	for (let index = 0; index < argv.length; index++) {
		const name = argv[index].replace(/^--/, "");
		if (argv[index].startsWith("--") && valueFlags.includes(name) && index + 1 < argv.length) flags[name] = argv[++index];
		else if (argv[index].startsWith("--") && switchFlags.includes(name)) flags[name] = true;
		else unknown.push(argv[index]);
	}
	return { flags, unknown };
}

export function log(message) {
	console.error(`[${new Date().toISOString().slice(11, 19)}] ${message}`);
}

/** Run a command to completion in its own process group, capturing its output. Never through a shell. */
export function run(command, args, { env, cwd, timeoutMs } = {}) {
	return new Promise((done) => {
		const started = performance.now();
		const child = spawn(command, args, { env, cwd, stdio: ["ignore", "pipe", "pipe"], detached: true });
		const chunks = { stdout: [], stderr: [] };
		let timedOut = false;
		const timer = timeoutMs
			? setTimeout(() => {
					timedOut = true;
					try {
						process.kill(-child.pid, "SIGKILL");
					} catch {
						child.kill("SIGKILL");
					}
				}, timeoutMs)
			: null;
		for (const stream of ["stdout", "stderr"]) child[stream].on("data", (chunk) => chunks[stream].push(chunk));
		const finish = (code, error) => {
			if (timer) clearTimeout(timer);
			done({
				code,
				timedOut,
				wallMs: Math.round(performance.now() - started),
				stdout: Buffer.concat(chunks.stdout).toString("utf8"),
				stderr: error ? String(error.message) : Buffer.concat(chunks.stderr).toString("utf8"),
			});
		};
		child.on("error", (error) => finish(127, error));
		child.on("close", (code) => finish(code ?? 1));
	});
}

export async function pool(jobs, concurrency, worker) {
	const results = new Array(jobs.length);
	let next = 0;
	await Promise.all(
		Array.from({ length: Math.min(concurrency, jobs.length) }, async () => {
			while (next < jobs.length) {
				const index = next++;
				results[index] = await worker(jobs[index], index);
			}
		}),
	);
	return results;
}

export function readJson(path) {
	try {
		return JSON.parse(readFileSync(path, "utf8"));
	} catch {
		return null;
	}
}

/** String values of a JSON document that look like secrets (long, no spaces), for scrubbing evidence. */
function secretsIn(value, out = []) {
	if (typeof value === "string") {
		if (value.length >= 20 && !/\s/.test(value) && !/^https?:\/\//.test(value)) out.push(value);
	} else if (value && typeof value === "object") for (const child of Object.values(value)) secretsIn(child, out);
	return out;
}

export function userSecrets() {
	const secrets = [];
	for (const file of CREDENTIAL_FILES) {
		try {
			secretsIn(JSON.parse(readFileSync(join(USER_PROFILE, file), "utf8")), secrets);
		} catch {}
	}
	return secrets;
}

export function scrub(text, secrets) {
	let out = text;
	for (const secret of secrets) out = out.split(secret).join("[redacted]");
	return out;
}

/**
 * Run `body(env, work)` with a private Ultron server dir and agent dir under a short temp path (socket paths are
 * limited to about 100 characters), deleted afterwards whatever happens. With `credentials`, the agent dir gets
 * copies of the user's `models.json` and `auth.json` and HOME is an empty directory, so the reviewer sees none of
 * the user's settings, skills, extensions or memory; the copies die with the temp dir and never reach the evidence.
 */
export async function withPrivateDirs({ credentials }, body) {
	const work = mkdtempSync(join("/tmp", "u-ar-"));
	try {
		const agentDir = join(work, "a");
		const serverDir = join(work, "s");
		mkdirSync(agentDir);
		mkdirSync(serverDir);
		const env = { ...process.env, ULTRON_SERVER_DIR: serverDir, ULTRON_CODING_AGENT_DIR: agentDir, ULTRON_HINDSIGHT_URL: "off" };
		if (credentials) {
			for (const file of CREDENTIAL_FILES) {
				if (existsSync(join(USER_PROFILE, file))) copyFileSync(join(USER_PROFILE, file), join(agentDir, file));
			}
			const home = join(work, "h");
			mkdirSync(home);
			Object.assign(env, {
				HOME: home,
				USERPROFILE: home,
				XDG_CONFIG_HOME: join(home, ".config"),
				XDG_CACHE_HOME: join(home, ".cache"),
				XDG_DATA_HOME: join(home, ".local", "share"),
				XDG_STATE_HOME: join(home, ".local", "state"),
			});
		}
		return await body(env, work);
	} finally {
		rmSync(work, { recursive: true, force: true });
	}
}

/**
 * One judge call: the Ultron CLI in print mode with no tools, extensions, skills or context files, in private dirs
 * with a copy of the credentials. Returns the child's result (`code`, `timedOut`, `stdout`, `stderr`).
 */
export function askJudge({ ultron, model, thinking = null, system, prompt }) {
	return withPrivateDirs({ credentials: true }, (env, work) =>
		run(
			ultron[0],
			[
				...ultron.slice(1),
				"-p",
				"--model",
				model,
				...(thinking ? ["--thinking", thinking] : []),
				"--no-session",
				"--no-tools",
				"--no-extensions",
				"--no-skills",
				"--no-context-files",
				"--system-prompt",
				system,
				prompt,
			],
			{ env, cwd: work, timeoutMs: JUDGE_TIMEOUT_MS },
		),
	);
}
