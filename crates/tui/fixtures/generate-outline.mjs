// Generates crates/tui/fixtures/outline-parity.json by running omp's transcript-outline engine (oh-my-pi
// packages/tui/src/chat/transcript-outline.ts), its scroll arithmetic (components/scroll-viewport.ts
// scrollOffsetForRow) and SGR mouse decoding (mouse.ts parseSgrMouse) over scripted inputs.
//
// The modules are copied into a temporary directory with their imports rewritten to a stub module so they run
// under plain node: visibleWidth/truncateToWidth come from this checkout's packages/tui (the Rust crate's own
// reference), padToWidth is omp's render/utils.ts body (with `…` as the ellipsis), and the theme is a fixed stub
// (`fg` wraps in a 256-color code per color name, `bold` in SGR 1/22; omp's unicode box glyphs). Usage rows are
// children flagged `usage: true`; user-turn detection uses omp's userMessageHasText.
//
// Usage: OMP_SRC=<oh-my-pi checkout> node crates/tui/fixtures/generate-outline.mjs
//        (deterministic; default OMP_SRC is ../omp-reference-src/oh-my-pi beside the worktree's parent)
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const omp = resolve(process.env.OMP_SRC ?? join(here, "../../../../../omp-reference-src/oh-my-pi"));
const tuiSrc = join(omp, "packages/tui/src");
const ultronUtils = pathToFileURL(join(here, "../../../packages/tui/src/utils.ts")).href;

const dir = mkdtempSync(join(tmpdir(), "outline-parity-"));
writeFileSync(
	join(dir, "stubs.ts"),
	`import { visibleWidth as vw, truncateToWidth as tw } from ${JSON.stringify(ultronUtils)};
export const visibleWidth = vw;
export type Component = { render(width: number): readonly string[] };
const CODES: Record<string, number> = { accent: 39, success: 34, dim: 245, muted: 244, warning: 214, error: 196 };
export type ThemeColor = string;
export const theme = {
	fg: (color: string, s: string) => \`\\x1b[38;5;\${CODES[color] ?? 15}m\${s}\\x1b[39m\`,
	bold: (s: string) => \`\\x1b[1m\${s}\\x1b[22m\`,
	boxDotted: { horizontal: "┄", vertical: "┆" },
	boxRound: { topLeft: "╭", topRight: "╮", bottomLeft: "╰", bottomRight: "╯" },
	radio: { selected: "◉", unselected: "○" },
};
export function padToWidth(text: string, width: number): string {
	if (width <= 0) return text;
	const w = vw(text);
	if (w === width) return text;
	const fitted = w < width ? text + " ".repeat(width - w) : tw(text, width, "…");
	const drift = width - vw(fitted);
	return drift > 0 ? fitted + " ".repeat(drift) : fitted;
}
export function isUsageRowBlock(component: { usage?: boolean }): boolean { return component.usage === true; }
export function isUserRequestEntry(_entry: unknown): boolean { return false; }
export function userTurnDraft(_entry: unknown): string | undefined { return undefined; }
export function userMessageLabel(content: unknown): string { return String(content); }
export type TranscriptEntryLike = any;
export type SessionMessageEntryLike = any;
export type ChatTranscriptBuilder = any;
export function getSegmenter() { return new Intl.Segmenter(undefined, { granularity: "grapheme" }); }
export function sliceWithWidth() { throw new Error("unused"); }
`,
);
const rewrite = (source) =>
	source.replace(/from "(\.\.?\/[^"]+)"/g, (_m, path) => {
		if (path.endsWith("/scroll-viewport")) return `from "./scroll-viewport.ts"`;
		return `from "./stubs.ts"`;
	});
writeFileSync(join(dir, "outline.ts"), rewrite(readFileSync(join(tuiSrc, "chat/transcript-outline.ts"), "utf8")));
writeFileSync(
	join(dir, "scroll-viewport.ts"),
	rewrite(readFileSync(join(tuiSrc, "components/scroll-viewport.ts"), "utf8")),
);
writeFileSync(join(dir, "mouse.ts"), rewrite(readFileSync(join(tuiSrc, "mouse.ts"), "utf8")));
const O = await import(pathToFileURL(join(dir, "outline.ts")).href);
const S = await import(pathToFileURL(join(dir, "scroll-viewport.ts")).href);
const M = await import(pathToFileURL(join(dir, "mouse.ts")).href);
rmSync(dir, { recursive: true });

const rowSets = [
	["plain row"],
	["\x1b[31mred\x1b[39m text", "", "second"],
	["日本語のテキスト", "wide ✨ emoji"],
	["a row that is definitely much longer than the inner width of a narrow outline"],
	["", "   ", "core", "  "],
	[],
];
const rules = [];
for (const inner of [0, 1, 6, 10, 24])
	for (const caption of [undefined, "3 blocks →", "a caption wider than everything"])
		for (const color of ["accent", "success"]) rules.push({ inner, caption, color, out: O.outlineRule("╭", "╮", inner, color, caption) });
const outlined = [];
for (const rows of rowSets)
	for (const inner of [0, 10, 18, 40])
		for (const style of [{}, { color: "success", caption: "2 blocks →" }])
			outlined.push({ rows, inner, style, out: O.outlineRows(rows, inner, style) });

