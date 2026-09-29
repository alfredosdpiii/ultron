/**
 * `ultron hook <event>`: Claude Code hook commands for an `ultron mcp` session. Claude Code runs them with the hook
 * input as JSON on stdin; each forwards it to its session's server over the control socket and prints the hook
 * output the server returns (JSON on stdout, exit 0). Failures are quiet: the hook prints nothing and exits 0, so a
 * missing server, Jev or Hindsight never blocks a prompt.
 *
 * - `session-start` (SessionStart): Loki's policy note and its one-time setup note as `additionalContext`.
 * - `user-prompt` (UserPromptSubmit): opens the root turn; Jev's recall gate and Hindsight recall (when both are
 *   configured) and runtime events that arrived since the last rlm call, as `additionalContext`.
 * - `stop` (Stop): closes the root turn; Jev's retention policy keeps or skips the exchange; budgets and the
 *   stuck-loop count start over.
 */
import { connectControl, findServer } from "./control-socket.ts";

export const HOOK_EVENTS = {
	"session-start": "SessionStart",
	"user-prompt": "UserPromptSubmit",
	stop: "Stop",
} as const;
export type HookEvent = keyof typeof HOOK_EVENTS;

/**
 * How long a hook waits for its server: a SessionStart hook can run while Claude Code is still starting the server;
 * later hooks find it running, so a missing server is not waited for long.
 */
const SESSION_START_WAIT_MS = 10_000;
const CONNECT_WAIT_MS = 1_500;
/** Recall can take up to its own 20 s timeout. */
const REQUEST_TIMEOUT_MS = 50_000;

async function readStdin(): Promise<string> {
	if (process.stdin.isTTY) return "";
	const chunks: Buffer[] = [];
	for await (const chunk of process.stdin) chunks.push(typeof chunk === "string" ? Buffer.from(chunk) : chunk);
	return Buffer.concat(chunks).toString("utf8");
}

/** Run one hook; returns the text to print (may be empty). Never throws. */
export async function runHook(
	event: string,
	options: { socket?: string; input?: string; env?: NodeJS.ProcessEnv } = {},
): Promise<string> {
	if (!(event in HOOK_EVENTS)) return "";
	try {
		const raw = options.input ?? (await readStdin());
		const input = raw.trim() ? (JSON.parse(raw) as Record<string, unknown>) : {};
		const env = options.env ?? process.env;
		const claudeSessionId =
			(typeof input.session_id === "string" ? input.session_id : undefined) ?? env.CLAUDE_CODE_SESSION_ID;
		let socket = options.socket;
		const deadline = Date.now() + (event === "session-start" ? SESSION_START_WAIT_MS : CONNECT_WAIT_MS);
		while (socket === undefined) {
			socket = findServer({
				...(claudeSessionId === undefined ? {} : { claudeSessionId }),
				...(typeof input.cwd === "string" ? { cwd: input.cwd } : {}),
			})?.socket;
			if (socket !== undefined || Date.now() >= deadline) break;
			await new Promise((done) => setTimeout(done, 200));
		}
		if (socket === undefined) return "";
		const client = await connectControl(socket, Math.max(0, deadline - Date.now()));
		try {
			const result = await client.request("hook", { event, input }, REQUEST_TIMEOUT_MS);
			return result && typeof result === "object" && Object.keys(result).length > 0 ? JSON.stringify(result) : "";
		} finally {
			client.close();
		}
	} catch {
		return "";
	}
}

/** `ultron hook <event> [--socket path]`. */
export async function runHookCommand(args: readonly string[]): Promise<void> {
	const event = args[0] ?? "";
	let socket: string | undefined;
	for (let index = 1; index < args.length; index += 1) {
		if (args[index] === "--socket" && args[index + 1] !== undefined) socket = args[++index];
		else {
			process.stderr.write(`ultron hook: unknown option ${args[index]}\n`);
			process.exitCode = 2;
			return;
		}
	}
	if (!(event in HOOK_EVENTS)) {
		process.stderr.write(
			`ultron hook: unknown event ${event || "(none)"}; one of ${Object.keys(HOOK_EVENTS).join(", ")}\n`,
		);
		process.exitCode = 2;
		return;
	}
	const output = await runHook(event, socket === undefined ? {} : { socket });
	if (output) process.stdout.write(`${output}\n`);
}
