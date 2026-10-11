// Generates crates/tui/fixtures/tui-parity.json by running packages/tui (TypeScript, this checkout) on scripted
// inputs: widths of every code point and of grapheme samples, truncation, wrapping, column slicing, key parsing,
// stdin splitting, component output at several widths, and the main-screen renderer's escape output per frame.
// Usage: node crates/tui/fixtures/generate.mjs   (deterministic; output is written next to this file)
import { writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
const here = dirname(fileURLToPath(import.meta.url));
const src = (p) => import(join(here, "../../../packages/tui/src", p));
const utils = await src("utils.ts");
const keys = await src("keys.ts");
const { StdinBuffer } = await src("stdin-buffer.ts");
const { Text } = await src("components/text.ts");
const { TruncatedText } = await src("components/truncated-text.ts");
const { Box } = await src("components/box.ts");
const { TuiMainScreen } = await src("tui-main-screen.ts");
const { CURSOR_MARKER } = await src("tui.ts");

let seed = 0x2545f491;
const rand = (n) => {
	seed ^= seed << 13; seed >>>= 0; seed ^= seed >>> 17; seed ^= seed << 5; seed >>>= 0;
	return seed % n;
};

// Per-code-point width, run-length encoded as [start, width] at each change.
const singleWidths = [];
let prev = -1;
for (let cp = 0; cp <= 0x10ffff; cp++) {
	if (cp >= 0xd800 && cp <= 0xdfff) continue;
	const w = utils.visibleWidth(String.fromCodePoint(cp));
	if (w !== prev) singleWidths.push([cp, w]);
	prev = w;
}

const FRAGMENTS = [
	"hello", "world", " ", "  ", "\t", "a-b/c.d", "supercalifragilisticexpialidocious", "日本語のテキスト", "中文", "한국어",
	"，", "。", "😀", "👍🏽", "👨‍👩‍👧‍👦", "🇯🇵", "🇺", "1️⃣", "🏴󠁧󠁢󠁳󠁣󠁴󠁿", "❤️", "☺", "⌚", "é", "क्षि", "ปำ", "ຳ",
	"ｈａｌｆ", "ﾊﾝｶｸ", "​", "­", "ẍ̈", "\x1b[1m", "\x1b[31m", "\x1b[4m", "\x1b[0m", "\x1b[38;5;240m",
	"\x1b[48;2;10;20;30m", "\x1b[22m", "\x1b[24m", "\x1b[39m", "\x1b[49m", "\x1b]8;;https://example.com\x07", "\x1b]8;;\x07",
	"\x1b]8;id=1;https://x.y\x1b\\", "\x1b]8;;\x1b\\", "\x1b_pi:c\x07", "\x1b[2K", "\n", "\r\n", " ", "ab　cd",
	"Ελληνικά", "Кириллица", "‮", "\u0007",
];
const randomText = (n) => Array.from({ length: n }, () => FRAGMENTS[rand(FRAGMENTS.length)]).join("");
const samples = [...FRAGMENTS, "", "plain ascii line", ...Array.from({ length: 300 }, () => randomText(1 + rand(12)))];

const widths = samples.map((s) => [s, utils.visibleWidth(s)]);
const truncate = [];
const wrap = [];
const slice = [];
for (const s of samples) {
	for (const w of [0, 1, 2, 3, 5, 8, 13, 40]) {
		for (const [ellipsis, pad] of [["...", false], ["…", true], ["", false], ["⋯⋯", true]]) {
			truncate.push([s, w, ellipsis, pad, utils.truncateToWidth(s, w, ellipsis, pad)]);
		}
		if (w > 0) wrap.push([s, w, utils.wrapTextWithAnsi(s, w)]);
		const start = rand(6);
		const strict = rand(2) === 1;
		const r = utils.sliceWithWidth(s, start, w, strict);
		slice.push([s, start, w, strict, r.text, r.width]);
	}
}

// Key inputs: legacy, SS3, modifyOtherKeys, kitty CSI-u (with alternates, events, locks, keypad), raw bytes.
const keyInputs = [];
for (let c = 0; c < 128; c++) {
	keyInputs.push(String.fromCharCode(c), `\x1b${String.fromCharCode(c)}`);
}
for (const s of ["A", "B", "C", "D", "H", "F", "E", "Z", "a", "b", "c", "d", "e", "2~", "3~", "5~", "6~", "7~", "8~",
	"[5~", "[6~", "[A", "[B", "[C", "[D", "[E", "11~", "12~", "13~", "14~", "15~", "17~", "18~", "19~", "20~", "21~", "23~", "24~",
	"2$", "3$", "5$", "6$", "7$", "8$", "2^", "3^", "5^", "6^", "7^", "8^", "200~", "M", "<0;1;2M"]) keyInputs.push(`\x1b[${s}`);
for (const s of "ABCDHFEPQRSMabcde") keyInputs.push(`\x1bO${s}`);
for (const m of [1, 2, 3, 5, 6, 9, 65, 129, 17, 0]) {
	for (const f of "ABCDHF") keyInputs.push(`\x1b[1;${m}${f}`, `\x1b[1;${m}:3${f}`);
	for (const n of [2, 3, 5, 6, 7, 8, 9]) keyInputs.push(`\x1b[${n};${m}~`, `\x1b[${n};${m}:2~`);
	for (const cp of [9, 13, 27, 32, 47, 48, 57, 65, 90, 97, 122, 127, 228, 1092, 57399, 57409, 57414, 57417, 57426, 57441, 91, 59]) {
		keyInputs.push(`\x1b[${cp};${m}u`, `\x1b[${cp}:${cp + 1};${m}u`, `\x1b[${cp}::${97};${m}u`, `\x1b[${cp};${m}:3u`, `\x1b[27;${m};${cp}~`);
	}
}
keyInputs.push("\x1b[97u", "\x1b[97:65u", "\x1b[97:65:97u", "\x1b[97:1:2:3u", "\x1b[3:3~", "\x1b[x;5u", "é", "日", "😀", "\x1b[1;5", "");
const parseKey = [];
for (const kitty of [false, true]) {
	keys.setKittyProtocolActive(kitty);
	for (const s of keyInputs) parseKey.push([s, kitty, keys.parseKey(s) ?? null]);
}
keys.setKittyProtocolActive(false);
const printable = keyInputs.map((s) => [s, keys.decodePrintableKey(s) ?? null]);

// Stdin chunk scripts -> emitted events, then the flushed remainder.
const stdinScripts = [
	["abc"], ["\x1b[A\x1b[B"], ["\x1b[", "A"], ["\x1b"], ["\x1b\x1b[27;1:3u"], ["\x1b\x1b"], ["\x1b[200~hello\nworld\x1b[201~x"],
	["a\x1b[200~par", "tial", "\x1b[201~\x1b[B"], ["\x1b]11;rgb:0/0/0\x07rest"], ["\x1b_Gi=1;OK\x1b\\z"], ["\x1bP>|kitty\x1b\\"],
	["\x1b[<0;10;5M\x1b[<0;10;5m"], ["\x1b[<0;10"], ["\x1b[Mabc"], ["\x1bOA\x1bO"], ["\x1b[97u", "a"], ["\x1b[97ub"],
	["\x1b[1;5A\x1b[3~é日"], ["\x1ba\x1b[200~\x1b[201~"], ["", "x"], [""],
];
const stdin = stdinScripts.map((chunks) => {
	const buf = new StdinBuffer();
	const events = [];
	buf.on("data", (d) => events.push(["data", d]));
	buf.on("paste", (d) => events.push(["paste", d]));
	for (const c of chunks) buf.process(c);
	const flushed = buf.flush();
	buf.destroy();
	return [chunks, events, flushed];
});

const bg = (s) => `\x1b[44m${s}\x1b[49m`;
const components = [];
for (const s of samples.slice(0, 120)) {
	for (const w of [1, 4, 10, 30, 80]) {
		components.push(["text", s, w, 1, 1, false, new Text(s, 1, 1).render(w)]);
		components.push(["text", s, w, 3, 0, true, new Text(s, 3, 0, bg).render(w)]);
		components.push(["truncated", s, w, 2, 1, false, new TruncatedText(s, 2, 1).render(w)]);
		for (const [px, py, withBg] of [[1, 1, false], [2, 0, true], [0, 2, false]]) {
			const box = new Box(px, py, withBg ? bg : undefined);
			box.addChild(new Text(s, 1, 0));
			box.addChild(new TruncatedText(s, 0, 0));
			components.push(["box", s, w, px, py, withBg, box.render(w)]);
		}
	}
}

// Renderer: scripted frames through TuiMainScreen with a recording terminal.
class RecordingTerminal {
	writes = [];
	columns = 20;
	rows = 6;
	kittyProtocolActive = false;
	start() {}
	stop() {}
	async drainInput() {}
	write(d) { this.writes.push(d); }
	moveBy() {}
	hideCursor() { this.writes.push("\x1b[?25l"); }
	showCursor() { this.writes.push("\x1b[?25h"); }
	clearLine() {}
	clearFromCursor() {}
	clearScreen() {}
	setTitle() {}
	setProgress() {}
}
const lines = (n, tag = "") => Array.from({ length: n }, (_, i) => `line ${i}${tag}`);
const renderScripts = [
	{ name: "grow-edit-shrink", frames: [[lines(3)], [lines(3)], [[...lines(2), "changed"]], [lines(5)], [lines(2)], [lines(2)]] },
	{ name: "scroll-past-viewport", frames: [[lines(4)], [lines(9)], [[...lines(8), "tail edit"]], [[...lines(3), "x", ...lines(9).slice(4)]], [lines(12)], [lines(3)]] },
	{ name: "resize", frames: [[lines(3)], [lines(3), 30, 6], [lines(3), 30, 4], [lines(7), 30, 4], [lines(7, "!"), 30, 4]] },
	{ name: "cursor", cursor: true, frames: [[["a", `ab${CURSOR_MARKER}c`, "d"]], [["a", "ab", `日本${CURSOR_MARKER}x`]], [["a"]], [[]], [lines(2)]] },
	{ name: "clear-on-shrink", clearOnShrink: true, frames: [[lines(5)], [lines(3)], [lines(4)]] },
	{ name: "styles", frames: [[["\x1b[31mred", "tab\there", "ปำ", "\x1b_Gimage\x1b\\"]], [["\x1b[31mred", "tab\tthere", "ปำ", "x"]]] },
	{ name: "empty", frames: [[[]], [["a"]], [[]]] },
];
const renders = renderScripts.map(({ name, frames, cursor, clearOnShrink }) => {
	const terminal = new RecordingTerminal();
	const tui = new TuiMainScreen(terminal, cursor ?? false);
	if (clearOnShrink) tui.setClearOnShrink(true);
	let current = [];
	tui.addChild({ render: () => [...current], invalidate() {} });
	tui.start();
	const out = [];
	for (const [frameLines, width, height] of frames) {
		terminal.columns = width ?? 20;
		terminal.rows = height ?? 6;
		terminal.writes = [];
		current = frameLines;
		tui.renderNow();
		out.push([frameLines, terminal.columns, terminal.rows, terminal.writes.join("")]);
	}
	tui.stop();
	return { name, showHardwareCursor: cursor ?? false, clearOnShrink: clearOnShrink ?? false, frames: out };
});

const fixture = { singleWidths, widths, truncate, wrap, slice, parseKey, printable, stdin, components, renders };
writeFileSync(join(here, "tui-parity.json"), `${JSON.stringify(fixture)}\n`);
