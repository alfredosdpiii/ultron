// Generates crates/tui/fixtures/vim-parity.json by running omp's Vim state machine (oh-my-pi
// packages/tui/src/vim.ts, TypeScript) over scripted buffers and key sequences. vim.ts imports only the grapheme
// segmenter and the word-navigation helpers from utils.ts; those are copied verbatim from omp's utils.ts into a
// temporary module so the reference runs under plain node without omp's dependencies.
//
// After every key the record holds the returned commands (null when the key is not Vim's), the mode, the pending
// text and the Visual anchor. Move commands are then applied the way omp's Editor does (#moveVimCursor clamps to
// the line, #clampVimCursor keeps a Normal cursor on the last grapheme); edits are recorded but not applied.
// Columns are converted from UTF-16 offsets to UTF-8 byte offsets; a column past the line end keeps its excess,
// and Infinity (the sticky `$` column) is written as -1.
//
// Usage: OMP_SRC=<oh-my-pi checkout> node crates/tui/fixtures/generate-vim.mjs
//        (deterministic; default OMP_SRC is ../omp-reference-src/oh-my-pi beside the worktree's parent)
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const omp = resolve(process.env.OMP_SRC ?? join(here, "../../../../../omp-reference-src/oh-my-pi"));
const tuiSrc = join(omp, "packages/tui/src");
const utils = readFileSync(join(tuiSrc, "utils.ts"), "utf8");
const start = utils.indexOf("export type WordNavKind");
const end = utils.indexOf("/**\n * Apply background color to a line");
if (start < 0 || end < start) throw new Error("word navigation block not found in omp utils.ts");
const stub = `const segmenter = new Intl.Segmenter(undefined, { granularity: "grapheme" });
export function getSegmenter(): Intl.Segmenter { return segmenter; }
${utils.slice(start, end)}`;
const dir = mkdtempSync(join(tmpdir(), "vim-parity-"));
writeFileSync(join(dir, "utils.ts"), stub);
writeFileSync(
	join(dir, "vim.ts"),
	readFileSync(join(tuiSrc, "vim.ts"), "utf8").replace(`from "./utils"`, `from "./utils.ts"`),
);
const { VimState, lastGraphemeStart } = await import(pathToFileURL(join(dir, "vim.ts")).href);
rmSync(dir, { recursive: true });

const buffers = [
	"alfa beta gamma",
	"one two three\nfour five six\n\nseven eight\nnine",
	'call(foo(bar), "baz qux") [x] {y} <z>',
	"  indented line\n\tTabbed\nlast.",
	"héllo wörld ünïcode 日本語テキスト x",
	"say \"hi there\" and 'yo' ok \\\"esc\\\" end",
	"fn x() {\n  a(1,\n    b[2]);\n}",
	"para one\npara one b\n\n\npara two\n\npara three",
	"snake_case kebab-case it's 3.14 foo.bar(baz)",
	"",
	"a",
	"x  y\n\nz",
	"abc日本語def テスト x",
];

const sequences = `h l j k 0 ^ $ w b e 3w 2b 2e gg G 2G 5gg % 50% 100% 200% { } 2} 2{ fa Fa ta Ta fa; fa;, ta; ta, Ta; 2fa f<sp> fe fe; Fe,
x 3x 20x dw d2w 2dw 3dw cw ce de db d$ d0 D 2D C dd 2dd 9dd yy yw y$ cc dj dk yj yk >> << 2>> >j <k J 3J dJ p P 2p u
rx 3rx 30rx r<esc> R Rx diw daw d2aw d3iw ciw yiw caw diW daW di( da( dib ci" ca" da" di' ca' di\` di[ da] di{ daB ci< da>
dip dap yap cip viw vaw vip vap v vl vly vjd Vd Vj> Vk< vjJ vlo vlov vv vV Vv Vy vex vec vlld v$y vjy vs vx vi( va" vgg vG
i a I A o O <esc> 2<esc> d<esc> di<esc> f<esc> jjk $j $jk jjkk 3j 10k ll$jj llljjkk d% y% c% g gx dg 5% d} y{ c} df<sp>
dt<sp> dT<sp> dF<sp> ; , q z 2d3w 2y2w >w <j >G d>> d< 0x $x ^d$ 2u 3p 2P vd vy vc vo v> v< vJ V> V< VJ Vjd Vjjy vjjc
wwhhll eee bbb wd bde cwX ciwX ea fbfb tt e2fa ;; ,, l;l, 2; 3, dfa dta d;  c2fa yfa vfa vta v2e vwd
$b $bb $bbb $2b $e $w 4w 6w 9w 4b 4e lllllljhj llllllj0j llllllj$j lllllljwj lllllljkj f)di( 2f)di( f)da( 3f)dib %di( $di( $da" vjjiw vipd vjip`
	.split(/\s+/)
	.filter(Boolean);

