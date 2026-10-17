// Generates crates/tui/fixtures/fullscreen-scroll-parity.json by driving packages/tui/src/tui-alt-screen.ts
// (TypeScript, this checkout) with the client TUI's fullscreen layout (chat-viewport.ts createChatViewport: a
// primary follow-end ScrollView with an auto scrollbar over a dock of pending/status/editor/footer) on a fake
// terminal, then feeding key, wheel and mouse input: page, half-page, line, top/bottom and previous/next-prompt
// scrolling, wheel (Alt multiplier, horizontal wheel ignored), the auto scrollbar (hover reveal, track jump, thumb
// drag), the jump-to-end indicator, drag selection with copy-on-select, word/line clicks, edge auto-scroll, focus
// loss and flashes. After each step it records the viewport top, follow state, the frame the TUI painted (its
// previousScreen, after line resets) and every copied text.
//
// Timers are virtual: setTimeout/setInterval are replaced before the TUI loads, so nothing depends on wall-clock
// time. `expire` runs the scrollbar-hide and flash timeouts (delays >= 1000 ms), `autoscroll` runs one tick of the
// selection auto-scroll interval; Date.now is a step clock (`at`, ms). Frames are painted by calling doRender after
// each step (the TUI's own render scheduling uses the virtual timers and never fires).
//
// Usage: node crates/tui/fixtures/generate-fullscreen-scroll.mjs   (deterministic; output is written next to this file)
import "../../cli/fixtures/offline-guard.mjs";
import { writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const timers = [];
let nextTimer = 1;
const fakeTimer = (interval) => (fn, ms = 0) => {
	const t = { id: nextTimer++, fn, ms, interval, live: true, unref() { return t; }, ref() { return t; }, hasRef() { return false; } };
	timers.push(t);
	return t;
};
globalThis.setTimeout = fakeTimer(false);
globalThis.setInterval = fakeTimer(true);
globalThis.clearTimeout = globalThis.clearInterval = (t) => {
	if (t) t.live = false;
};
let clock = 1_000_000;
Date.now = () => clock;

const here = dirname(fileURLToPath(import.meta.url));
const src = (p) => import(join(here, "../../../packages/tui/src", p));
const { TuiAltScreen } = await src("tui-alt-screen.ts");
const { ScrollView } = await src("components/scroll-view.ts");
const { VStack } = await src("components/v-stack.ts");
const { KeybindingsManager, TUI_KEYBINDINGS, setKeybindings } = await src("keybindings.ts");
const { setCapabilities } = await src("terminal-image.ts");
setCapabilities({ images: null, trueColor: true, hyperlinks: true });

/** Bindings the scenarios use for the actions without defaults. */
const USER_BINDINGS = {
	"tui.altScreen.halfPageUp": "ctrl+u",
	"tui.altScreen.halfPageDown": "ctrl+d",
	"tui.altScreen.lineUp": "alt+k",
	"tui.altScreen.lineDown": "alt+j",
};
setKeybindings(new KeybindingsManager(TUI_KEYBINDINGS, USER_BINDINGS));

const PROMPT = "\x1b]133;A\x07";
class Lines {
	constructor(lines) {
		this.lines = lines;
	}
	render() {
		return [...this.lines];
	}
	invalidate() {}
}

class FakeTerminal {
	constructor(columns, rows) {
		this.columns = columns;
		this.rows = rows;
		this.writes = [];
		this.kittyProtocolActive = false;
	}
	start(onInput) {
		this.onInput = onInput;
	}
	stop() {}
	async drainInput() {}
	write(data) {
		this.writes.push(data);
	}
	moveBy() {}
	hideCursor() {}
	showCursor() {}
	clearLine() {}
	clearFromCursor() {}
	clearScreen() {}
	setTitle() {}
	setProgress() {}
}

function documentLines(count) {
	return Array.from({ length: count }, (_, i) => {
		const n = i + 1;
		if (n % 10 === 5) return `${PROMPT}> prompt ${n}`;
		if (n === 7) return "path /usr/local-bin/x.sh end";
		if (n === 8) return "\x1b[31mred\x1b[39m and \x1b[1mbold\x1b[22m text";
		if (n === 9) return "日本語 テキスト wide";
		if (n === 12) return "a very long line that runs well past the terminal width";
		return `line ${n}`;
	});
}

/**
 * One scenario: a fresh TUI over `doc` lines, `cols` x `rows`, then `steps`. A step is `{ input }`,
 * `{ grow: n }` (append n document lines; `growAfter` appends after the input), `{ expire: true }`, `{ autoscroll: true }`, `{ at: ms }` (advance the
 * clock), `{ resize: [cols, rows] }`, optionally combined; every step records a frame.
 */
function run({ name, cols = 30, rows = 12, doc = 40, options = {}, copyResult = true, steps }) {
	timers.length = 0;
	clock = 1_000_000;
	const terminal = new FakeTerminal(cols, rows);
	const copies = [];
	const tuiOptions = { ...options };
	if (options.injectCopy !== false) {
		tuiOptions.copySelection = async (text) => {
			copies.push(text);
			return copyResult;
		};
	}
	delete tuiOptions.injectCopy;
	const tui = new TuiAltScreen(terminal, false, undefined, tuiOptions);
	const document = new Lines(documentLines(doc));
	const transcript = new ScrollView(document, { follow: "end", primary: true, overscroll: "chain", scrollbar: "auto" });
	const dock = new VStack([
		{ component: new Lines([]), shrink: 1, minSize: 0 },
		{ component: new Lines(["status"]), shrink: 1, minSize: 0 },
		{ component: new Lines(["────────", "> editor", "────────"]), shrink: 1, minSize: 3 },
		{ component: new Lines(["footer"]), shrink: 1, minSize: 0 },
	]);
	tui.setLayoutRoot(
		new VStack([
			{ component: transcript, basis: 0, grow: 1, shrink: 1, minSize: 1 },
			{ component: dock, basis: "auto", grow: 0, shrink: 1, minSize: 1 },
		]),
	);
	tui.start();
	tui.doRender();
	const frames = [];
	const record = (step) => {
		frames.push({
			step,
			top: transcript.scrollTop,
			following: transcript.isFollowingEnd,
			screen: [...tui.previousScreen],
			copies: [...copies],
			osc52: terminal.writes.filter((w) => w.startsWith("\x1b]52;")),
		});
	};
	record({});
	return (async () => {
		for (const step of steps) {
			if (step.at !== undefined) clock += step.at;
			if (step.resize) {
				terminal.columns = step.resize[0];
				terminal.rows = step.resize[1];
			}
			if (step.grow) document.lines.push(...documentLines(document.lines.length + step.grow).slice(-step.grow));
			if (step.input !== undefined) terminal.onInput(step.input);
			if (step.growAfter) document.lines.push(...documentLines(document.lines.length + step.growAfter).slice(-step.growAfter));
			for (let i = 0; i < 5; i++) await Promise.resolve();
			if (step.autoscroll) {
				for (const t of timers.filter((t) => t.live && t.interval)) t.fn();
			}
			if (step.expire) {
				for (const t of timers.filter((t) => t.live && !t.interval && t.ms >= 1000)) {
					t.live = false;
					t.fn();
				}
			}
			tui.doRender();
			record(step);
		}
		tui.stop();
		return { name, cols, rows, doc, copyOnSelect: options.copyOnSelect ?? true, indicator: !!options.scrollToEndIndicator, copyResult, injectCopy: options.injectCopy !== false, frames };
	})();
}

const key = (input) => ({ input });
const sgr = (button, x, y, release = false) => ({ input: `\x1b[<${button};${x};${y}${release ? "m" : "M"}` });
const INDICATOR = () => "\x1b[7m ↓ Jump to latest message · End \x1b[27m";

const scenarios = [
	{
		name: "keys",
		steps: [
			key("\x1b[5~"), key("\x1b[5~"), key("\x1b[6~"), key("\x15"), key("\x04"), key("\x1bk"), key("\x1bk"),
			key("\x1bj"), key("\x1b[H"), key("\x1b[6~"), key("\x1b[F"), key("\x1b[6~"), key("\x1b[H"), key("\x1b[5~"),
		],
	},
	{
		name: "prompts",
		steps: [
			key("\x1b[1;5A"), key("\x1b[1;5A"), key("\x1b[1;6A"), key("\x1b[1;5B"), key("\x1b[1;6B"), key("\x1b[H"),
			key("\x1b[1;5A"), key("\x1b[1;5B"), key("\x1b[1;5B"), key("\x1b[1;5B"), key("\x1b[1;5B"), key("\x1b[1;5B"),
		],
	},
	{
		name: "wheel",
		options: { wheelScrollLines: 3 },
		steps: [
			sgr(64, 2, 2), sgr(64, 2, 2), sgr(72, 2, 2), sgr(66, 2, 2), sgr(67, 2, 2), sgr(65, 2, 2), { expire: true },
			sgr(65, 2, 10), sgr(73, 2, 2), sgr(73, 2, 2), { grow: 3 }, sgr(64, 2, 2), { grow: 2 },
			{ input: "\x1b[M`\x23\x23" }, { input: "\x1b[Ma\x23\x23" },
		],
	},
	{
		name: "follow",
		steps: [{ grow: 2 }, sgr(64, 1, 1), { grow: 3 }, key("\x1b[F"), { grow: 1 }, key("\x1b[6~"), { grow: 1 }],
	},
	{
		name: "scrollbar",
		steps: [
			{ input: "\x1b[<35;30;3M" }, { input: "\x1b[<35;10;3M" }, { expire: true }, sgr(0, 30, 2), sgr(32, 30, 5),
			sgr(32, 30, 8), sgr(0, 30, 8, true), { expire: true }, sgr(0, 30, 1), sgr(32, 30, 3), sgr(0, 30, 3, true),
			{ input: "\x1b[<35;30;4M" }, sgr(0, 30, 4), sgr(32, 30, 4), sgr(0, 30, 4, true), { expire: true },
		],
	},
	{
		name: "indicator",
		options: { scrollToEndIndicator: INDICATOR },
		steps: [sgr(64, 2, 2), { expire: true }, sgr(0, 3, 7), sgr(0, 3, 7, true), sgr(64, 2, 2), sgr(0, 8, 8), sgr(0, 8, 8, true),
			sgr(64, 2, 2), sgr(64, 2, 2), { resize: [50, 12] }, { resize: [24, 12] }, { resize: [12, 12] }, { expire: true }],
	},
	{
		name: "select-drag",
		steps: [
			sgr(64, 1, 1), sgr(64, 1, 1), { expire: true }, sgr(0, 3, 2), sgr(32, 6, 2), sgr(32, 4, 4), sgr(0, 4, 4, true),
			{ at: 100 }, { expire: true }, sgr(0, 6, 4), sgr(32, 2, 3), sgr(3, 2, 3, true), { at: 600 }, sgr(0, 2, 11),
			sgr(32, 5, 12), sgr(0, 5, 12, true), sgr(0, 2, 2), sgr(0, 2, 2, true), { input: "\x1b[O" },
		],
	},
	{
		name: "select-words",
		steps: [
			key("\x1b[H"), sgr(65, 1, 1), sgr(65, 1, 1), sgr(65, 1, 1), { expire: true }, sgr(0, 10, 4),
			sgr(0, 10, 4, true), { at: 100 }, sgr(0, 10, 4), sgr(0, 10, 4, true), { at: 100 }, sgr(0, 10, 4),
			sgr(0, 10, 4, true), { at: 100 }, sgr(0, 10, 4), sgr(0, 10, 4, true), { at: 100 }, sgr(0, 10, 4),
			sgr(0, 10, 4, true), { at: 600 }, sgr(0, 1, 4),
			sgr(0, 1, 4, true), { at: 100 }, sgr(0, 1, 4), sgr(32, 3, 5), sgr(32, 3, 5), sgr(0, 3, 5, true),
			{ at: 700 }, sgr(0, 3, 5), sgr(0, 3, 5, true), { at: 50 }, sgr(0, 3, 5), sgr(32, 1, 4),
			sgr(0, 1, 4, true), { at: 700 }, sgr(0, 5, 5), sgr(0, 5, 5, true), { at: 50 }, sgr(0, 5, 5),
			sgr(0, 5, 5, true), { at: 700 }, sgr(0, 9, 6), sgr(0, 9, 6, true), { at: 50 }, sgr(0, 9, 6),
			sgr(0, 9, 6, true), { at: 700 }, sgr(0, 12, 6), sgr(0, 12, 6, true), { at: 50 }, sgr(0, 12, 6),
			sgr(0, 12, 6, true), { at: 700 }, sgr(0, 5, 4), sgr(0, 5, 4, true), { at: 50 }, sgr(0, 5, 4),
			sgr(0, 5, 4, true), { at: 50 }, sgr(0, 5, 4), sgr(32, 2, 7), sgr(0, 2, 7, true), { at: 700 },
			sgr(0, 28, 3), sgr(0, 28, 3, true), { at: 50 }, sgr(0, 28, 3), sgr(0, 28, 3, true), sgr(0, 6, 5),
			sgr(32, 8, 6), sgr(0, 8, 6, true),
		],
	},
	{
		name: "autoscroll",
		steps: [
			sgr(64, 1, 1), sgr(64, 1, 1), { expire: true }, sgr(0, 4, 4), sgr(32, 4, 1), { autoscroll: true },
			{ autoscroll: true }, sgr(32, 6, 1), { autoscroll: true }, sgr(32, 6, 4), { autoscroll: true },
			sgr(32, 3, 7), { autoscroll: true }, { autoscroll: true }, sgr(0, 3, 7, true), { autoscroll: true },
		],
	},
	{
		name: "copy-off",
		options: { copyOnSelect: false },
		steps: [sgr(64, 1, 1), { expire: true }, sgr(0, 1, 2), sgr(32, 5, 3), sgr(0, 5, 3, true), { input: "\x1b[O" },
			{ input: "\x1b[I" }],
	},
	{
		name: "copy-fails",
		copyResult: "Clipboard unavailable: install `xclip` or `xsel`",
		steps: [sgr(0, 1, 2), sgr(32, 5, 2), sgr(0, 5, 2, true), { expire: true }],
	},
	{
		name: "copy-osc52",
		options: { injectCopy: false },
		steps: [sgr(0, 1, 2), sgr(32, 5, 3), sgr(0, 5, 3, true), sgr(0, 2, 6), sgr(0, 2, 6, true), { expire: true }],
	},
	{
		name: "focus-loss",
		steps: [sgr(0, 1, 2), sgr(32, 5, 3), { input: "\x1b[O" }, sgr(0, 5, 3, true), sgr(32, 6, 3), sgr(0, 1, 2),
			sgr(32, 5, 2), sgr(0, 5, 2, true), { input: "\x1b[O" }, { input: "\x1b[I" }],
	},
	{
		name: "short",
		doc: 4,
		steps: [sgr(64, 1, 1), key("\x1b[5~"), key("\x1b[H"), key("\x1b[1;5A"), { input: "\x1b[<35;30;2M" },
			sgr(0, 30, 2), { grow: 10 }, key("\x1b[5~"), { grow: 1 }, key("\x1b[H"), { resize: [30, 40] },
			{ input: "\x1b[H", growAfter: 30 }, { resize: [30, 12] }],
	},
	{
		name: "fits",
		doc: 10,
		steps: [sgr(64, 2, 2), { resize: [30, 20] }, { resize: [30, 12] }, sgr(65, 2, 2), { expire: true }],
	},
];

const results = [];
for (const s of scenarios) results.push(await run(s));

// Mouse tracking: tmux-like terminals get button-motion tracking, others all-motion; leaving restores the terminal.
const startStop = [];
for (const env of [{}, { TMUX: "/tmp/tmux-1/default,1,0" }, { TERM: "screen-256color" }, { ZELLIJ: "0" }, { STY: "1.pts" }]) {
	for (const k of ["TMUX", "ZELLIJ", "STY", "TERM"]) delete process.env[k];
	Object.assign(process.env, env);
	const terminal = new FakeTerminal(20, 4);
	const tui = new TuiAltScreen(terminal);
	tui.start();
	const enter = terminal.writes[0];
	terminal.writes.length = 0;
	tui.stop({ preserveScreen: true });
	startStop.push({ env, enter, exit: terminal.writes.join("") });
}

writeFileSync(
	join(here, "fullscreen-scroll-parity.json"),
	`${JSON.stringify({ userBindings: USER_BINDINGS, scenarios: results, startStop })}\n`,
);
process.exit(0);
