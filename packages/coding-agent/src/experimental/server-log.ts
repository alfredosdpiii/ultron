import { appendFileSync, renameSync, statSync } from "node:fs";
import { join } from "node:path";
import { inspect } from "node:util";

/** A log past this size is rotated to `<log>.1` before the next write, so it stays bounded. */
const MAX_SERVER_LOG_BYTES = 1024 * 1024;

/**
 * Where a server records the internal errors its clients see only as "Internal server error". The protocol keeps
 * unexpected errors opaque, and a server's stderr is either discarded (automatically activated) or a terminal UI
 * (foreground), so this file is where the real error can be read.
 */
export function serverErrorLogPath(directory: string, serverId: string): string {
	return join(directory, `server-${serverId}.log`);
}

/** Append an internal error to the server log (and stderr under ULTRON_DEBUG_INTERNAL=1). Never throws. */
export function recordServerError(logPath: string, label: string, error: unknown): void {
	if (process.env.ULTRON_DEBUG_INTERNAL === "1") console.error(label, error);
	try {
		try {
			if (statSync(logPath).size > MAX_SERVER_LOG_BYTES) renameSync(logPath, `${logPath}.1`);
		} catch {
			// No log yet.
		}
		appendFileSync(logPath, `${new Date().toISOString()} ${label} ${inspect(error, { depth: 8 })}\n`, {
			mode: 0o600,
		});
	} catch {
		// Error reporting cannot affect the server.
	}
}