function tokens(seq) {
	const out = [];
	for (let i = 0; i < seq.length; ) {
		if (seq.startsWith("<esc>", i)) {
			out.push("escape");
			i += 5;
		} else if (seq.startsWith("<sp>", i)) {
			out.push(" ");
			i += 4;
		} else {
			const cp = String.fromCodePoint(seq.codePointAt(i));
			out.push(cp);
			i += cp.length;
		}
	}
	return out;
}

const byteCol = (lines, line, col) => {
	if (col === Number.POSITIVE_INFINITY) return -1;
	const text = lines[line] ?? "";
	if (col > text.length) return Buffer.byteLength(text) + (col - text.length);
	return Buffer.byteLength(text.slice(0, col));
};
const utf16Col = (text, bytes) => {
	let col = 0;
	while (col < text.length && Buffer.byteLength(text.slice(0, col)) < bytes) col++;
	return col;
};
const p = (lines, pos) => [pos.line, byteCol(lines, pos.line, pos.col)];

function convert(lines, c) {
	switch (c.kind) {
		case "move":
			return { kind: c.kind, to: p(lines, c.to) };
		case "yank":
			return { kind: c.kind, from: p(lines, c.from), to: p(lines, c.to), linewise: c.linewise };
		case "delete":
			return { kind: c.kind, from: p(lines, c.from), to: p(lines, c.to), linewise: c.linewise, insert: c.insert };
		case "replace":
			return { kind: c.kind, from: p(lines, c.from), to: p(lines, c.to), text: c.text };
		default:
			return c;
	}
}

function run(text, line, bcol, seq, mode) {
	const lines = text.split("\n");
	const vim = new VimState();
	vim.mode = mode;
	let cursorLine = line;
	let cursorCol = utf16Col(lines[line], bcol);
	const steps = [];
	for (const key of tokens(seq)) {
		const cmds = vim.handleKey(key, { lines, cursorLine, cursorCol });
		if (cmds) {
			for (const c of cmds) {
				if (c.kind !== "move") continue;
				cursorLine = Math.max(0, Math.min(c.to.line, lines.length - 1));
				cursorCol = Math.max(0, Math.min(c.to.col, lines[cursorLine].length));
			}
			if (vim.mode === "normal") cursorCol = Math.min(cursorCol, lastGraphemeStart(lines[cursorLine]));
		}
		steps.push({
			key,
			cmds: cmds === null ? null : cmds.map((c) => convert(lines, c)),
			mode: vim.mode,
			pending: vim.pendingText,
			anchor: vim.anchor ? p(lines, vim.anchor) : null,
			cursor: [cursorLine, byteCol(lines, cursorLine, cursorCol)],
		});
	}
	return { text, cursor: [line, bcol], mode, keys: seq, steps };
}

const cases = [];
for (const text of buffers) {
	const lines = text.split("\n");
	const mid = lines[Math.min(1, lines.length - 1)];
	const cursors = [
		[0, 0],
		[Math.min(1, lines.length - 1), Buffer.byteLength(mid.slice(0, Math.floor(mid.length / 2)))],
	];
	for (const [line, bcol] of cursors) for (const seq of sequences) cases.push(run(text, line, bcol, seq, "normal"));
}
// Insert mode hands keys back; Escape lands Normal one grapheme left.
for (const [text, col] of [["abc", 3], ["héllo", 6], ["", 0]]) cases.push(run(text, 0, col, "a<esc>x", "insert"));

writeFileSync(join(here, "vim-parity.json"), `${JSON.stringify({ source: "oh-my-pi packages/tui/src/vim.ts", cases })}\n`);
console.log(`${cases.length} cases`);
