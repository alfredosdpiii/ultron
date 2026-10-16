// Generates crates/claude-root/fixtures/watch.json from packages/coding-agent's `ultron watch` frame
// (ultron/claude/watch.ts renderWatchFrame, with experimental/rlm-pane.ts and rlm-graph.ts; TypeScript, this checkout).
// The snapshots are the wire cases of crates/interactive/fixtures/rlm_views.json, built through the TypeScript parsers;
// each is drawn with PLAIN_STYLE as `ultron watch --once` draws it (pane height 30, unfocused) for several server
// statuses and terminal sizes. Date.now is the snapshot's time. crates/claude-root/tests/watch.rs replays every case.
// Usage: node crates/claude-root/fixtures/generate-watch.mjs   (deterministic, offline)
import "../../cli/fixtures/offline-guard.mjs";
import { readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const ca = (p) => import(join(here, "../../../packages/coding-agent/src", p));
const V = await ca("experimental/rlm-visualizer.ts");
const P = await ca("experimental/rlm-pane.ts");
const W = await ca("ultron/claude/watch.ts");
const { KeybindingsManager } = await ca("core/keybindings.ts");
const keybindings = new KeybindingsManager({});
const views = JSON.parse(readFileSync(join(here, "../../interactive/fixtures/rlm_views.json"), "utf8"));

function snapshot(w) {
	const status = V.parseAgentsStatus(w.agentsStatus);
	return {
		now: w.now,
		tasks: status.tasks,
		usage: status.usage,
		limits: status.limits,
		jobs: status.jobs,
		toolCalls: status.toolCalls,
		workflows: status.workflows,
		...(status.truncatedTasks === undefined ? {} : { truncatedTasks: status.truncatedTasks }),
		pool: V.parsePool(w.pool),
		retained: V.parseRetained(w.instances),
		frames: V.parseFrames(w.frames),
		context: V.parseContextState(w.ctx),
		cells: w.cells ?? [],
		turn: w.turn ?? null,
		rootCell: w.rootCell ?? null,
		timing: new Map(Object.entries(w.timing ?? {})),
		progress: new Map(Object.entries(w.progress ?? {}).map(([id, value]) => [id, V.parseProgress(value)])),
		...(w.error === undefined ? {} : { error: w.error }),
	};
}

const statuses = [
	{},
	{ cwd: "/work/app", claudeSessionId: "0123456789abcdef", ready: false, frameModel: "anthropic/claude-x", children: "2/4", cells: 3, pendingEvents: 2 },
	{ cwd: "/work/app", ready: true, cells: 0, pendingEvents: 0 },
];
const cases = [];
for (const view of views.cases) {
	const snap = snapshot(view.wire);
	Date.now = () => view.wire.now;
	for (const [index, status] of statuses.entries()) {
		for (const [width, height] of [[100, 40], [60, 24], [140, 50]]) {
			const pane = new P.RlmPane({
				snapshot: () => snap,
				height: () => 30,
				keybindings,
				style: V.PLAIN_STYLE,
				focused: () => false,
				onClose: () => {},
				requestRender: () => {},
			});
			const lines = W.renderWatchFrame(pane, snap, status, width, height, V.PLAIN_STYLE);
			cases.push({ name: view.name, status: index, width, height, lines });
		}
	}
}
writeFileSync(join(here, "watch.json"), `${JSON.stringify({ generator: "generate-watch.mjs", statuses, wires: Object.fromEntries(views.cases.map((v) => [v.name, v.wire])), cases }, null, "\t")}\n`);
