// Generates crates/tui/fixtures/theme-parity.json from packages/coding-agent theme.ts and theme-json.ts (TypeScript,
// this checkout): resolved palettes (ANSI per token, truecolor and 256color), export colors, CSS colors, validation
// errors for malformed theme documents, color-resolution errors, 256-color math and terminal-theme detection.
// Usage: node crates/tui/fixtures/generate-theme.mjs   (deterministic; output is written next to this file)
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
const here = dirname(fileURLToPath(import.meta.url));
const themeDir = join(here, "../../../packages/coding-agent/src/modes/interactive/theme");
const T = await import(join(themeDir, "theme.ts"));
const { validateThemeJson } = await import(join(themeDir, "theme-json.ts"));
T.setThemeJsonValidator(validateThemeJson);

const builtin = (n) => JSON.parse(readFileSync(join(themeDir, `${n}.json`), "utf-8"));
const dark = builtin("dark");
const without = (obj, ...keys) => { const c = structuredClone(obj); for (const k of keys) delete c.colors[k]; return c; };
const withColors = (obj, patch) => { const c = structuredClone(obj); Object.assign(c.colors, patch); return c; };

const err = (f) => { try { f(); return null; } catch (e) { return e.message; } };

// Validation of malformed documents.
const invalid = {
	number: 42,
	null: null,
	array: [],
	string: "dark",
	empty: {},
	noName: (() => { const c = structuredClone(dark); delete c.name; return c; })(),
	nameNumber: { ...structuredClone(dark), name: 123 },
	nameSlash: { ...structuredClone(dark), name: "a/b" },
	schemaNumber: { ...structuredClone(dark), $schema: 5 },
	colorsString: { name: "x", colors: "red" },
	colorsArray: { name: "x", colors: [] },
	missingOne: without(dark, "accent"),
	missingMany: without(dark, "text", "accent", "bashMode", "toolErrorBg", "thinkingMax", "scrollbarTrack"),
	onlyOptionalMissing: without(dark, "thinkingMax", "searchMatchBg", "searchMatchText", "scrollbarTrack", "scrollbarThumb"),
	colorBool: withColors(dark, { accent: true }),
	colorFloat: withColors(dark, { accent: 1.5 }),
	colorTooBig: withColors(dark, { border: 256 }),
	colorNegative: withColors(dark, { border: -1 }),
	colorNull: withColors(dark, { muted: null }),
	colorObject: withColors(dark, { muted: { r: 1 } }),
	optionalBad: withColors(dark, { thinkingMax: false }),
	extraColorBad: withColors(dark, { notAColor: false }),
	varsArray: { ...structuredClone(dark), vars: ["#ffffff"] },
	varsBad: { ...structuredClone(dark), vars: { a: true, b: 3, c: 999, d: 2.5 } },
	varsString: { ...structuredClone(dark), vars: "x" },
	exportBad: { ...structuredClone(dark), export: { pageBg: true, cardBg: 300, infoBg: "#000000" } },
	exportString: { ...structuredClone(dark), export: "x" },
	mixed: (() => { const c = without(dark, "mdHr", "error"); c.name = 7; c.colors.accent = 1.25; c.vars = { z: null }; return c; })(),
};
const validation = Object.entries(invalid).map(([id, doc]) => ({ id, doc, error: err(() => validateThemeJson(id, doc)) }));
validation.push({ id: "darkOk", doc: dark, error: err(() => validateThemeJson("dark", dark)) });

