// The child process of rpc_pi_events.mjs: Pi's RPC mode (`modes/rpc/rpc-mode.ts` `runRpcMode`) over the test-suite
// harness (faux provider, in-memory session) with one inline extension that does what the example extension
// crates/extensions/examples/hello does for these events:
// - `user_bash`: a command starting with "hello" is answered by the extension (`{ result }`, never run); one with
//   "rm -rf" fails closed (the handler throws); "bad-result" returns an invalid result; anything else runs locally;
// - `session_info_changed`: the name "boom" makes the handler throw.
// The in-memory session's working directory is the process's.
import { createHarness } from "../../../packages/coding-agent/test/suite/harness.ts";
import { runRpcMode } from "../../../packages/coding-agent/src/modes/rpc/rpc-mode.ts";

const hello = (pi) => {
	pi.on("user_bash", async (event) => {
		if (event.command.includes("rm -rf")) throw new Error("hello refuses rm -rf");
		if (event.command === "bad-result") return {};
		if (!event.command.startsWith("hello")) return undefined;
		const how = event.excludeFromContext ? "excluded" : "included";
		return {
			result: {
				output: `hello ran ${JSON.stringify(event.command)} (${how}) in ${event.cwd}\n`,
				exitCode: 0,
				cancelled: false,
				truncated: false,
			},
		};
	});
	pi.on("session_info_changed", async (event) => {
		if (event.name === "boom") throw new Error("hello cannot be named boom");
	});
};

const harness = await createHarness({ extensionFactories: [hello] });
const runtime = {
	session: harness.session,
	newSession: async () => ({ cancelled: true }),
	switchSession: async () => ({ cancelled: true }),
	fork: async () => ({ cancelled: true, selectedText: "" }),
	dispose: async () => harness.cleanup(),
	setRebindSession: () => {},
};
await runRpcMode(runtime);
