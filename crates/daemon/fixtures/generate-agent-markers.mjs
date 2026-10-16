// Generates crates/daemon/fixtures/agent-markers.json: the variables cli/setup.ts `setupCli` sets in the CLI's
// environment before anything starts (and that every process the CLI starts inherits), and what an internal
// process (experimental/process.ts: `{ ...process.env, ...options.env }`) sees when options.env names one too.
// crates/daemon/tests/agent_markers.rs replays it.
// Usage: node crates/daemon/fixtures/generate-agent-markers.mjs
import "../../cli/fixtures/offline-guard.mjs";
import { writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
for (const key of ["PI_CODING_AGENT", "AI_AGENT"]) delete process.env[key];
process.env.AI_AGENT = "someone-else";
const { setupCli } = await import(pathToFileURL(join(here, "../../../packages/coding-agent/src/cli/setup.ts")).href);
setupCli();
const markers = { PI_CODING_AGENT: process.env.PI_CODING_AGENT, AI_AGENT: process.env.AI_AGENT };
const internal = { ...process.env, ...{ AI_AGENT: "from-options" } };
writeFileSync(
	join(here, "agent-markers.json"),
	`${JSON.stringify({ markers, withOptionsEnv: { PI_CODING_AGENT: internal.PI_CODING_AGENT, AI_AGENT: internal.AI_AGENT } }, null, 1)}\n`,
);
console.log("wrote agent markers");
