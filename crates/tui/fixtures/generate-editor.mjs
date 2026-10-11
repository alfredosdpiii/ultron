// Generates crates/tui/fixtures/editor-parity.json by running packages/tui (TypeScript, this checkout) on scripted
// inputs: matchesKey over key sequences x key ids (Kitty on/off), keybinding definitions per platform and the
// manager's resolution, fuzzyMatch/fuzzyFilter, word navigation, and the Input component's state and render after
// each key of scripted sequences at several widths.
// Usage: node crates/tui/fixtures/generate-editor.mjs   (deterministic; output is written next to this file)
import { writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, "../../..");
delete process.env.WT_SESSION;
const src = (p) => import(join(root, "packages/tui/src", p));
const keys = await src("keys.ts");
const kbMod = await src("keybindings.ts");
const fuzzy = await src("fuzzy.ts");
const words = await src("word-navigation.ts");
const { Input } = await src("components/input.ts");

const keyIds = [];
const bases = ["escape", "esc", "space", "tab", "enter", "return", "backspace", "insert", "delete", "clear", "home", "end",
	"pageUp", "pageDown", "up", "down", "left", "right", "f1", "f5", "f12", "a", "c", "j", "z", "0", "9", "-", "[", "]", "\\",
	"_", "/", ";", "'", "`", "=", ".", "?", "!"];
for (const b of bases) for (const m of ["", "shift+", "ctrl+", "alt+", "super+", "ctrl+alt+", "shift+ctrl+", "ctrl+shift+alt+"]) keyIds.push(m + b);
keyIds.push("CTRL+C", "Shift+Tab", "", "ctrl+", "ctrl+ab", "é");

const inputs = [];
for (let c = 0; c < 128; c++) inputs.push(String.fromCharCode(c), `\x1b${String.fromCharCode(c)}`);
for (const s of ["A", "B", "C", "D", "H", "F", "E", "Z", "a", "b", "c", "d", "e", "1~", "2~", "3~", "4~", "5~", "6~", "7~", "8~",
	"[5~", "[6~", "[A", "[E", "11~", "15~", "17~", "24~", "2$", "3$", "5$", "8$", "2^", "3^", "6^", "7^", "1;3D", "1;5D", "1;3C", "1;5C"]) inputs.push(`\x1b[${s}`);
for (const s of "ABCDHFEPQRSMabcde") inputs.push(`\x1bO${s}`);
inputs.push("\x1b\r", "\x1b\x7f", "\x1b\b", "\x1b ", "\x1bB", "\x1bF");
for (const m of [1, 2, 3, 5, 6, 7, 9, 65, 69]) {
	for (const f of "ABCDHF") inputs.push(`\x1b[1;${m}${f}`);
	for (const n of [2, 3, 5, 6]) inputs.push(`\x1b[${n};${m}~`);
	for (const cp of [9, 13, 27, 32, 45, 47, 48, 65, 91, 97, 99, 106, 122, 127, 1089, 57414, 57417, 57426]) {
		inputs.push(`\x1b[${cp};${m}u`, `\x1b[27;${m};${cp}~`);
	}
	inputs.push(`\x1b[1089::99;${m}u`, `\x1b[97::99;${m}u`, `\x1b[47::91;${m}u`);
}
inputs.push("é", "日", "");
const matchKey = [];
for (const kitty of [false, true]) {
	keys.setKittyProtocolActive(kitty);
	for (const data of inputs) matchKey.push([kitty, data, keyIds.filter((k) => keys.matchesKey(data, k))]);
}
keys.setKittyProtocolActive(false);

const defsOf = (defs) => Object.entries(defs).map(([id, d]) => [id, [d.defaultKeys].flat(), d.description ?? ""]);
const platforms = [];
const realPlatform = process.platform;
for (const [platform, wsl] of [["linux", false], ["linux", true], ["darwin", false], ["win32", false]]) {
	Object.defineProperty(process, "platform", { value: platform });
	if (wsl) process.env.WSL_DISTRO_NAME = "Ubuntu"; else delete process.env.WSL_DISTRO_NAME;
	delete process.env.WSL_INTEROP;
	const app = await import(join(root, "packages/coding-agent/src/core/keybindings.ts") + `?p=${platform}${wsl}`);
	platforms.push([platform, wsl, defsOf(app.KEYBINDINGS)]);
}
Object.defineProperty(process, "platform", { value: realPlatform });
delete process.env.WSL_DISTRO_NAME;

const userCases = [
	[],
	[["tui.editor.undo", ["ctrl+z", "ctrl+z"]], ["tui.editor.yank", "ctrl+z"], ["nope.unknown", "ctrl+z"], ["tui.select.up", []]],
	[["tui.input.submit", ["enter", "ctrl+m"]], ["tui.select.confirm", ["ctrl+m", "enter"]], ["tui.editor.cursorUp", undefined]],
];
const manager = userCases.map((user) => {
	const m = new kbMod.KeybindingsManager(kbMod.TUI_KEYBINDINGS, Object.fromEntries(user));
	return [user.map(([id, k]) => [id, k === undefined ? null : [k].flat()]),
		Object.entries(m.getResolvedBindings()).map(([id, k]) => [id, [k].flat()]),
		m.getConflicts().map((c) => [c.key, c.keybindings]),
		["\x1a", "\r", "\x1b[A", "\x19", "\x1f"].map((d) => [d, Object.keys(kbMod.TUI_KEYBINDINGS).filter((id) => m.matches(d, id))])];
});

