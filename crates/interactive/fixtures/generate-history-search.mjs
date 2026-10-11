// Generates crates/interactive/fixtures/history-search-parity.json from packages/coding-agent's prompt-history search
// (experimental/prompt-history-search.ts and searchPromptHistory in experimental/prompt-history.ts; TypeScript, this
// checkout): scripted key sequences over a fixed history, with the accepted/cancelled outcome, query, matches,
// selection and rendered rows after every key, at several widths in the dark (truecolor) and light (256color) themes.
// Usage: node crates/interactive/fixtures/generate-history-search.mjs   (deterministic; output is written next to this file)
process.env.FORCE_COLOR = "3";
import { writeFileSync } from "node:fs";
import { createRequire } from "node:module";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const ca = (p) => import(join(here, "../../../packages/coding-agent/src", p));
const T = await ca("modes/interactive/theme/theme.ts");
const { validateThemeJson } = await ca("modes/interactive/theme/theme-json.ts");
T.setThemeJsonValidator(validateThemeJson);
const { KeybindingsManager } = await ca("core/keybindings.ts");
const tuiUrl = pathToFileURL(createRequire(join(here, "../../../packages/coding-agent/package.json")).resolve("@ultron/tui")).href;
const { setKeybindings } = await import(tuiUrl);
setKeybindings(new KeybindingsManager({}));
const { PromptHistorySearchComponent } = await ca("experimental/prompt-history-search.ts");
const { searchPromptHistory } = await ca("experimental/prompt-history.ts");
const themeDir = join(here, "../../../packages/coding-agent/src/modes/interactive/theme");

const history = [
	"fix the failing test in parser.rs",
	"Explain   the\nparser module",
	"run cargo test --workspace",
	"write a test for the tokenizer",
	"refactor PARSER error handling",
	"what does rlm.spawn return?",
	"Straße und ÜBER test",
	"one more test prompt",
	"test",
	"tests for the lexer and the parser and the tokenizer and everything else in between",
	"deploy",
];
const searches = ["", "test", "TEST parser", "parser test", "zzz", "  the   parser ", "über", "ß", "t"].map((q) => [
	q,
	searchPromptHistory(history, q),
]);

const KEYS = { up: "\x1b[A", down: "\x1b[B", enter: "\r", tab: "\t", esc: "\x1b", ctrlc: "\x03", ctrlr: "\x12", ctrls: "\x13", altr: "\x1br", bs: "\x7f" };
const scripts = [
	["t", "e", "s", "t", "up", "up", "down", "enter"],
	["t", "e", "s", "t", "altr", "ctrlr", "altr", "ctrls", "tab"],
	["p", "a", "r", "s", "e", "r", " ", "t", "e", "s", "t", "esc"],
	["z", "z", "z", "enter"],
	["z", "bs", "up", "up", "up", "up", "up", "up", "up", "up", "up", "up", "up", "up", "down", "enter"],
	["ctrlc"],
	["Ü", "b", "e", "r", "enter"],
];
const runs = [];
for (const [name, mode] of [["dark", "truecolor"], ["light", "256color"]]) {
	T.setThemeInstance(T.loadThemeFromPath(join(themeDir, `${name}.json`), mode));
	for (const width of [12, 40, 80]) {
		for (const script of scripts) {
			let outcome = null;
			const c = new PromptHistorySearchComponent(
				history,
				(text) => {
					outcome = { accept: text };
				},
				() => {
					outcome = { cancel: true };
				},
			);
			c.focused = true;
			const steps = [{ key: null, lines: c.render(width), query: c.query, matches: c.matches.length, selected: c.selected ?? null }];
			for (const key of script) {
				c.handleInput(KEYS[key] ?? key);
				steps.push({ key, lines: c.render(width), query: c.query, matches: c.matches.length, selected: c.selected ?? null, outcome });
			}
			runs.push({ theme: `${name}-${mode}`, width, script, steps });
		}
	}
}
// A search opened with a query (dark theme).
T.setThemeInstance(T.loadThemeFromPath(join(themeDir, "dark.json"), "truecolor"));
const seeded = new PromptHistorySearchComponent(history, () => {}, () => {}, "parser");
const seededLines = seeded.render(60);

writeFileSync(
	join(here, "history-search-parity.json"),
	`${JSON.stringify({ history, searches, runs, seeded: { query: "parser", lines: seededLines, matches: seeded.matches } })}\n`,
);
