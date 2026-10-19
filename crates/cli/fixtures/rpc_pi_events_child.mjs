// The child process of rpc_pi_events.mjs: Pi's RPC mode (`modes/rpc/rpc-mode.ts` `runRpcMode`) over the test-suite
// harness (faux provider, in-memory session) without extensions. The in-memory session's working directory is the
// process's.
import { createHarness } from "../../../packages/coding-agent/test/suite/harness.ts";
import { runRpcMode } from "../../../packages/coding-agent/src/modes/rpc/rpc-mode.ts";

const harness = await createHarness({});
const runtime = {
	session: harness.session,
	newSession: async () => ({ cancelled: true }),
	switchSession: async () => ({ cancelled: true }),
	fork: async () => ({ cancelled: true, selectedText: "" }),
	dispose: async () => harness.cleanup(),
	setRebindSession: () => {},
};
await runRpcMode(runtime);