let seed = 0x1234567;
const rand = (n) => { seed ^= seed << 13; seed >>>= 0; seed ^= seed >>> 17; seed ^= seed << 5; seed >>>= 0; return seed % n; };
const names = ["src/components/editor.ts", "packages/tui/src/fuzzy.ts", "README.md", "docs/rust-rewrite/ledger.tsv", "Cargo.toml",
	"crates/tui/src/keys.rs", "app.model.cycleForward", "tui.editor.cursorWordLeft", "/compact", "/model", "abc123", "123abc", "x-y_z.w:v", "Ελληνικά", "日本語"];
const queries = ["", "ed", "edt", "tui", "src ed", "rs", "key", "abc", "123", "c12", "12c", "abc123", "123abc", "cargo", "MODEL", "/m", "x y", "zz", "ελ", "日本", "a.b", "src/ed", "  "];
const fuzzyCases = [];
for (const q of queries) for (const t of names) { const r = fuzzy.fuzzyMatch(q, t); fuzzyCases.push([q, t, r.matches, String(r.score)]); }
const filterCases = queries.map((q) => [q, fuzzy.fuzzyFilter(names, q, (x) => x)]);

const wordTexts = ["hello world", "  foo.bar(baz)  qux", "path/to/file.ts", "日本語 テキスト", "a--b  c", "x=1; y=2;", "café au lait", "emoji 😀 test", "", "...", "snake_case_word camelCase", "tab\there"];
const wordCases = [];
for (const t of wordTexts) for (let c = 0; c <= t.length; c++) {
	if (c > 0 && c < t.length && /[\uDC00-\uDFFF]/.test(t[c])) continue;
	wordCases.push([t, c, words.findWordBackward(t, c), words.findWordForward(t, c)]);
}

// Input: scripted sequences, state and render after each step.
const K = { left: "\x1b[D", right: "\x1b[C", home: "\x01", end: "\x05", bs: "\x7f", del: "\x1b[3~", cw: "\x17", ab: "\x1b\x7f",
	ad: "\x1bd", cu: "\x15", ck: "\x0b", cy: "\x19", ay: "\x1by", undo: "\x1f", wl: "\x1bb", wr: "\x1bf", cl: "\x1b[1;5D", enter: "\r", esc: "\x1b" };
const scripts = [
	[{}, ["h", "e", "l", "l", "o", " ", "w", "o", "r", "l", "d", K.left, K.left, "X", K.home, "Y", K.end, K.bs, K.del, K.left, K.del, K.undo, K.undo, K.undo, K.enter]],
	[{}, [..."the quick brown fox jumps", K.cw, K.cw, K.cy, K.home, K.ad, K.ad, K.end, K.cy, K.ay, K.ay, K.undo, K.wl, K.wl, K.wr, K.cl, K.cu, K.ck, K.cy, K.esc]],
	[{ prompt: "$ " }, ["\x1b[200~multi\nline\tpaste\r\n end\x1b[201~", K.home, K.wr, K.wr, "日本語", "😀", K.left, K.bs, K.left, K.left, "\x1b[97u", "\x1b[65;2u", "\x1b[27;2;65~", "\x1b[200~par", "tial\x1b[201~tail"]],
	[{}, [..."one two three", K.undo, K.undo, ..." aa bb cc", K.cw, K.left, K.cw, K.left, K.cw, K.end, K.cy, K.ay, K.ay, K.ay, K.undo]],
	[{ placeholder: "Type here…" }, ["", "a", K.bs, "b"]],
	[{ mask: "•" }, [..."secret", K.left, K.left, K.bs]],
	[{}, [..."abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGHIJ", K.home, ...Array(20).fill(K.right), ...Array(5).fill(K.left), K.end, K.wl]],
];
const inputCases = [];
for (const [opts, seq] of scripts) {
	for (const width of [1, 2, 3, 8, 13, 30, 60]) {
		for (const focused of [false, true]) {
			const input = new Input(opts);
			input.focused = focused;
			const events = [];
			input.onSubmit = (v) => events.push(["submit", v]);
			input.onEscape = () => events.push(["escape"]);
			const steps = [];
			for (const data of seq) {
				events.length = 0;
				input.handleInput(data);
				steps.push([data, input.getValue(), input.cursor, input.render(width), [...events]]);
			}
			inputCases.push([opts, width, focused, steps]);
		}
	}
}

const out = { keyIds, matchKey, tuiKeybindings: defsOf(kbMod.TUI_KEYBINDINGS), platforms, manager, fuzzyCases, filterCases, wordCases, inputCases };
writeFileSync(join(here, "editor-parity.json"), `${JSON.stringify(out)}\n`);
console.log(`matchKey ${matchKey.length}, fuzzy ${fuzzyCases.length}, words ${wordCases.length}, input ${inputCases.length}`);
