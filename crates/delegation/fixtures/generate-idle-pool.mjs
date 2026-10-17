// Generates crates/delegation/fixtures/idle-pool.json from packages/coding-agent/src/experimental/session-worker.ts
// (TypeScript, this checkout): the session's rlm tool (`createUltronRlmTool`, the pool the footer's kernel strip
// reads through `rlm.pool`) is created with its defaults and inspected before any cell ran. Its `poolStats()` and the
// kernel strip drawn from it (rlm-graph.ts `renderKernelStrip`, PLAIN_STYLE, 80 columns) are recorded. No kernel
// process starts: the TS pool admits a kernel only for a cell (`kernels.use`).
// Usage: node crates/delegation/fixtures/generate-idle-pool.mjs   (deterministic; output is written next to this file)
import "../../cli/fixtures/offline-guard.mjs";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const ca = (p) => import(join(here, "../../../packages/coding-agent/src", p));
const { createUltronRlmTool } = await ca("experimental/session-worker.ts");
const V = await ca("experimental/rlm-visualizer.ts");
const G = await ca("experimental/rlm-graph.ts");

const cwd = mkdtempSync(join(tmpdir(), "idle-pool-"));
const handler = async () => {
	throw new Error("no host requests in this fixture");
};
const tool = createUltronRlmTool(cwd, handler, undefined, { now: () => 1_000_000 });
const stats = tool.poolStats();
const strip = G.renderKernelStrip({ pool: V.parsePool(stats) }, 80, V.PLAIN_STYLE);
await tool.close?.();
rmSync(cwd, { recursive: true, force: true });

writeFileSync(join(here, "idle-pool.json"), `${JSON.stringify({ idle: { stats, strip } }, null, "\t")}\n`);
