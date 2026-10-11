// Generates crates/tui/fixtures/editor-component-parity.json by running packages/tui's Editor (TypeScript, this
// checkout) on scripted key sequences at several widths, recording text, cursor (UTF-16 column), submissions and
// the rendered lines after every key; plus wordWrapLine and isKeyRelease/isKeyRepeat cases.
// Usage: node crates/tui/fixtures/generate-editor-component.mjs   (deterministic; output is written next to this file)
import { writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, "../../..");
delete process.env.WT_SESSION;
const src = (p) => import(join(root, "packages/tui/src", p));
const { Editor, wordWrapLine } = await src("components/editor.ts");
const keys = await src("keys.ts");
const kb = await src("keybindings.ts");
kb.setKeybindings(new kb.KeybindingsManager(kb.TUI_KEYBINDINGS));

const theme = {
	borderColor: (s) => `<${s}>`,
	selectList: { selectedPrefix: (s) => s, selectedText: (s) => s, description: (s) => s, scrollInfo: (s) => s, noMatch: (s) => s },
};

const K = {
	up: "\x1b[A", down: "\x1b[B", right: "\x1b[C", left: "\x1b[D", home: "\x01", end: "\x05", enter: "\r",
	shiftEnter: "\x1b\r", bs: "\x7f", del: "\x1b[3~", wordLeft: "\x1bb", wordRight: "\x1bf", killWord: "\x17",
	killWordFwd: "\x1bd", killEnd: "\x0b", killStart: "\x15", yank: "\x19", yankPop: "\x1by", undo: "\x1f",
	pgUp: "\x1b[5~", pgDn: "\x1b[6~", jumpF: "\x1d", jumpB: "\x1b\x1d", esc: "\x1b", tab: "\t", ctrlC: "\x03",
	paste: (s) => `\x1b[200~${s}\x1b[201~`,
};
const type = (s) => [...s];
const long = Array.from({ length: 12 }, (_, i) => `line ${i}`).join("\n");

const scripts = [
	{ name: "type-and-submit", keys: [...type("hello world"), K.left, K.left, "X", K.enter] },
	{ name: "wrap-long", keys: [...type("the quick brown fox jumps over the lazy dog again and again"), K.up, K.up, K.down, K.home, K.right, K.end] },
	{ name: "multiline-nav", keys: [...type("first line here"), K.shiftEnter, ...type("ab"), K.shiftEnter, ...type("third long line"), K.up, K.up, K.down, K.down, K.up, K.left, K.up] },
	{ name: "sticky-col", keys: [...type("aaaaaaaaaaaa"), K.shiftEnter, ...type("bb"), K.shiftEnter, ...type("cccccccccccc"), K.up, K.up, K.down, K.down] },
	{ name: "kill-yank", keys: [...type("one two three four"), K.killWord, K.killWord, K.end, K.yank, K.home, K.killEnd, K.yank, K.killWord, K.yank, K.yankPop, K.undo, K.undo] },
	{ name: "kill-lines", keys: [...type("abc"), K.shiftEnter, ...type("def"), K.home, K.killStart, K.bs, K.killEnd, K.killEnd, K.yank, K.wordLeft, K.killWordFwd, K.del, K.home, K.killWord] },
	{ name: "undo-coalesce", keys: [...type("foo bar baz"), K.undo, K.undo, ...type("qq"), K.undo, K.undo, K.undo, K.undo] },
	{ name: "word-nav", keys: [...type("foo.bar(baz) qux  end"), K.wordLeft, K.wordLeft, K.wordLeft, K.wordLeft, K.wordRight, K.wordRight, K.home, K.wordLeft, K.wordRight] },
	{ name: "jump", keys: [...type("abcabc"), K.shiftEnter, ...type("xbz"), K.jumpB, "b", K.jumpB, "b", K.jumpB, "a", K.jumpF, "z", K.jumpF, K.jumpF, "q", K.jumpF, K.left, K.home, K.jumpB, "x"] },
	{ name: "jump-start", keys: [...type("abc"), K.home, K.jumpB, "a", K.jumpF, "a"] },
	{ name: "paste-small", keys: [...type("ab"), K.paste("x\ty\r\nz"), ...type("cd"), K.undo] },
	{ name: "paste-path", keys: [...type("see"), K.paste("/tmp/file.txt")] },
	{ name: "paste-large", keys: [...type("pre "), K.paste(long), ...type(" mid "), K.paste("y".repeat(1200)), K.left, K.bs, K.left, K.left, K.right, K.wordLeft, K.wordLeft, K.bs, K.enter] },
	{ name: "paste-marker-nav", keys: [K.paste(long), K.home, K.right, K.del, K.undo, K.wordRight, K.wordLeft, K.end, K.up, K.down] },
	{ name: "backslash-enter", keys: [...type("abc\\"), K.enter, ...type("d"), K.enter] },
	{ name: "page-scroll", keys: [...Array.from({ length: 14 }, (_, i) => [...type(`row ${i}`), K.shiftEnter]).flat(), ...type("last"), K.pgUp, K.pgUp, K.pgDn, K.up, K.up, K.pgDn, K.pgDn] },
	{ name: "unicode", keys: [...type("héllo 世界世界世界 😀👍🏽 done"), K.left, K.left, K.left, K.left, K.left, K.left, K.left, K.bs, K.up, K.down, K.end, K.wordLeft, K.del, K.end] },
	// Word motion inside Han runs is excluded: ICU segments Han with a dictionary, editing.rs merges each run.
	{ name: "cjk-wrap", keys: [...type("日本語のテキストを折り返すテストです。もっと長い文章を書きます"), K.up, K.home, K.down, K.down] },
	{ name: "history", history: ["first entry", "second\nmultiline", "third"], keys: [K.up, K.up, K.up, K.up, K.down, K.down, K.down, K.down, ...type("draft"), K.home, K.up, K.down, K.end, K.up] },
	{ name: "history-down-end", history: ["older long text that wraps around the editor width", "newest"], keys: [...type("x"), K.up, K.home, K.up, K.up, K.up, K.down, K.down, K.down] },
	{ name: "ctrl-c-tab-esc", keys: [...type("ab"), K.ctrlC, K.tab, K.esc, K.right] },
	{ name: "jump-back-col0", keys: [...type("a"), K.shiftEnter, ...type("abc"), K.home, K.jumpB, "a", ...type("z")] },
	{ name: "paste-ten-lines", keys: [K.paste(long.split("\n").slice(0, 10).join("\n")), K.paste(long.split("\n").slice(0, 11).join("\n"))] },
	{ name: "paste-renumber-literal", keys: [...type("[paste #1] "), K.paste(long), K.paste(long), K.left, K.bs, K.end, K.bs] },
	{ name: "snap-surrogate", keys: [...type("ab😀"), K.shiftEnter, ...type("abc"), K.up, K.down, K.up, ...type("x")] },
	{ name: "history-dupes", history: ["same", "same", "other"], keys: [K.up, K.up, K.up, K.down] },
	{ name: "history-up-mid", history: ["a1", "b2", "c3"], keys: [K.up, K.up, K.up, K.down, K.up] },
	{ name: "scroll-border-12", widths: [10, 11, 12, 13], keys: [...Array.from({ length: 8 }, (_, i) => [...type(`r${i}`), K.shiftEnter]).flat(), K.pgUp, K.down] },
	{ name: "spaces-wrap", keys: [...type("word      spaced      out     words here and there"), K.up, K.end, K.down] },
];

