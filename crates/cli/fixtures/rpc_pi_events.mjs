// Records the events Pi's RPC mode (`modes/rpc/rpc-mode.ts`) streams around RPC `bash` and `set_session_name`, which
// the native RPC command (`experimental/rpc-native.ts`) does not emit: `bash_execution_update` (a local command's
// output as it streams, under the command's id), `session_info_changed` (after a rename), `extension_error` (an
// extension handler failed), and the `user_bash` hook's effect (an extension answers the command, or the command
// fails closed). The child (rpc_pi_events_child.mjs) runs runRpcMode over the test-suite harness (faux provider) with
// an inline extension equal to crates/extensions/examples/hello for these events.
//
// Recorded per step: the responses, the events other than the deltas, and each command's streamed output joined
// (its chunking is timing). The working directory (the session's: the child's own) and the extension's path are
// placeholders.
// Run behind the offline guard, twice, same sha256: node crates/cli/fixtures/rpc_pi_events.mjs
import "./offline-guard.mjs";
import { spawn } from "node:child_process";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const SYSTEM_PATH = "/usr/local/bin:/usr/bin:/bin";

export const steps = [
	{ command: { id: "b1", type: "bash", command: "printf 'one\\n'; printf 'two\\n' >&2; exit 3" } },
	{ command: { id: "b2", type: "bash", command: "hello world" } },
	{ command: { id: "b3", type: "bash", command: "hello quietly", excludeFromContext: true } },
	{ command: { id: "b4", type: "bash", command: "rm -rf /nowhere" }, wait: ["response", "extension_error"] },
	{ command: { id: "b5", type: "bash", command: "bad-result" }, wait: ["response", "extension_error"] },
	{ command: { type: "bash", command: "printf 'no id\\n'" } },
	{ command: { id: "n1", type: "set_session_name", name: " Renamed " } },
	{ command: { id: "n2", type: "set_session_name", name: "boom" }, wait: ["response", "extension_error"] },
	{ command: { id: "m1", type: "get_messages" } },
];

/** A step is done once every awaited line was seen: "response" is the response to the step's command id. */
function stepDone(step, lines) {
	return (step.wait ?? ["response"]).every((want) =>
		lines.some((line) => (want === "response" ? line.type === "response" && line.id === step.command.id : line.type === want)),
	);
}

/** The comparison's form: `cwd` and extension paths are placeholders; deltas are joined per command id. */
export function canonical(lines, cwd) {
	const scrub = (value) =>
		JSON.parse(JSON.stringify(value), (key, v) => {
			if (key === "timestamp" && typeof v === "number") return 0;
			if (key === "extensionPath" && typeof v === "string") return "<ext>";
			return typeof v === "string" ? v.split(cwd).join("<CWD>") : v;
		});
	const responses = [];
	const events = [];
	const output = {};
	for (const line of scrub(lines)) {
		if (line.type === "response") responses.push(line);
		else if (line.type === "bash_execution_update") {
			const id = line.id ?? "<none>";
			output[id] = (output[id] ?? "") + line.delta;
		} else events.push(line);
	}
	return { responses, events, output };
}

async function run() {
	const home = mkdtempSync(join(tmpdir(), "ultron-rpc-pi-events-"));
	const child = spawn(process.execPath, [new URL("./rpc_pi_events_child.mjs", import.meta.url).pathname], {
		cwd: home,
		env: {
			PATH: SYSTEM_PATH,
			HOME: home,
			ULTRON_CODING_AGENT_DIR: join(home, "agent"),
			ULTRON_SERVER_DIR: join(home, "server"),
			PI_OFFLINE: "1",
			NO_COLOR: "1",
			TZ: "UTC",
			NODE_OPTIONS: `--import ${new URL("./offline-guard.mjs", import.meta.url).href}`,
		},
		stdio: ["pipe", "pipe", "pipe"],
	});
	const lines = [];
	let buffer = "";
	let stderr = "";
	let wake = () => {};
	child.stdout.on("data", (chunk) => {
		buffer += chunk;
		let i;
		while ((i = buffer.indexOf("\n")) !== -1) {
			lines.push(JSON.parse(buffer.slice(0, i)));
			buffer = buffer.slice(i + 1);
		}
		wake();
	});
	child.stderr.on("data", (chunk) => (stderr += chunk));
	const closed = new Promise((r) => child.on("close", r));
	for (const step of steps) {
		const from = lines.length;
		child.stdin.write(`${JSON.stringify(step.command)}\n`);
		const deadline = Date.now() + 60_000;
		while (!stepDone(step, lines.slice(from))) {
			if (Date.now() > deadline) throw new Error(`timed out on ${JSON.stringify(step)}\n${stderr}`);
			await new Promise((r) => {
				wake = r;
				setTimeout(r, 50);
			});
		}
	}
	child.stdin.end();
	const code = await closed;
	rmSync(home, { recursive: true, force: true });
	return { code, ...canonical(lines, home) };
}

if (import.meta.url === `file://${process.argv[1]}`) {
	const result = await run();
	writeFileSync(new URL("./rpc_pi_events.json", import.meta.url), `${JSON.stringify({ steps, ...result }, null, 1)}\n`);
}