// Palettes for built-in and custom themes, both color modes.
const tmp = mkdtempSync(join(tmpdir(), "theme-parity-"));
const writeTheme = (file, doc) => { const p = join(tmp, file); writeFileSync(p, typeof doc === "string" ? doc : JSON.stringify(doc)); return p; };
const COLORS = ["accent","border","borderAccent","borderMuted","success","error","warning","muted","dim","text","thinkingText","scrollbarTrack","scrollbarThumb","searchMatchText","userMessageText","customMessageText","customMessageLabel","toolTitle","toolOutput","mdHeading","mdLink","mdLinkUrl","mdCode","mdCodeBlock","mdCodeBlockBorder","mdQuote","mdQuoteBorder","mdHr","mdListBullet","toolDiffAdded","toolDiffRemoved","toolDiffContext","syntaxComment","syntaxKeyword","syntaxFunction","syntaxVariable","syntaxString","syntaxNumber","syntaxType","syntaxOperator","syntaxPunctuation","thinkingOff","thinkingMinimal","thinkingLow","thinkingMedium","thinkingHigh","thinkingXhigh","thinkingMax","bashMode"];
const BGS = ["selectedBg","searchMatchBg","userMessageBg","customMessageBg","toolPendingBg","toolSuccessBg","toolErrorBg"];
const custom = withColors(without(dark, "thinkingMax", "searchMatchBg", "searchMatchText", "scrollbarTrack", "scrollbarThumb"), {
	accent: 39, border: 0, text: "", muted: 244, selectedBg: 17, toolErrorBg: "", dim: "#ABCDEF", warning: "deep",
});
custom.name = "custom";
custom.vars = { ...custom.vars, deep: "deeper", deeper: 208, page: 16 };
custom.export = { pageBg: "page", cardBg: "", infoBg: "#123456" };
const customPath = writeTheme("custom.json", custom);
const lightNoExport = structuredClone(builtin("light")); delete lightNoExport.export; lightNoExport.name = "plain";
const plainPath = writeTheme("plain.json", lightNoExport);
const palettes = [];
for (const [name, path] of [["dark", join(themeDir, "dark.json")], ["light", join(themeDir, "light.json")], ["custom", customPath], ["plain", plainPath]]) {
	for (const mode of ["truecolor", "256color"]) {
		const t = T.loadThemeFromPath(path, mode);
		palettes.push({
			name, mode, themeName: t.name,
			fg: Object.fromEntries(COLORS.map((c) => [c, t.getFgAnsi(c)])),
			bg: Object.fromEntries(BGS.map((c) => [c, t.getBgAnsi(c)])),
			sampleFg: t.fg("accent", "hi"), sampleBg: t.bg("selectedBg", "hi"),
			thinking: ["off", "minimal", "low", "medium", "high", "xhigh", "max", "bogus"].map((l) => t.getThinkingBorderColor(l)("x")),
			bash: t.getBashModeBorderColor()("x"),
		});
	}
}
const customTheme = T.loadThemeFromPath(customPath, "truecolor");
const plainTheme = T.loadThemeFromPath(plainPath, "truecolor");
T.setRegisteredThemes([customTheme, plainTheme]);
const exportsOut = ["dark", "light", "custom", "plain", "missing"].map((name) => ({
	name,
	css: err(() => T.getResolvedThemeColors(name)) === null ? T.getResolvedThemeColors(name) : { error: err(() => T.getResolvedThemeColors(name)) },
	exportColors: T.getThemeExportColors(name),
	isLight: T.isLightTheme(name),
}));

// Color-resolution and parse errors when loading a theme file.
const loadCases = {
	circular: { ...structuredClone(dark), vars: { ...dark.vars, a: "b", b: "a" }, colors: { ...dark.colors, accent: "a" } },
	missingVar: withColors(dark, { accent: "nope" }),
	badHexShort: withColors(dark, { accent: "#fff" }),
	badHexChars: withColors(dark, { border: "#zzzzzz" }),
	badHexBg: withColors(dark, { selectedBg: "#12345" }),
	selfVar: { ...structuredClone(dark), vars: { ...dark.vars, me: "me" }, colors: { ...dark.colors, text: "me" } },
	varToEmpty: { ...structuredClone(dark), vars: { ...dark.vars, none: "" }, colors: { ...dark.colors, text: "none" } },
};
const loadErrors = [];
for (const mode of ["truecolor", "256color"]) {
	for (const [id, doc] of Object.entries(loadCases)) {
		const p = writeTheme(`${id}.json`, doc);
		const message = err(() => T.loadThemeFromPath(p, mode));
		loadErrors.push({ id, mode, doc, error: message === null ? null : message.replaceAll(p, "<path>") });
	}
}
const bom = writeTheme("bom.json", `﻿${JSON.stringify({ ...dark, name: "bom" })}`);
const bomTheme = T.loadThemeFromPath(bom, "truecolor");
const parseError = err(() => T.loadThemeFromPath(writeTheme("broken.json", "{ nope"), "truecolor"));
rmSync(tmp, { recursive: true, force: true });

