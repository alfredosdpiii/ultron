/**
 * The host side of the kernel's `bash` skill: one shell command in the working directory, run through Pi's
 * bash executor (same shell, sanitizing and output spill as Pi's native bash tool), with an optional timeout
 * that the skill reports as `timed_out` rather than as a plain cancellation.
 */
import { executeBashWithOperations } from "../../core/bash-executor.ts";
import type { BashOperations } from "../../core/tools/bash.ts";

/** The `bash` host reply the kernel's skill turns into a BashOutput. */
export interface HostBashResult {
	output: string;
	exit_code: number | null;
	cancelled: boolean;
	timed_out: boolean;
	truncated: boolean;
	full_output_path: string | null;
}

/** At most a day: the kernel's own cell limits end anything longer. */
const MAX_TIMEOUT_SECONDS = 86_400;

export function bashTimeoutSeconds(value: unknown): number | undefined {
	if (value === undefined || value === null) return undefined;
	if (typeof value !== "number" || !Number.isFinite(value) || value <= 0)
		throw new Error("bash timeout must be a positive number of seconds");
	return Math.min(value, MAX_TIMEOUT_SECONDS);
}

export async function runHostBash(
	payload: Record<string, unknown>,
	cwd: string,
	operations: BashOperations,
	signal?: AbortSignal,
): Promise<HostBashResult> {
	const command = payload.command;
	if (typeof command !== "string" || !command.trim()) throw new Error("bash command must be a non-empty string");
	const timeout = bashTimeoutSeconds(payload.timeout);
	const controller = new AbortController();
	let timedOut = false;
	const forward = () => controller.abort();
	if (signal?.aborted) controller.abort();
	else signal?.addEventListener("abort", forward, { once: true });
	const timer =
		timeout === undefined
			? undefined
			: setTimeout(() => {
					timedOut = true;
					controller.abort();
				}, timeout * 1000);
	try {
		const result = await executeBashWithOperations(command, cwd, operations, { signal: controller.signal });
		return {
			output: result.output,
			exit_code: result.exitCode ?? null,
			cancelled: result.cancelled && !timedOut,
			timed_out: timedOut,
			truncated: result.truncated,
			full_output_path: result.fullOutputPath ?? null,
		};
	} finally {
		if (timer !== undefined) clearTimeout(timer);
		signal?.removeEventListener("abort", forward);
	}
}
