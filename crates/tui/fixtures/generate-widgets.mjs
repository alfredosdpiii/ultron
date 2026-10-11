// Generates crates/tui/fixtures/widgets-parity.json from packages/tui (TypeScript, this checkout): SelectList,
// SettingsList, Loader/CancellableLoader, overlay layout and compositing, terminal-image encoding and the Image
// component, as line arrays per scripted input and width.
// Usage: node crates/tui/fixtures/generate-widgets.mjs   (deterministic; output is written next to this file)
import { writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
const here = dirname(fileURLToPath(import.meta.url));
const src = (p) => import(join(here, "../../../packages/tui/src", p));
const { SelectList } = await src("components/select-list.ts");
const { SettingsList } = await src("components/settings-list.ts");
const { Loader } = await src("components/loader.ts");
const { CancellableLoader } = await src("components/cancellable-loader.ts");
const { Text } = await src("components/text.ts");
const { Image } = await src("components/image.ts");
const tui = await src("tui.ts");
const ti = await src("terminal-image.ts");

const WIDTHS = [1, 5, 10, 30, 41, 42, 60, 100];
const KEYS = { up: "\x1b[A", down: "\x1b[B", enter: "\r", esc: "\x1b", ctrlc: "\x03", space: " ", x: "x" };
const sty = (tag) => (s) => `\x1b[${tag}m${s}\x1b[0m`;

// SelectList
const selectTheme = { selectedPrefix: sty(36), selectedText: sty(1), description: sty(2), scrollInfo: sty(90), noMatch: sty(33) };
const selectItems = [
	{ value: "alpha", label: "Alpha", description: "First letter" },
	{ value: "beta", label: "", description: "Second\nline two\r\nthree" },
	{ value: "gamma-ray-burst-long-value-name-that-overflows", label: "gamma-ray-burst-long-value-name-that-overflows", description: "A very long description that keeps on going and going past the edge" },
	{ value: "delta", label: "日本語のラベル", description: "  spaced  " },
	{ value: "epsilon", label: "Epsilon" },
	{ value: "Alps", label: "Alps", description: "\n" },
	{ value: "zeta", label: "Zeta", description: "z" },
];
const selectScripts = [
	{ maxVisible: 5, layout: {}, filter: null, keys: ["down", "down", "down", "up", "up", "up", "up", "enter", "esc", "x", "ctrlc"] },
	{ maxVisible: 3, layout: { minPrimaryColumnWidth: 8, maxPrimaryColumnWidth: 20 }, filter: null, keys: ["down", "down", "down", "down", "down", "down", "down"] },
	{ maxVisible: 10, layout: { maxPrimaryColumnWidth: 12 }, filter: "al", keys: ["down", "enter"] },
	{ maxVisible: 4, layout: { minPrimaryColumnWidth: 50 }, filter: "nothing", keys: ["down", "enter"] },
	{ maxVisible: 2, layout: {}, filter: "", keys: ["up", "up"] },
];
const select = selectScripts.map((script) => {
	const list = new SelectList(selectItems, script.maxVisible, selectTheme, script.layout);
	const events = [];
	list.onSelect = (i) => events.push(["selected", i.value]);
	list.onCancel = () => events.push(["cancelled"]);
	list.onSelectionChange = (i) => events.push(["changed", i.value]);
	if (script.filter !== null) list.setFilter(script.filter);
	const frames = [];
	const snap = () => frames.push(WIDTHS.map((w) => list.render(w)));
	snap();
	for (const k of script.keys) {
		list.handleInput(KEYS[k]);
		snap();
	}
	return { ...script, frames, events };
});
// Every width from 1 to 120 for the default layout, to pin the description-column thresholds.
const SWEEP = Array.from({ length: 120 }, (_, i) => i + 1);
const selectSweep = [0, 2, 3].map((sel) => {
	const list = new SelectList(selectItems, 7, selectTheme);
	list.setSelectedIndex(sel);
	return [sel, SWEEP.map((w) => list.render(w))];
});

// SettingsList (no search, no submenus)
const settingsTheme = {
	label: (t, s) => (s ? `\x1b[1m${t}\x1b[0m` : t),
	value: (t, s) => (s ? `\x1b[36m${t}\x1b[0m` : `\x1b[2m${t}\x1b[0m`),
	description: sty(3),
	cursor: "→ ",
	hint: sty(90),
};
const settingsItems = () => [
	{ id: "theme", label: "Theme", currentValue: "dark", values: ["dark", "light", "system"], description: "Color theme used for the whole interface, wrapped when it is long enough" },
	{ id: "thinking", label: "Thinking level", currentValue: "unknown", values: ["off", "low", "high"] },
	{ id: "ro", label: "Read only", currentValue: "fixed value that is fairly long" },
	{ id: "cjk", label: "日本語の設定項目", currentValue: "はい", values: ["はい", "いいえ"], description: "説明" },
	{ id: "e", label: "E", currentValue: "1", values: ["1"] },
	{ id: "long", label: "A label that is much longer than thirty-six columns", currentValue: "v", values: ["v", "w"] },
];
const settingsScripts = [
	{ maxVisible: 10, items: true, keys: ["enter", "enter", "space", "down", "enter", "down", "enter", "down", "space", "up", "up", "up", "up", "up", "esc", "x"] },
	{ maxVisible: 2, items: true, keys: ["down", "down", "down", "down", "down", "ctrlc"] },
	{ maxVisible: 3, items: false, keys: ["down", "enter"] },
];
const settings = settingsScripts.map((script) => {
	const events = [];
	const list = new SettingsList(script.items ? settingsItems() : [], script.maxVisible, settingsTheme, (id, v) => events.push(["changed", id, v]), () => events.push(["cancelled"]));
	const frames = [];
	const snap = () => frames.push(WIDTHS.map((w) => list.render(w)));
	snap();
	for (const k of script.keys) {
		list.handleInput(KEYS[k]);
		snap();
	}
	return { ...script, frames, events };
});

// Loader / CancellableLoader. Animation frames are stepped by hand instead of by the interval timer.
const fakeUi = { requestRender() {} };
const loaderCases = [
	{ message: "Loading...", indicator: null, ticks: 11 },
	{ message: "Working on a long task that wraps across lines", indicator: { frames: ["-", "\\", "|", "/"], intervalMs: 50 }, ticks: 5 },
	{ message: "No spinner", indicator: { frames: [] }, ticks: 2 },
	{ message: "Static", indicator: { frames: ["●"], intervalMs: -1 }, ticks: 2 },
];
const loaders = loaderCases.map((c) => {
	const l = new Loader(fakeUi, sty(36), sty(2), c.message, c.indicator ?? undefined);
	l.stop();
	const frames = [];
	for (let t = 0; t <= c.ticks; t++) {
		frames.push(WIDTHS.map((w) => l.render(w)));
		if (l.frames.length > 1) {
			l.currentFrame = (l.currentFrame + 1) % l.frames.length;
			l.updateDisplay();
		}
	}
	return { ...c, intervalMs: l.intervalMs, animated: l.frames.length > 1, frames };
});
const cancelInputs = ["x", "\r", "\x1b", "\x03", "\x1b[A"];
const cancellable = cancelInputs.map((data) => {
	const l = new CancellableLoader(fakeUi, sty(36), sty(2), "Cancel me");
	l.stop();
	let fired = false;
	l.onAbort = () => (fired = true);
	l.handleInput(data);
	return [data, fired, l.aborted];
});

// Overlay layout
const proto = tui.TuiBase.prototype;
const ANCHORS = ["center", "top-left", "top-right", "bottom-left", "bottom-right", "top-center", "bottom-center", "left-center", "right-center"];
let seed = 0x1234567;
const rand = (n) => {
	seed ^= seed << 13; seed >>>= 0; seed ^= seed >>> 17; seed ^= seed << 5; seed >>>= 0;
	return seed % n;
};
const size = () => (rand(3) === 0 ? `${rand(120)}%` : rand(3) === 0 ? `${rand(100)}.5%` : rand(130) - 10);
const pick = (f) => (rand(2) ? f() : undefined);
const layoutCases = [];
for (let i = 0; i < 400; i++) {
	const opt = {
		width: pick(size), minWidth: pick(() => rand(60)), maxHeight: pick(size), anchor: pick(() => ANCHORS[rand(9)]),
		offsetX: pick(() => rand(40) - 20), offsetY: pick(() => rand(20) - 10), row: pick(size), col: pick(size),
		margin: pick(() => (rand(2) ? rand(6) - 1 : { top: rand(5) - 1, right: rand(5), bottom: rand(5), left: rand(5) - 1 })),
	};
	for (const k of Object.keys(opt)) if (opt[k] === undefined) delete opt[k];
	const h = rand(40), tw = 1 + rand(150), th = 1 + rand(50);
	layoutCases.push([opt, h, tw, th, proto.resolveOverlayLayout.call(proto, opt, h, tw, th)]);
}

// compositeTuiLine
const BASES = ["", "plain base line with some text", "\x1b[31mred\x1b[0m and \x1b[1mbold text continuing\x1b[22m", "日本語のテキストが続きます", "\x1b]8;;https://x.y\x07link text here\x1b]8;;\x07 after", "\x1b[44mbackground all the way across\x1b[49m", "\x1b_Gf=100;AAAA\x1b\\"];
const OVERLAYS = ["", "OVER", "\x1b[7minverse\x1b[27m", "全角", "😀x😀"];
const composite = [];
for (const b of BASES) for (const o of OVERLAYS) for (const [c, w, t] of [[0, 4, 20], [3, 6, 30], [10, 5, 12], [1, 3, 4], [5, 10, 40], [2, 2, 3]]) {
	composite.push([b, o, c, w, t, tui.compositeTuiLine(b, o, c, w, t)]);
}

// compositeOverlays with Text overlays
const overlayScenarios = [
	{ base: ["line one", "line two \x1b[32mgreen\x1b[0m", "line three"], tw: 40, th: 8, overlays: [["Hello overlay", { width: 20 }], ["Second one on top", { anchor: "bottom-right", width: "50%", maxHeight: 2 }]] },
	{ base: Array.from({ length: 20 }, (_, i) => `row ${i} ${"x".repeat(i)}`), tw: 30, th: 6, overlays: [["A long message that wraps over several lines in the overlay", { row: 1, col: 2, width: 15 }]] },
	{ base: [], tw: 10, th: 3, overlays: [["tiny", { anchor: "top-left", offsetX: 3, offsetY: 5 }]] },
];
const overlays = overlayScenarios.map((s) => {
	const fake = Object.create(proto);
	fake.overlayStack = s.overlays.map(([text, options], i) => ({ component: new Text(text, 1, 0), options, focusOrder: i, hidden: false }));
	fake.isOverlayVisible = () => true;
	const lines = fake.compositeOverlays([...s.base], s.tw, s.th);
	return { ...s, lines, bounds: fake.overlayStack.map((e) => e.bounds) };
});

// terminal-image
const bytes = (arr) => Buffer.from(arr).toString("base64");
const png = (w, h) => bytes([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 13, 0x49, 0x48, 0x44, 0x52, w >>> 24, (w >> 16) & 255, (w >> 8) & 255, w & 255, h >>> 24, (h >> 16) & 255, (h >> 8) & 255, h & 255, 8, 6, 0, 0, 0]);
const jpeg = bytes([0xff, 0xd8, 0xff, 0xe0, 0, 16, ...Array(14).fill(0), 0xff, 0xc0, 0, 17, 8, 0x01, 0x2c, 0x02, 0x58, 3, 0, 0, 0, 0, 0, 0, 0]);
const gif = bytes([...Buffer.from("GIF89a"), 0x40, 0x01, 0xf0, 0x00, 0, 0]);
const webpVp8 = bytes([...Buffer.from("RIFF"), 0, 0, 0, 0, ...Buffer.from("WEBPVP8 "), ...Array(10).fill(0), 0x80, 0x02, 0xe0, 0x01, 0, 0]);
const webpVp8l = bytes([...Buffer.from("RIFF"), 0, 0, 0, 0, ...Buffer.from("WEBPVP8L"), 0, 0, 0, 0, 0x2f, 0x3f, 0xc0, 0x77, 0x00, 0, 0, 0, 0, 0]);
const webpVp8x = bytes([...Buffer.from("RIFF"), 0, 0, 0, 0, ...Buffer.from("WEBPVP8X"), ...Array(8).fill(0), 0x7f, 0x02, 0x00, 0xdf, 0x01, 0x00, 0, 0]);
const images = [["image/png", png(1920, 1080)], ["image/png", png(1, 4000)], ["image/png", "notpng"], ["image/jpeg", jpeg], ["image/jpeg", bytes([0xff, 0xd8, 0, 0])], ["image/gif", gif], ["image/webp", webpVp8], ["image/webp", webpVp8l], ["image/webp", webpVp8x], ["image/bmp", png(2, 2)], ["image/png", png(300, 200).replace(/=+$/, "") + "\n"]];
const dims = images.map(([m, d]) => [m, d, ti.getImageDimensions(d, m)]);
const bigData = "QUJD".repeat(2500);
const kitty = [];
for (const data of ["AAAA", bigData, "A".repeat(4096), "A".repeat(8193)]) {
	for (const opt of [{}, { columns: 10, rows: 3 }, { columns: 0, imageId: 42, moveCursor: false }, { imageId: 7, moveCursor: true }]) kitty.push([data, opt, ti.encodeKitty(data, opt)]);
}
const iterm = [];
for (const data of ["AAAA", "AAA=", "AA==", "QUJDRA", ""]) {
	for (const opt of [{}, { width: 10, height: "auto" }, { name: "pic é.png", preserveAspectRatio: false, inline: false }, { preserveAspectRatio: true, name: "" }]) iterm.push([data, opt, ti.encodeITerm2(data, opt)]);
}
const cellSizes = [];
for (let i = 0; i < 300; i++) {
	const d = { widthPx: rand(5000), heightPx: rand(5000) };
	const mw = rand(120), mh = rand(3) ? rand(60) : undefined, cell = rand(2) ? { widthPx: 9, heightPx: 18 } : { widthPx: 7 + rand(10), heightPx: 14 + rand(20) };
	cellSizes.push([d, mw, mh ?? null, cell, ti.calculateImageCellSize(d, mw, mh, cell)]);
}
const capsList = [{ images: "kitty", trueColor: true, hyperlinks: true }, { images: "iterm2", trueColor: true, hyperlinks: true }, { images: null, trueColor: false, hyperlinks: false }, { images: null, trueColor: true, hyperlinks: true }];
const imageComponents = [];
for (const caps of capsList) {
	ti.setCapabilities(caps);
	for (const [mime, data, opts] of [["image/png", png(1920, 1080), { imageId: 5 }], ["image/png", png(100, 900), { imageId: 9, maxHeightCells: 4, filename: "/tmp/a b#c é.png" }], ["image/gif", gif, { maxWidthCells: 8, filename: "rel/pic.gif", imageId: 3 }], ["image/png", "zz", { imageId: 1 }]]) {
		for (const w of [1, 3, 20, 80]) imageComponents.push([caps, mime, data, opts, w, new Image(data, mime, { fallbackColor: sty(33) }, opts).render(w)]);
	}
}
ti.setCapabilities(null);

// Capability detection from the environment
const ENV_KEYS = ["TERM_PROGRAM", "TERMINAL_EMULATOR", "TERM", "COLORTERM", "TMUX", "KITTY_WINDOW_ID", "GHOSTTY_RESOURCES_DIR", "WEZTERM_PANE", "WARP_SESSION_ID", "WARP_TERMINAL_SESSION_UUID", "ITERM_SESSION_ID", "WT_SESSION", "PI_HYPERLINKS", "PI_IMAGE_PROTOCOL", "PI_TRUE_COLOR"];
const envCases = [{}, { TMUX: "1" }, { TERM: "tmux-256color", COLORTERM: "truecolor" }, { TERM: "screen" }, { TERM_PROGRAM: "Kitty" }, { KITTY_WINDOW_ID: "1" }, { TERM: "xterm-ghostty" }, { GHOSTTY_RESOURCES_DIR: "/x" }, { TERM_PROGRAM: "WezTerm" }, { WARP_SESSION_ID: "1" }, { TERM_PROGRAM: "WarpTerminal" }, { ITERM_SESSION_ID: "1" }, { TERM_PROGRAM: "iTerm.app" }, { WT_SESSION: "1" }, { TERM_PROGRAM: "vscode" }, { TERM_PROGRAM: "zed" }, { TERM_PROGRAM: "alacritty" }, { TERMINAL_EMULATOR: "JetBrains-JediTerm" }, { COLORTERM: "24bit" }, { TERM: "xterm-256color", COLORTERM: "TRUECOLOR" }, { TMUX: "", TERM: "xterm" }, { PI_IMAGE_PROTOCOL: "KITTY" }, { PI_IMAGE_PROTOCOL: "none", KITTY_WINDOW_ID: "1" }, { PI_IMAGE_PROTOCOL: "iterm2", PI_TRUE_COLOR: "0", TERM_PROGRAM: "kitty" }, { PI_HYPERLINKS: "0", TMUX: "1" }, { PI_HYPERLINKS: "1", PI_TRUE_COLOR: "1" }, { PI_IMAGE_PROTOCOL: "sixel", PI_HYPERLINKS: "yes" }];
const saved = Object.fromEntries(ENV_KEYS.map((k) => [k, process.env[k]]));
const detect = [];
for (const env of envCases) for (const tmuxProbe of [false, true]) {
	for (const k of ENV_KEYS) delete process.env[k];
	Object.assign(process.env, env);
	ti.resetCapabilitiesCache();
	detect.push([env, tmuxProbe, ti.detectCapabilities(() => tmuxProbe)]);
}
for (const k of ENV_KEYS) if (saved[k] === undefined) delete process.env[k]; else process.env[k] = saved[k];

// ProcessTerminal escape output (stdout.write captured; no tty needed for these methods)
const { ProcessTerminal } = await src("terminal.ts");
const terminalWrites = [];
{
	const term = new ProcessTerminal();
	const realWrite = process.stdout.write;
	for (const [method, arg] of [["moveBy", -3], ["moveBy", 0], ["moveBy", 5], ["hideCursor"], ["showCursor"], ["clearLine"], ["clearFromCursor"], ["clearScreen"], ["setTitle", "ultron — 日本"], ["write", "plain \x1b[1mbold"]]) {
		let out = "";
		process.stdout.write = (d) => ((out += d), true);
		try {
			term[method](arg);
		} finally {
			process.stdout.write = realWrite;
		}
		terminalWrites.push([method, arg ?? null, out]);
	}
}

const fixture = { terminalWrites, settingsItems: settingsItems(), selectSweep, sweep: SWEEP, widths: WIDTHS, keys: KEYS, selectItems, select, settings, loaders, cancellable, layoutCases, composite, overlays, dims, kitty, iterm, cellSizes, imageComponents, detect };
writeFileSync(join(here, "widgets-parity.json"), `${JSON.stringify(fixture)}\n`);
