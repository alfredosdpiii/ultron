// Generates crates/interactive/fixtures/omp-agent-hub-parity.json from omp's agent hub (oh-my-pi
// packages/tui/src/overlays/agent-hub-projection.ts projectAgentTree, run as a module; and, sliced out of modules
// that import the whole TUI: agent-hub.ts compareRosterAgents and the window arithmetic of #renderRosterWindow,
// agent-hub-renderer.ts treePrefix/taskSummary/formatCost, and packages/utils format.ts formatAge).
//
// The theme is a stub whose `fg` leaves text unstyled, so tree prefixes compare as plain text. taskSummary's
// sanitizeDisplaySingleLine is reduced to its newline collapse (the cases carry no control characters).
//
// Usage: OMP_SRC=<oh-my-pi checkout> node crates/interactive/fixtures/generate-omp-agent-hub.mjs
//        (deterministic; default OMP_SRC is ../omp-reference-src/oh-my-pi beside the worktree's parent)
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const omp = resolve(process.env.OMP_SRC ?? join(here, "../../../../../omp-reference-src/oh-my-pi"));
const overlays = join(omp, "packages/tui/src/overlays");
const read = (p) => readFileSync(p, "utf8");
const slice = (source, start, end) => {
	const a = source.indexOf(start);
	const b = source.indexOf(end, a + 1);
	if (a < 0 || b < 0) throw new Error(`slice not found: ${start}`);
	return source.slice(a, b);
};

const dir = mkdtempSync(join(tmpdir(), "omp-agent-hub-"));
writeFileSync(join(dir, "agent-hub-types.ts"), read(join(overlays, "agent-hub-types.ts")));
writeFileSync(
	join(dir, "projection.ts"),
	read(join(overlays, "agent-hub-projection.ts")).replaceAll(`from "./agent-hub-types"`, `from "./agent-hub-types.ts"`),
);
const hub = read(join(overlays, "agent-hub.ts"));
const renderer = read(join(overlays, "agent-hub-renderer.ts"));
const format = read(join(omp, "packages/utils/src/format.ts"));
const windowBody = slice(hub, "		let start = this.#selectedRow;", "		return { lines, hitRows };\n	}\n\n	#summaryLines")
	.replaceAll("this.#selectedRow", "selectedRow")
	.replaceAll("this.#rows.length", "heights.length");
writeFileSync(
	join(dir, "helpers.ts"),
	`import { MAIN_AGENT_ID, type AgentRecordLike } from "./agent-hub-types.ts";
const STATUS_ORDER = { running: 0, idle: 1, parked: 2, aborted: 3 };
const theme = { fg: (_c: string, s: string) => s };
function sanitizeDisplaySingleLine(text: string): string { return text.replace(/[\\r\\n]+/g, " "); }
${slice(hub, "function compareRosterAgents", "function activityGlyph")}
${slice(renderer, "/** Legacy progress snapshots may omit counters", "/** Compute the max content width")}
${slice(renderer, "const ASSIGNMENT_PREFIX", "/** Native span of {@link statusGlyph}")}
${slice(renderer, "export function formatCost", "export function formatMetrics(")}
${slice(renderer, "const TREE_SEGMENT_WIDTH", "/** Bash \`tree\`-style branch")}
${slice(format, "export function formatAge", "/**\n * Pluralize")}
export function rosterWindow(heights: number[], selectedRow: number, budget: number) {
	const lines: string[] = [];
	const hitRows: Array<number | undefined> = [];
	const entryAt = (index: number): string[] => Array.from({ length: heights[index]! }, (_, k) => \`\${index}.\${k}\`);
	const appendEntry = (index: number, entry = entryAt(index)): void => {
		for (const line of entry) {
			lines.push(line);
			hitRows.push(index);
		}
	};
${windowBody}		return { lines, hitRows };
}
export { compareRosterAgents, treePrefix };
`,
);
const P = await import(pathToFileURL(join(dir, "projection.ts")).href);
const H = await import(pathToFileURL(join(dir, "helpers.ts")).href);
rmSync(dir, { recursive: true });

const ref = (id, status, lastActivity, parentId) => ({ id, displayName: id, kind: "sub", status, lastActivity, parentId, session: null, sessionFile: null, createdAt: 0 });
const rosters = [
	[ref("a", "idle", 10), ref("b", "running", 5), ref("c", "running", 9), ref("d", "aborted", 99), ref("e", "parked", 1), ref("f", "idle", 10)],
	[ref("x", "running", 1, "Main"), ref("y", "running", 1, "x"), ref("z", "idle", 2, "y"), ref("w", "idle", 3, "x"), ref("v", "idle", 4, "gone")],
	[ref("p", "idle", 1, "q"), ref("q", "idle", 1, "p"), ref("r", "idle", 1, "q")],
	[ref("c1", "running", 3, "root"), ref("root", "idle", 1), ref("c2", "running", 2, "root"), ref("g1", "idle", 1, "c1"), ref("g2", "idle", 1, "c1"), ref("g3", "idle", 1, "g1")],
	[],
];
const sorted = rosters.map((refs) => [...refs].sort(H.compareRosterAgents).map((r) => r.id));
const trees = [];
for (const refs of rosters) {
	for (const ranks of [null, "order", "newChild"]) {
		let rank;
		if (ranks === "order") rank = new Map(refs.map((r, i) => [r, i]));
		if (ranks === "newChild") rank = new Map(refs.map((r, i) => [r, i === refs.length - 1 && i > 0 ? -1 : i]));
		const t = P.projectAgentTree(refs, rank);
		const prefixes = [];
		for (const width of [3, 6, 9, 40])
			for (const continuation of [false, true])
				prefixes.push(t.rows.map((r) => H.treePrefix(r, width, t.depthById, t.parentById, t.lastSiblingById, continuation)));
		trees.push({
			ids: refs.map((r) => r.id),
			ranks: rank ? refs.map((r) => rank.get(r)) : null,
			rows: t.rows.map((r) => r.id),
			depth: Object.fromEntries(t.depthById),
			parent: Object.fromEntries(t.parentById),
			last: Object.fromEntries(t.lastSiblingById),
			prefixes,
		});
	}
}
const tasks = ["Complete assignment thoroughly: fix it", "  complete ASSIGNMENT thoroughly:\n\tline two", "plain\r\ntask", "Complete assignment thoroughly:", ""].map((t) => [t, H.taskSummary(t)]);
const costs = [0, 0.001234, 0.0099, 0.01, 0.5555, 0.9999, 1, 12.345, -1].map((c) => [c, H.formatCost(c)]);
const ages = [0, 1, 59, 60, 3599, 3600, 86399, 86400, 604800, 2592000, 31536000].map((s) => [s, H.formatAge(s)]);
const windows = [];
const heightSets = [[3, 3, 3, 3, 3], [2, 3, 2, 3, 2, 3, 2], [1, 1, 1, 1, 1, 1, 1, 1, 1, 1], [5], [4, 9, 1]];
for (const heights of heightSets)
	for (let selected = 0; selected < heights.length; selected++)
		for (const budget of [1, 3, 4, 6, 7, 10, 30]) {
			const w = H.rosterWindow(heights, selected, budget);
			windows.push({ heights, selected, budget, lines: w.lines, hits: w.hitRows.map((h) => h ?? null) });
		}

writeFileSync(
	join(here, "omp-agent-hub-parity.json"),
	`${JSON.stringify({ rosters: rosters.map((r) => r.map(({ id, status, lastActivity, parentId }) => ({ id, status, lastActivity, parentId: parentId ?? null }))), sorted, trees, tasks, costs, ages, windows })}\n`,
);
