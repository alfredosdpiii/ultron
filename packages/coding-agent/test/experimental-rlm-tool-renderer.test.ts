import { beforeAll, describe, expect, test } from "vitest";
import { outputTail } from "../src/experimental/rlm-tool-renderer.ts";
import { truncateToVisualLines } from "../src/modes/interactive/components/visual-truncate.ts";
import { initTheme, theme } from "../src/modes/interactive/theme/theme.ts";

describe("rlm output tail", () => {
	beforeAll(() => initTheme("dark"));

	const color = (text: string) => theme.fg("toolOutput", text);
	/** What the renderer computed before: the whole styled output wrapped, then its last lines. */
	const reference = (output: string, maxLines: number, width: number) =>
		truncateToVisualLines(
			output
				.split("\n")
				.map((line) => color(line))
				.join("\n"),
			maxLines,
			width,
		);

	const outputs = [
		Array.from({ length: 60 }, (_, index) => `${index} ${index * index} match`).join("\n"),
		"short",
		"one\n\ntwo\n\n\nthree",
		`${"word ".repeat(40)}\nshort line\n${"x".repeat(250)}\nend`,
		"tab\tseparated\tvalues\n\tindented",
		"wide 漢字漢字漢字漢字漢字漢字漢字漢字漢字漢字 cells\nemoji 🎉🎉🎉🎉🎉🎉🎉🎉🎉🎉🎉🎉🎉🎉🎉🎉",
		"\x1b[31mred\nstill red?\x1b[0m\nplain",
		"progress 10%\rprogress 100%\ndone",
		Array.from({ length: 30 }, (_, index) => `${index} ${"long text ".repeat(index % 7)}`).join("\n"),
	];

	test("matches wrapping the whole styled output, for any width and line count", () => {
		for (const output of outputs) {
			for (const width of [1, 5, 20, 37, 80, 200]) {
				for (const maxLines of [1, 3, 10]) {
					expect(
						outputTail(output, color, maxLines, width),
						`${JSON.stringify(output)} @${width}/${maxLines}`,
					).toEqual(reference(output, maxLines, width));
				}
			}
		}
	});
});
