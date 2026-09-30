import { RemoteServiceError } from "@ultron/chord";
import { DisconnectedError } from "@ultron/client";

/** How to get back to a Session after its server or worker went away: the Session file itself is durable. */
export const RESUME_HINT = "your session is saved — run `ultron -c` to resume";

/** Low-level signs that the server connection or the Session worker behind it is gone. */
const LOST_SERVER_PATTERNS: readonly RegExp[] = [
	/binding is closed/i,
	/byte transport closed/i,
	/unix connection is closed/i,
	/session worker .*(?:disconnected|exited) unexpectedly/i,
	/session worker disconnected during/i,
	/lost its coordinator/i,
	/experimental server (?:was replaced|is shutting down|detached)/i,
	/\bECONNRESET\b|\bEPIPE\b|\bECONNREFUSED\b/,
];

function rawMessage(error: unknown): string {
	return error instanceof Error ? error.message : String(error);
}

/** True when `error` means the server connection or the Session worker was lost, not that a request failed. */
export function isLostServerError(error: unknown): boolean {
	if (error instanceof DisconnectedError) return true;
	if (error instanceof RemoteServiceError && error.code === "service_stale_instance") return true;
	const text = rawMessage(error);
	return LOST_SERVER_PATTERNS.some((pattern) => pattern.test(text));
}

/** The user-facing explanation for a lost server, with the low-level reason when there is one. */
export function lostServerMessage(reason?: string): string {
	const detail = reason === undefined || reason.trim().length === 0 ? "" : ` (${reason.trim()})`;
	return `The Ultron server stopped${detail}; ${RESUME_HINT}`;
}

/** A clear message for a lost server or worker; undefined for any other error. */
export function describeLostServer(error: unknown): string | undefined {
	if (!isLostServerError(error)) return undefined;
	const text = rawMessage(error);
	return text.includes(RESUME_HINT) ? text : lostServerMessage(text);
}

/** `error`'s message, replaced by the clear lost-server message when the server or worker went away. */
export function clientErrorMessage(error: unknown): string {
	return describeLostServer(error) ?? rawMessage(error);
}

/** A status line's `Error: …` text, made clear when it reports a lost server or worker. */
export function friendlyStatus(text: string): string {
	if (text.includes(RESUME_HINT)) return text;
	const match = /^(Error: |Reconnect error: )([\s\S]*)$/.exec(text);
	if (match === null || !isLostServerError(match[2])) return text;
	return `Error: ${lostServerMessage(match[2])}`;
}
