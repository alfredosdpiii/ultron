// Generates crates/tui/fixtures/editor-autocomplete-parity.json from packages/tui (TypeScript, this checkout):
// - provider: CombinedAutocompleteProvider.getSuggestions over a recorded temp file tree (readdir path completion,
//   `@` fuzzy search through fd) and shouldTriggerFileCompletion;
// - editor: Editor driven by scripted keys and mouse events with that provider (plus `#`/`$` trigger characters):
//   after each event and after the debounce settles, the lines, cursor (UTF-16), whether a debounced request was
//   pending, whether the picker shows, the rendered lines, onChange texts and submissions;
// - select: SelectList rendering with a custom truncatePrimary and the slash-command column bounds;
// - collate: String.prototype.localeCompare order over file-name-like strings.
// Usage: node crates/tui/fixtures/generate-editor-autocomplete.mjs   (deterministic; output is written next to this file)
import { execFileSync } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
const here = dirname(fileURLToPath(import.meta.url));
const src = (p) => import(join(here, "../../../packages/tui/src", p));
const { CombinedAutocompleteProvider } = await src("autocomplete.ts");
const { Editor } = await src("components/editor.ts");
const { SelectList } = await src("components/select-list.ts");
const kb = await src("keybindings.ts");
kb.setKeybindings(new kb.KeybindingsManager(kb.TUI_KEYBINDINGS));
delete process.env.WT_SESSION;

const fdPath = execFileSync("sh", ["-c", "command -v fd"], { encoding: "utf-8" }).trim();

// The tree: paths ending in "/" are directories; "->" entries are symlinks.
const tree = [
	"src/", "src/main.ts", "src/main.test.ts", "src/my dir/", "src/my dir/file.ts", "src/utils/", "src/utils/strings.ts",
	"src/Util.ts", "docs/", "docs/guide.md", "docs/main-notes.md", "README.md", "readme-old.md", ".hidden", "_private.ts",
	"日本/", "日本/メモ.txt", "a b.txt", "Zeta.txt", "zeta-2.txt", "link-src -> src", "deep/", "deep/nested/",
	"deep/nested/main.rs",
];
const base = mkdtempSync(join(tmpdir(), "ac-tree-"));
for (const entry of tree) {
	const [p, target] = entry.split(" -> ");
	if (target) symlinkSync(target, join(base, p));
	else if (p.endsWith("/")) mkdirSync(join(base, p), { recursive: true });
	else writeFileSync(join(base, p), "");
}

