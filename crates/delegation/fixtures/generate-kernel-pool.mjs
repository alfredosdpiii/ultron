// Generates crates/delegation/fixtures/kernel-pool.json from packages/coding-agent/src/ultron/rlm/kernel-pool.ts.
// Each scenario drives a KernelPool over fake kernels (scripted snapshot outcomes, fixed clock) and records every
// operation's result or error, plus the log of kernels created, snapshotted and shut down.
// Usage: node crates/delegation/fixtures/generate-kernel-pool.mjs
import { writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
const here = dirname(fileURLToPath(import.meta.url));
const { KernelPool } = await import(join(here, "../../../packages/coding-agent/src/ultron/rlm/kernel-pool.ts"));

// snapshot outcomes per lane: {skipped: [...]} | {error: "evalue"} | {throws: "msg"} | {none: true}
const scenarios = [
	{ name: "capacity evicts least recently used", options: { maxLive: 2, idleTtlMs: 1000 }, ops: [
		["acquire", "a"], ["tick", 5], ["acquire", "b"], ["release", 0], ["tick", 5], ["release", 1], ["tick", 5],
		["acquire", "a"], ["release", 2], ["tick", 1], ["acquire", "c"], ["stats"], ["evictions"]] },
	{ name: "capacity error when all running or pinned", options: { maxLive: 2, idleTtlMs: 1000 }, ops: [
		["acquire", "a"], ["acquire", "b"], ["acquire", "c"], ["release", 1], ["pin", "b", "holder"], ["acquire", "c"],
		["unpin", "b", "holder"], ["acquire", "c"], ["stats"], ["evictions"]] },
	{ name: "explicit refusals", options: { maxLive: 3, idleTtlMs: 1000 }, ops: [
		["evict", "none"], ["acquire", "a"], ["evict", "a"], ["release", 0], ["release", 0], ["pin", "a", "x"], ["pin", "a", "y"],
		["evict", "a"], ["unpin", "a", "x"], ["evict", "a"], ["unpin", "a", "y"], ["evict", "a"], ["live", "a"], ["stats"]] },
	{ name: "sweep by idle ttl", options: { maxLive: 4, idleTtlMs: 100 }, ops: [
		["acquire", "a"], ["release", 0], ["tick", 50], ["acquire", "b"], ["release", 1], ["acquire", "c"], ["pin", "d", "h"],
		["acquire", "d"], ["release", 3], ["tick", 60], ["sweep"], ["tick", 50], ["sweep"], ["stats"], ["live", "b"], ["live", "c"]] },
	{ name: "snapshot before eviction", options: { maxLive: 4, idleTtlMs: 0, snapshot: true },
		snapshots: { a: { skipped: [] }, b: { skipped: ["sock", "lock"] }, c: { error: "pickle failed" }, d: { throws: "kernel gone" }, e: { none: true }, f: { error: null } },
		ops: [
		["acquire", "a"], ["acquire", "b"], ["acquire", "c"], ["acquire", "d"], ["release", 0], ["release", 1], ["release", 2], ["release", 3],
		["evict", "a"], ["evict", "b"], ["evict", "c"], ["evict", "d"], ["acquire", "e"], ["release", 4], ["evict", "e"],
		["acquire", "f"], ["release", 5], ["sweep"], ["stats"]] },
	{ name: "close snapshots idle, skips running, ignores pins and failures", options: { maxLive: 4, idleTtlMs: 0, snapshot: true },
		snapshots: { a: { skipped: [] }, b: { skipped: [] }, c: { error: "bad" } },
		ops: [["acquire", "a"], ["acquire", "b"], ["acquire", "c"], ["release", 1], ["release", 2], ["pin", "b", "h"], ["close"], ["acquire", "z"], ["evictions"]] },
	{ name: "pin capacity", options: { maxLive: 3, idleTtlMs: 0, maxPinned: 1 }, ops: [
		["pin", "a", "h1"], ["pin", "a", "h2"], ["pin", "b", "h1"], ["unpin", "a", "h1"], ["pin", "b", "h1"], ["unpin", "a", "h2"], ["pin", "b", "h1"], ["stats"]] },
	{ name: "constructor validation", options: { maxLive: 0, idleTtlMs: 0 }, ops: [] },
	{ name: "constructor ttl", options: { maxLive: 1, idleTtlMs: -1 }, ops: [] },
	{ name: "constructor pinned", options: { maxLive: 1, idleTtlMs: 0, maxPinned: 2 }, ops: [] },
	{ name: "empty pin holder", options: { maxLive: 1, idleTtlMs: 0 }, ops: [["pin", "a", ""]] },
];

const out = [];
for (const s of scenarios) {
	let clock = 1000;
	const log = [];
	const steps = [];
	let pool;
	try {
		pool = new KernelPool({
			maxLive: s.options.maxLive,
			idleTtlMs: s.options.idleTtlMs,
			...(s.options.maxPinned === undefined ? {} : { maxPinned: s.options.maxPinned }),
			now: () => clock,
			snapshotPath: s.options.snapshot ? (lane) => `/snap/${lane}.pkl` : undefined,
			create: (lane) => {
				log.push(`create ${lane}`);
				return {
					lane,
					snapshot: async (path) => {
						log.push(`snapshot ${lane} ${path}`);
						const o = s.snapshots?.[lane] ?? { skipped: [] };
						if (o.throws) throw new Error(o.throws);
						if (o.none) return { status: "ok" };
						if ("error" in o) return { status: "error", error: o.error === null ? undefined : { evalue: o.error } };
						return { status: "ok", snapshot: { skipped: o.skipped } };
					},
					shutdown: async () => void log.push(`shutdown ${lane}`),
				};
			},
		});
	} catch (e) {
		out.push({ name: s.name, options: s.options, snapshots: s.snapshots ?? {}, constructError: e.message, steps: [], log });
		continue;
	}
	const leases = [];
	for (const op of s.ops) {
		let result, error;
		try {
			switch (op[0]) {
				case "acquire": { const l = await pool.acquire(op[1]); leases.push(l); result = l.kernel.lane; break; }
				case "release": leases[op[1]].release(); result = null; break;
				case "tick": clock += op[1]; result = null; break;
				case "pin": pool.pin(op[1], op[2]); result = null; break;
				case "unpin": pool.unpin(op[1], op[2]); result = null; break;
				case "evict": result = await pool.evict(op[1]); break;
				case "sweep": result = await pool.sweep(); break;
				case "stats": result = pool.stats(); break;
				case "evictions": result = [...pool.evictions]; break;
				case "live": result = pool.live(op[1])?.lane ?? null; break;
				case "close": await pool.close(); result = null; break;
			}
		} catch (e) { error = e.message; }
		steps.push({ op, ...(error === undefined ? { result: JSON.parse(JSON.stringify(result ?? null)) } : { error }) });
	}
	out.push({ name: s.name, options: s.options, snapshots: s.snapshots ?? {}, constructError: null, steps, log });
}
writeFileSync(join(here, "kernel-pool.json"), `${JSON.stringify(out, null, "\t")}\n`);
