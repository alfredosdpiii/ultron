/**
 * How `ultron autoreview` runs `gh` and `git`: one injected function, so tests replace GitHub with a fake and
 * assert on every call. A token reaches a child process only through `env`; it is never part of `argv`.
 */
import { spawn } from "node:child_process";

export interface RunOptions {
	readonly cwd?: string;
	/** Added to the process environment of the child. */
	readonly env?: Readonly<Record<string, string>>;
	/** Written to the child's stdin. */
	readonly input?: string;
	readonly timeoutMs?: number;
}

export interface RunResult {
	readonly code: number;
	readonly stdout: string;
	readonly stderr: string;
}

export type Runner = (argv: readonly string[], options?: RunOptions) => Promise<RunResult>;

const DEFAULT_TIMEOUT_MS = 120_000;
const MAX_OUTPUT_BYTES = 64 * 1024 * 1024;

/** Run a process to completion; a missing binary is exit code 127, a timeout 124. Never throws. */
export const runProcess: Runner = (argv, options = {}) =>
	new Promise<RunResult>((resolve) => {
		const [command, ...args] = argv;
		if (command === undefined) {
			resolve({ code: 127, stdout: "", stderr: "empty command" });
			return;
		}
		const child = spawn(command, args, {
			...(options.cwd === undefined ? {} : { cwd: options.cwd }),
			env: {
				...process.env,
				GIT_PAGER: "cat",
				PAGER: "cat",
				GIT_TERMINAL_PROMPT: "0",
				GH_PROMPT_DISABLED: "1",
				GH_NO_UPDATE_NOTIFIER: "1",
				NO_COLOR: "1",
				...options.env,
			},
			stdio: ["pipe", "pipe", "pipe"],
		});
		const stdout: Buffer[] = [];
		const stderr: Buffer[] = [];
		let size = 0;
		let timedOut = false;
		const timeoutMs = options.timeoutMs ?? DEFAULT_TIMEOUT_MS;
		const timer = setTimeout(() => {
			timedOut = true;
			child.kill("SIGKILL");
		}, timeoutMs);
		const collect = (into: Buffer[]) => (chunk: Buffer) => {
			size += chunk.length;
			if (size <= MAX_OUTPUT_BYTES) into.push(chunk);
			else child.kill("SIGKILL");
		};
		child.stdout.on("data", collect(stdout));
		child.stderr.on("data", collect(stderr));
		child.stdin.on("error", () => {});
		child.stdin.end(options.input ?? "");
		child.on("error", (error: NodeJS.ErrnoException) => {
			clearTimeout(timer);
			resolve({ code: error.code === "ENOENT" ? 127 : 1, stdout: "", stderr: `${command}: ${error.message}` });
		});
		child.on("close", (code) => {
			clearTimeout(timer);
			resolve({
				code: timedOut ? 124 : (code ?? 1),
				stdout: Buffer.concat(stdout).toString("utf8"),
				stderr: timedOut
					? `${command}: timed out after ${Math.round(timeoutMs / 1000)}s`
					: Buffer.concat(stderr).toString("utf8"),
			});
		});
	});
