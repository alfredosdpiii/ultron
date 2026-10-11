// Generates crates/tui/fixtures/theme-styles-parity.json: Theme.bold/italic/underline/inverse/strikethrough from
// packages/coding-agent theme.ts (chalk 6) at chalk levels 0-3, over plain, nested-ANSI, multi-line and empty inputs.
// Usage: node crates/tui/fixtures/generate-theme-styles.mjs   (deterministic; output is written next to this file)
import chalk from "chalk";
import { writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
const here = dirname(fileURLToPath(import.meta.url));
const themeDir = join(here, "../../../packages/coding-agent/src/modes/interactive/theme");
const T = await import(join(themeDir, "theme.ts"));
const { validateThemeJson } = await import(join(themeDir, "theme-json.ts"));
T.setThemeJsonValidator(validateThemeJson);
const theme = T.loadThemeFromPath(join(themeDir, "dark.json"), "truecolor");

const INPUTS = [
	"hello", "", "a\nb", "a\r\nb\n", "\n", "x\x1b[22my", "x\x1b[23m\x1b[24m\x1b[27m\x1b[29my", "\x1b[1mbold\x1b[22m tail",
	"multi\nline\x1b[22m\r\nend", "日本 😀", "\x1b", "a\rb",
];
const STYLES = ["bold", "italic", "underline", "inverse", "strikethrough"];
const cases = [];
for (const level of [0, 1, 2, 3]) {
	chalk.level = level;
	for (const style of STYLES) for (const input of INPUTS) cases.push({ level, style, input, output: theme[style](input) });
}
writeFileSync(join(here, "theme-styles-parity.json"), `${JSON.stringify({ cases }, null, "\t")}\n`);
console.log(`theme-styles-parity.json: ${cases.length} cases`);
