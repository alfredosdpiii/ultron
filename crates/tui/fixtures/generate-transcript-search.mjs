// Generates crates/tui/fixtures/transcript-search-parity.json from packages/tui/src/alt-screen-search.ts (TypeScript,
// this checkout): match segments over rendered transcript rows (ASCII fast path, ANSI, wide graphemes, matches
// across wrapped rows), the index's change flags, match keys, and the search bar's rows and button hit-testing at
// several widths with the default keybindings (process.platform as run: linux). Row highlighting replays the body of
// TuiAltScreen's private applySearchHighlights/applySearchTextHighlight (copied below, with the module's own
// sliceByColumn, visibleWidth and extractAnsiCode) using `<..>` / `[..]` styles.
// Usage: node crates/tui/fixtures/generate-transcript-search.mjs   (deterministic; output is written next to this file)
import { writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const src = (p) => import(join(here, "../../../packages/tui/src", p));
const S = await src("alt-screen-search.ts");
const { KeybindingsManager, TUI_KEYBINDINGS, setKeybindings } = await src("keybindings.ts");
const { sliceByColumn, visibleWidth, extractAnsiCode } = await src("utils.ts");
setKeybindings(new KeybindingsManager(TUI_KEYBINDINGS));

const corpora = [
	["hello world", "  indented hello", "HELLO again"],
	["the quick brown", "fox jumps over", "the lazy dog"],
	["\x1b[1mbold\x1b[22m word and \x1b[31mred word\x1b[39m", "", "word"],
	["日本語 テキスト 検索", "émoji ✨ sparkle ✨", "ünïcode Straße"],
	["a  b   c", "a b c", "abc"],
	["tab\there", "ends with space ", " starts", "x"],
	["aaaa", "aa"],
	["Wrapped long line that continues", "on the next row"],
	[],
	["(regex) [chars] .* +? ^$ {1} |pipe| \\back"],
];
const queries = [
	"hello", "HELLO", "world hello", "  the   ", "word", "red word", "o", "line that continues on", "テキスト",
	"✨", "straße", "a b", "b c", "aa", "", "   ", "[chars]", ".*", "\\back", "sparkle ✨ ünïcode", "continues on the",
];
const matches = [];
for (const lines of corpora)
	for (const query of queries) matches.push({ lines, query, matches: S.findAltScreenSearchMatches(lines, query) });

const index = new S.AltScreenSearchIndex();
const steps = [
	[["one two", "three"], "two"],
	[["one two", "three"], "two"],
	[["one two", "three"], " two "],
	[["one two", "three"], "three"],
	[["one two", "three", "two"], "three"],
	[["one two", "three", "two"], "three"],
	[["one TWO"], "two"],
];
const indexSteps = steps.map(([lines, query]) => {
	const r = index.search(lines, query);
	return { lines, query, changed: r.changed, keys: r.matches.map(S.getAltScreenSearchMatchKey) };
});

const bars = [];
for (const width of [1, 2, 8, 20, 30, 44, 80])
	for (const [query, index, count] of [["", -1, 0], ["foo", -1, 0], ["foo", 0, 3], ["longer query text", 11, 12]])
		for (const hovered of [undefined, -1, 1]) {
			const c = new S.AltScreenSearchComponent(() => {}, (text, h) => (h ? `[${text}]` : text));
			for (const ch of query) c.handleInput(ch);
			c.setResult(index, count);
			c.setHoveredNavigationDirection(hovered);
			const lines = c.render(width);
			const hits = [];
			for (let col = 0; col < width; col++) hits.push(c.getNavigationDirectionAt(2, col) ?? 0);
			bars.push({ width, query, index, count, hovered: hovered ?? 0, lines, hits, row1: c.getNavigationDirectionAt(1, 3) ?? 0 });
		}

function applySearchTextHighlight(text, current) {
	const style = current ? (t) => `[${t}]` : (t) => `<${t}>`;
	let result = "";
	let plainStart = 0;
	let index = 0;
	while (index < text.length) {
		const ansi = extractAnsiCode(text, index);
		if (!ansi) {
			index += 1;
			continue;
		}
		if (index > plainStart) result += style(text.slice(plainStart, index));
		result += ansi.code;
		index += ansi.length;
		plainStart = index;
	}
	if (plainStart < text.length) result += style(text.slice(plainStart));
	return result;
}
function highlightRow(line, ranges) {
	const lineWidth = visibleWidth(line);
	for (const range of [...ranges].sort((a, b) => b.startCol - a.startCol)) {
		const startCol = Math.min(range.startCol, lineWidth);
		const endCol = Math.min(range.endCol, lineWidth);
		if (endCol <= startCol) continue;
		const before = sliceByColumn(line, 0, startCol, true);
		const highlighted = sliceByColumn(line, startCol, endCol - startCol, true);
		const after = sliceByColumn(line, endCol, Math.max(0, lineWidth - endCol), true);
		line = `${before}${applySearchTextHighlight(highlighted, range.current)}${after}`;
	}
	return line;
}
const rows = ["hello world", "\x1b[31mred\x1b[39m text", "ab", "日本語 テキスト", "\x1b[1mbold\x1b[22m and \x1b[4mund\x1b[24m", "x\ty"];
const rangeSets = [
	[{ startCol: 0, endCol: 2, current: false }],
	[{ startCol: 6, endCol: 11, current: true }, { startCol: 0, endCol: 2, current: false }],
	[{ startCol: 1, endCol: 6, current: false }],
	[{ startCol: 1, endCol: 9, current: false }, { startCol: 5, endCol: 9, current: true }],
	[{ startCol: 2, endCol: 5, current: true }, { startCol: 7, endCol: 9, current: false }],
	[{ startCol: 3, endCol: 3, current: false }],
];
const highlights = [];
for (const line of rows) for (const ranges of rangeSets) highlights.push({ line, ranges, out: highlightRow(line, ranges) });

writeFileSync(join(here, "transcript-search-parity.json"), `${JSON.stringify({ matches, indexSteps, bars, highlights })}\n`);
