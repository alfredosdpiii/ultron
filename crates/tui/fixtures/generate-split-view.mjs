// Generates crates/tui/fixtures/split-view-parity.json from packages/tui components/split-view.ts (TypeScript, this
// checkout): SplitView's column widths and its line-by-line merge (`render`), over plain, colored, wide-character and
// overlong lines, with the pane open and closed, at several widths.
// Usage: node crates/tui/fixtures/generate-split-view.mjs   (deterministic; output is written next to this file)
import { writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
const here = dirname(fileURLToPath(import.meta.url));
const { SplitView } = await import(join(here, "../../../packages/tui/src/components/split-view.ts"));

const fixed = (lines) => ({ render: () => lines, invalidate() {} });
const mains = [
	["hello", "\x1b[31mred text that runs long past the column edge\x1b[39m", "", "日本語の行です、長い行"],
	["one"],
];
const sides = [
	[" pane", " \x1b[1mbold\x1b[22m row", " 日本語"],
	[" a", " b", " c", " d", " e"],
];
const cases = [];
for (const [mi, main] of mains.entries()) {
	for (const [si, side] of sides.entries()) {
		for (const width of [20, 50, 120, 160]) {
			for (const open of [true, false]) {
				const view = new SplitView(fixed(main), fixed(side), {
					columns: () => width,
					rows: () => 4,
					open: () => open,
					sideWidth: (w) => Math.max(44, Math.round(w * 0.4)),
					borderStyle: (t) => `\x1b[90m${t}\x1b[39m`,
				});
				const [mainWidth, sideWidth] = view.widths(width);
				cases.push({ main: mi, side: si, width, open, mainWidth, sideWidth, lines: view.render(width) });
			}
		}
	}
}
writeFileSync(join(here, "split-view-parity.json"), `${JSON.stringify({ mains, sides, cases }, null, "\t")}\n`);
