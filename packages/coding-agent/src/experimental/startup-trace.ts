import { appendFileSync } from "node:fs";

const TRACE_ENV = "ULTRON_STARTUP_TRACE";

/**
 * Append one startup milestone to the file named by ULTRON_STARTUP_TRACE (all processes share it).
 * Off unless the variable is set; used by scripts/profile-overhead.mjs to break down startup time.
 */
export function traceStartup(label: string): void {
	const path = process.env[TRACE_ENV];
	if (!path) return;
	try {
		appendFileSync(
			path,
			`${JSON.stringify({ at: performance.timeOrigin + performance.now(), origin: performance.timeOrigin, pid: process.pid, label })}\n`,
		);
	} catch {
		// Tracing must never affect startup.
	}
}