// 256-color conversion and terminal detection.
const rgbSamples = [];
for (let v = 0; v < 256; v += 17) rgbSamples.push([v, v, v], [v, 0, 255 - v], [v, 128, 64], [v, v + (v < 250 ? 5 : 0), v]);
for (const v of [47, 48, 115, 155, 195, 235, 3, 13, 238, 243, 246, 250]) rgbSamples.push([v, v, v], [v, v + 4, v + 1], [v, 255 - v, v]);
const to256 = rgbSamples.map(([r, g, b]) => {
	const hex = `#${[r, g, b].map((n) => n.toString(16).padStart(2, "0")).join("")}`;
	const t = new T.Theme({ ...Object.fromEntries(COLORS.map((c) => [c, hex])) }, Object.fromEntries(BGS.map((c) => [c, hex])), "256color");
	return { hex, fg: t.getFgAnsi("accent"), bg: t.getBgAnsi("selectedBg") };
});
const colorfgbg = ["", "15;0", "0;15", "0;7", "7;0", "12;8", "default;default", "0;default;15", "15;300", "abc", "15;232", "15;255", "1;2;3"].map((v) => ({ value: v, detection: T.detectTerminalBackgroundFromEnv({ env: v === "" ? {} : { COLORFGBG: v } }) }));
const rgbThemes = [[0, 0, 0], [255, 255, 255], [128, 128, 128], [188, 188, 188], [187, 187, 187], [255, 255, 0], [0, 0, 255]].map(([r, g, b]) => ({ rgb: [r, g, b], theme: T.getThemeForRgbColor({ r, g, b }) }));
const settings = [undefined, "", "dark", "light/dark", " solarized / nord ", "a/b/c", "/dark", "light/", "a/ ", "x/y"].map((s) => ({
	setting: s ?? null,
	auto: T.parseAutoThemeSetting(s) ?? null,
	light: T.resolveThemeSetting(s, "light") ?? null,
	dark: T.resolveThemeSetting(s, "dark") ?? null,
}));
const fullFg = Object.fromEntries(COLORS.map((c) => [c, ""]));
const fullBg = Object.fromEntries(BGS.map((c) => [c, ""]));
// Theme constructor fallbacks for the optional tokens, each base token a distinct index.
const OPTIONAL = ["scrollbarTrack", "scrollbarThumb", "thinkingMax", "searchMatchText", "searchMatchBg"];
const distinctFg = Object.fromEntries(COLORS.filter((c) => !OPTIONAL.includes(c)).map((c, i) => [c, i + 1]));
const distinctBg = Object.fromEntries(BGS.filter((c) => !OPTIONAL.includes(c)).map((c, i) => [c, i + 100]));
const ctorTheme = new T.Theme(distinctFg, distinctBg, "256color");
const ctorFallbacks = { inputFg: distinctFg, inputBg: distinctBg, fg: Object.fromEntries(COLORS.map((c) => [c, ctorTheme.getFgAnsi(c)])), bg: Object.fromEntries(BGS.map((c) => [c, ctorTheme.getBgAnsi(c)])) };
const registerSlash = err(() => T.setRegisteredThemes([new T.Theme(fullFg, fullBg, "truecolor", { name: "a/b" })]));
const languages = ["a.ts", "B.TSX", "x.rs", "Makefile", "Dockerfile", "noext", "a.tar.gz", "x.", ".bashrc", "f.yml", "a.HCL"].map((p) => ({ path: p, lang: T.getLanguageFromPath(p) ?? null }));

const out = { validation, palettes, exports: exportsOut, loadErrors, bomName: bomTheme.name, parseError: parseError.replace(/^(Failed to parse theme )[^:]*:/, "$1<path>:"), to256, colorfgbg, rgbThemes, settings, registerSlash, languages, ctorFallbacks };
writeFileSync(join(here, "theme-parity.json"), `${JSON.stringify(out, null, "\t")}\n`);