const widths = [8, 14, 30, 60];
const records = [];
for (const script of scripts) {
	for (const width of script.widths ?? widths) {
		for (const focused of width === 14 ? [false, true] : [false]) {
			const tui = { terminal: { rows: 20 }, requestRender() {} };
			const editor = new Editor(tui, theme, { paddingX: width === 30 ? 1 : 0 });
			editor.focused = focused;
			const submits = [];
			editor.onSubmit = (t) => submits.push(t);
			for (const h of [...(script.history ?? [])].reverse()) editor.addToHistory(h);
			editor.render(width);
			const steps = [];
			for (const k of script.keys) {
				editor.handleInput(k);
				const c = editor.getCursor();
				steps.push({ key: k, lines: editor.getLines(), line: c.line, col: c.col, expanded: editor.getExpandedText(), render: editor.render(width) });
			}
			records.push({ name: script.name, width, focused, paddingX: width === 30 ? 1 : 0, history: script.history ?? [], steps, submits });
		}
	}
}

// Width 1 is omitted: TS wordWrapLine recurses forever on a width-2 grapheme at maxWidth 1.
const wrapCases = [];
const wrapInputs = ["", "short", "hello world foo bar", "aaaaaaaaaaaaaaaaaaaa", "a  b   c    d", "日本語のテキスト", "mix 日本 text here", "emoji 😀😀😀 wide", "trailing   ", "   leading spaces here", "x [paste #1 +12 lines] y"];
for (const line of wrapInputs) for (const w of [0, 2, 3, 5, 8, 12]) wrapCases.push({ line, width: w, chunks: wordWrapLine(line, w) });

const eventSeqs = ["a", "\x1b[97u", "\x1b[97;1:2u", "\x1b[97;1:3u", "\x1b[1;1:3A", "\x1b[1;5:2D", "\x1b[3;1:3~", "\x1b[200~90:62:3F\x1b[201~", "\x1b[1;1:2H", "\x1b[1;1:3F", "x:3u", "\x1b[13;2u"];
const events = eventSeqs.map((s) => ({ data: s, release: keys.isKeyRelease(s), repeat: keys.isKeyRepeat(s) }));

writeFileSync(join(here, "editor-component-parity.json"), `${JSON.stringify({ records, wrapCases, events })}\n`);
console.log(`records=${records.length} wrap=${wrapCases.length} events=${events.length}`);