const commands = [
	{ name: "model", description: "Switch the active model for this session", argumentHint: "<name>",
		getArgumentCompletions: (p) => ["alpha", "beta", "gamma"].filter((v) => v.startsWith(p)).map((v) => ({ value: v, label: v })) },
	{ name: "settings", description: "Open settings" },
	{ name: "session", description: "Show session info and stats" },
	{ name: "skill:review", description: "Review code" },
	{ name: "a-very-long-command-name-that-overflows", description: "Long one" },
	{ name: "quit", description: "Exit" },
	{ name: "export" },
];
const combined = new CombinedAutocompleteProvider(commands, base, fdPath);
const tags = ["bug", "build", "feature", "docs"];
const vars = ["HOME", "HOST", "PATH"];
// A provider with extra trigger characters: `#tag` and `$VAR` items, everything else from the combined provider.
const provider = {
	triggerCharacters: ["$", "ab", "/", " ", "#"],
	async getSuggestions(lines, line, col, options) {
		const before = (lines[line] ?? "").slice(0, col);
		const m = /([#$])(\w*)$/.exec(before);
		if (m) {
			const list = m[1] === "#" ? tags : vars;
			const items = list.filter((v) => v.toLowerCase().startsWith(m[2].toLowerCase())).map((v) => ({ value: m[1] + v, label: v }));
			return items.length ? { items, prefix: m[0] } : null;
		}
		return combined.getSuggestions(lines, line, col, options);
	},
	applyCompletion: (...a) => combined.applyCompletion(...a),
	shouldTriggerFileCompletion: (...a) => combined.shouldTriggerFileCompletion(...a),
};

const signal = new AbortController().signal;
const suggestionCases = [
	["./", false], ["src/", false], ["src/m", false], ["./src/../RE", false], [".h", false], ['"src/my', false],
	["see src/my dir/", false], ["x = src/", false], ["text ", false], ["", false], ["", true], ["REA", true], ["ze", true],
	["日本/", false], ["日本。src/", false], ["deep/nested/", false], ["link-src/", false], ["src/utils", false],
	["@src/m", false], ["@", false], ['@"src/my', false], ["@main", false], ["@utils/", false], ["@src/", false],
	["@nomatch", false], ["@deep/nested/m", false], ["@MAIN", false], ["@.hid", false], ["@zeta", false],
	["/mo", false], ["/model g", false], ["/mo", true], ["<base>/sr", false], ["@<base>/docs/g", false],
	['say "./src/m', false], ["path=./do", false],
];
const suggestions = [];
for (const [text0, force] of suggestionCases) {
	const text = text0.replaceAll("<base>", base);
	const result = await combined.getSuggestions([text], 0, text.length, { signal, force });
	const norm = (s) => s.split(base).join("<base>");
	suggestions.push({ text: text0, force, result: result && JSON.parse(norm(JSON.stringify(result))) });
}
const shouldTrigger = ["/mo", "  /mo", "/model x", "src/", "", " / "].map((text) => ({ text, result: combined.shouldTriggerFileCompletion([text], 0, text.length) }));

const theme = {
	borderColor: (s) => `<${s}>`,
	selectList: { selectedPrefix: (s) => s, selectedText: (s) => `[${s}]`, description: (s) => `{${s}}`, scrollInfo: (s) => `(${s})`, noMatch: (s) => `!${s}` },
};
const K = { up: "\x1b[A", down: "\x1b[B", left: "\x1b[D", right: "\x1b[C", enter: "\r", shiftEnter: "\x1b\r", tab: "\t", esc: "\x1b",
	bs: "\x7f", del: "\x1b[3~", killWord: "\x17", killEnd: "\x0b", yank: "\x19", undo: "\x1f", home: "\x01", ctrlC: "\x03",
	paste: (s) => `\x1b[200~${s}\x1b[201~` };
const type = (s) => [...s];
const mouse = (type, y, x = 2, extra = {}) => ({ mouse: { type, button: type === "wheel" ? "none" : "left", x, y, ...extra } });

const scripts = [
	{ name: "slash-tab", keys: [...type("/se"), K.down, K.tab, ...type("x")] },
	{ name: "slash-enter-submits", keys: [...type("/mo"), K.enter] },
	{ name: "slash-args", keys: [...type("/model "), ...type("b"), K.tab, K.enter] },
	{ name: "slash-esc-backspace", keys: [...type("/set"), K.esc, K.bs, K.up, K.up, K.down, K.esc, K.up] },
	{ name: "slash-tab-explicit", keys: [...type("/sk"), K.esc, K.tab, K.down, K.enter] },
	{ name: "slash-skill-prefix", keys: [...type("/rev"), K.tab] },
	{ name: "slash-move-cursor", keys: [...type("/model al"), K.left, K.left, K.left, K.left, K.right, K.right] },
	{ name: "slash-second-line", keys: [...type("abc"), K.shiftEnter, ...type("/se"), K.tab] },
	{ name: "slash-overflow", keys: [...type("/"), K.down, K.down, K.down, K.down, K.down, K.down] },
	{ name: "slash-none", keys: [...type("/zzz"), K.bs, K.bs, K.bs] },
	{ name: "at-fuzzy", keys: [...type("see @ma"), K.down, K.enter, ...type("x")] },
	{ name: "at-dir-continue", keys: [...type("@src/"), K.tab, ...type("ut"), K.tab] },
	{ name: "at-quoted", keys: [...type('@"src/my'), K.tab] },
	{ name: "at-scroll", keys: [...type("@"), K.down, K.down, K.down, K.down, K.down, K.down, K.down, K.up, K.esc] },
	{ name: "at-cjk-boundary", keys: [...type("日本。@zeta"), K.tab] },
	{ name: "at-mid-word", keys: [...type("mail@ma"), K.tab] },
	{ name: "at-then-space", keys: [...type("@ma "), ...type("x")] },
	{ name: "hash-tags", keys: [...type("fix #b"), K.down, K.tab, ...type(" #"), K.esc, ...type("d"), K.enter] },
	{ name: "dollar-trigger", keys: [...type("echo $H"), K.down, K.enter] },
	{ name: "ignored-trigger", keys: [...type("ab x")] },
	{ name: "tab-path", keys: [...type("open src/"), K.tab, K.down, K.tab] },
	{ name: "tab-single", keys: [...type("open REA"), K.tab, ...type("x"), K.undo] },
	{ name: "tab-dot", keys: [...type("cat ./s"), K.tab, K.tab] },
	{ name: "tab-force-update", keys: [...type("ls "), K.tab, ...type("d"), ...type("o"), K.bs, K.bs, K.bs, K.esc] },
	{ name: "tab-empty", keys: [K.tab, K.down, K.enter] },
	{ name: "tab-no-match", keys: [...type("cat qqq"), K.tab] },
	{ name: "paste-closes", keys: [...type("/se"), K.paste("tt"), ...type("x")] },
	{ name: "history-closes", history: ["older"], keys: [...type("/se"), K.esc, K.bs, K.bs, K.bs, K.up] },
	{ name: "change-events", history: ["one", "two"], keys: [...type("ab cd"), K.bs, K.killWord, K.yank, K.killEnd, K.home, K.del, K.undo, K.undo, K.up, K.up, K.down, K.down, K.shiftEnter, K.enter, K.ctrlC] },
	{ name: "mouse-picker", keys: [...type("/s"), mouse("wheel", 3, 2, { wheelDelta: 1 }), mouse("wheel", 3, 2, { wheelDelta: -3 }), mouse("wheel", 3, 2, { wheelDelta: -1 }), mouse("wheel", 3, 2, { wheelDelta: 0 }), mouse("press", 4), mouse("click", 4), ...type("z")] },
	{ name: "mouse-picker-hover", keys: [...type("/s"), mouse("move", 4), mouse("press", 9), mouse("click", 3)] },
	{ name: "mouse-text", keys: [...type("hello world this is a long line that wraps around"), mouse("click", 1, 3), mouse("click", 2, 40), mouse("click", 1, 99), mouse("press", 1, 1), mouse("click", 0, 1), mouse("click", 7, 1)] },
	{ name: "mouse-update-picker", keys: [...type("/model al"), mouse("click", 1, 4), mouse("click", 1, 9)] },
];

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const records = [];
for (const script of scripts) {
	for (const [width, paddingX, maxVisible] of [[30, 0, 5], [64, 1, 4], [100, 2, 5]]) {
		const tui = { terminal: { rows: 20 }, requestRender() {} };
		const editor = new Editor(tui, theme, { paddingX, autocompleteMaxVisible: maxVisible });
		editor.setAutocompleteProvider(provider);
		const changes = [];
		const submits = [];
		editor.onChange = (t) => changes.push(t);
		editor.onSubmit = (t) => submits.push(t);
		for (const h of [...(script.history ?? [])].reverse()) editor.addToHistory(h);
		editor.render(width);
		changes.length = 0;
		const steps = [];
		for (const k of script.keys) {
			let mouseResult = null;
			if (typeof k === "string") editor.handleInput(k);
			else {
				const ev = { ...k.mouse, screenX: k.mouse.x, screenY: k.mouse.y + 10, width, height: 12, shift: false, alt: false, ctrl: false };
				mouseResult = editor.handleMouse(ev) ?? null;
			}
			const pending = editor.autocompleteDebounceTimer !== undefined;
			// Settle: let the debounce timer fire, then wait for the request chain (fd runs) to finish.
			while (editor.autocompleteDebounceTimer !== undefined) await sleep(5);
			await editor.autocompleteRequestTask;
			await sleep(0);
			const c = editor.getCursor();
			steps.push({ key: k, pending, mouseResult, lines: editor.getLines(), line: c.line, col: c.col,
				showing: editor.isShowingAutocomplete(), render: editor.render(width), changes: changes.splice(0) });
		}
		records.push({ name: script.name, width, paddingX, maxVisible, history: script.history ?? [], steps, submits });
	}
}

// SelectList with a custom truncatePrimary (records the context it gets) and the slash-command column bounds.
const selectItems = [
	{ value: "short", label: "short", description: "first" },
	{ value: "medium-length-value", label: "medium-length-value", description: "second\nline" },
	{ value: "x".repeat(40), label: "", description: "unlabeled long value" },
	{ value: "plain", label: "plain" },
];
const select = [];
for (const layout of [{ minPrimaryColumnWidth: 12, maxPrimaryColumnWidth: 32 }, { minPrimaryColumnWidth: 20 }, { maxPrimaryColumnWidth: 8 }, { minPrimaryColumnWidth: 30, maxPrimaryColumnWidth: 10 }]) {
	for (const custom of [false, true]) {
		for (const width of [30, 41, 60, 90]) {
			const contexts = [];
			const opts = { ...layout };
			if (custom) opts.truncatePrimary = (ctx) => {
				contexts.push({ text: ctx.text, maxWidth: ctx.maxWidth, columnWidth: ctx.columnWidth, value: ctx.item.value, isSelected: ctx.isSelected });
				return ctx.isSelected ? `*${ctx.text}*` : `${ctx.text.slice(0, 3)}~`;
			};
			const list = new SelectList(selectItems, 3, theme.selectList, opts);
			list.setSelectedIndex(1);
			select.push({ layout, custom, width, render: list.render(width), contexts });
		}
	}
}

const corpus = ["a-b", "a_b", "a.b", "ab", "Ab", "aB", "AB", "a b", "a1", "a10", "a2", "f", "foo.ts", "foo-bar.ts", "foo_bar.ts",
	"Foo.ts", "README.md", "readme.md", "readme-old.md", ".git", ".hidden", "_x", "1x", "x/", "x.ts/", "dir/", "Dir/", "my dir/",
	"main.ts", "main.test.ts", "Main.ts", "~home", "$var", "#tag", "@at", "a(1)", "a[1]", "a{1}", "a+b", "a=b", "a,b", "a;b",
	"zeta-2.txt", "Zeta.txt", "", "a", "A", "aa", "aA", "Aa"];
const collate = [...corpus].sort((a, b) => a.localeCompare(b));

rmSync(base, { recursive: true, force: true });
writeFileSync(join(here, "editor-autocomplete-parity.json"), `${JSON.stringify({ tree, fdPath, suggestions, shouldTrigger, records, select, corpus, collate })}\n`);
console.log(`suggestions=${suggestions.length} records=${records.length} select=${select.length} collate=${collate.length}`);