// Transcript columns: child rows, targets, selection.
const children = [
	["", "\x1b[1muser\x1b[22m prompt"],
	["assistant says", "", "more"],
	["", "tool card", ""],
	["   "],
	["", ""],
	["follow-up user"],
	["final answer", "line two"],
];
const targets = [
	{ entryId: "u1", turnId: "u1", isUserTurn: true, start: 0, end: 1, entries: [] },
	{ entryId: "r1", turnId: "a1", isUserTurn: false, start: 1, end: 3, entries: [] },
	{ entryId: "x", turnId: "x", isUserTurn: false, start: 3, end: 5, entries: [] },
	{ entryId: "u2", turnId: "u2", isUserTurn: true, start: 5, end: 6, entries: [] },
	{ entryId: "a2", turnId: "a2", isUserTurn: false, start: 6, end: 7, entries: [] },
];
const columns = [];
for (const [from, to] of [[0, 7], [0, 3], [2, 7], [1, 2], [6, 7]])
	for (const selected of [-1, 0, 1, 2, 3, 4])
		for (const width of [8, 14, 30])
			for (const header of [undefined, ["HEADER"]])
				for (const style of [{}, { color: "success", caption: "1 block →" }]) {
					const c = O.composeOutlineColumn(children, from, to, targets, selected, width, header, style);
					columns.push({ from, to, selected, width, header: header ?? null, style, lines: c.lines, selStart: c.selStart, selEnd: c.selEnd });
				}
const visibility = O.outlineVisibility(children, targets);

const zones = [
	"no zone",
	"\x1b]133;A\x07prompt\x1b]133;B\x07",
	"\x1b]133;A\x1b\\st-terminated\x1b]133;D;0\x1b\\ tail",
	"\x1b]133;unterminated",
	"\x1b[31mred\x1b]133;C\x07\x1b[39m",
];
const stripped = O.stripPromptZones(zones);

// appendOutlineEntries over a builder whose append adds `usage` usage rows then `rows` children.
const msg = (id, role, content, rows, usage = 0) => ({ entry: { type: "message", id, message: { role, content } }, rows, usage });
const scripts = [
	[msg("u1", "user", "hello", 2), msg("a1", "assistant", [{ type: "text", text: "hi" }], 1), msg("t1", "toolResult", [], 0), msg("t2", "toolResult", [], 1)],
	[msg("t0", "toolResult", [], 1), msg("n", "custom", "notice", 0), msg("u1", "user", "  ", 1), msg("u2", "user", [{ type: "text", text: "x" }], 1, 1)],
	[msg("a1", "assistant", [], 2), msg("a2", "assistant", [], 1, 2), msg("u1", "user", [{ type: "image" }], 1), msg("t", "toolResult", [], 0, 1), msg("a3", "assistant", [], 0, 1)],
];
const appends = scripts.map((script) => {
	const container = { children: [] };
	const builder = {
		container,
		append([entry]) {
			const step = script.find((s) => s.entry === entry);
			for (let i = 0; i < step.usage; i++) container.children.push({ usage: true });
			for (let i = 0; i < step.rows; i++) container.children.push({});
		},
	};
	const out = O.appendOutlineEntries(builder, script.map((s) => s.entry));
	return {
		entries: script.map((s) => ({ id: s.entry.id, role: s.entry.message.role, content: s.entry.message.content, rows: s.rows, usage: s.usage, isUserTurn: O.isUserTurnEntry(s.entry) })),
		targets: out.map((t) => ({ entryId: t.entryId, turnId: t.turnId, isUserTurn: t.isUserTurn, start: t.start, end: t.end, entries: t.entries.map((e) => e.id) })),
	};
});

const scroll = [];
for (const total of [0, 1, 5, 40])
	for (const height of [0, 1, 4, 10, 50])
		for (const current of [-3, 0, 2, 7, 35, 99])
			for (const row of [-1, 0, 3, 9, 20, 39, 60])
				for (const alignment of ["nearest", "start", "center", "end"])
					scroll.push([current, row, total, height, alignment, S.scrollOffsetForRow(current, row, total, height, alignment)]);

const mice = [
	"\x1b[<0;10;5M", "\x1b[<0;10;5m", "\x1b[<64;1;1M", "\x1b[<65;3;4M", "\x1b[<66;3;4M", "\x1b[<67;3;4M",
	"\x1b[<32;7;2M", "\x1b[<35;7;2M", "\x1b[<2;7;2M", "\x1b[<1;1;1M", "\x1b[<4;2;2M", "\x1b[<16;2;2M",
	"\x1b[<96;5;5M", "\x1b[<0;1;1", "\x1b[0;1;1M", "\x1b[<a;1;1M", "\x1b[<0;1M", "\x1b[<0;1;1;1M", "x",
	"\x1b[<0;250;80M", "\x1b[<64;0;0M",
].map((data) => [data, M.parseSgrMouse(data)]);

writeFileSync(
	join(here, "outline-parity.json"),
	`${JSON.stringify({ rules, outlined, children, targets, columns, visibility, zones, stripped, appends, scroll, mice })}\n`,
);
