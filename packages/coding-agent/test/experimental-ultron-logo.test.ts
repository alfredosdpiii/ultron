import { describe, expect, test } from "vitest";
import { StartupHeader } from "../src/experimental/client-tui-commands.ts";
import { halveLogo, splashLines, ULTRON_LOGO } from "../src/experimental/ultron-logo.ts";
import { initTheme } from "../src/modes/interactive/theme/theme.ts";

const strip = (line: string) => line.replace(/\x1b\[[0-9;]*m/g, "");
const FULL_HEIGHT = ULTRON_LOGO.split("\n").length;

describe("Ultron splash logo", () => {
	test("the full logo shows when the terminal fits it, centered", () => {
		const lines = splashLines(120, 80);
		expect(lines).toHaveLength(FULL_HEIGHT);
		expect(Math.max(...lines.map((line) => line.length))).toBeLessThanOrEqual(120);
		// Centered: the widest row starts after (120 - 92) / 2 columns of padding.
		expect(lines.find((line) => line.trim().length > 0)?.startsWith(" ".repeat(14))).toBe(true);
	});

	test("the half-size logo shows in a normal terminal, and none in a tiny one", () => {
		const half = splashLines(80, 45);
		expect(half.length).toBe(halveLogo(ULTRON_LOGO).length);
		expect(half.length).toBeLessThan(FULL_HEIGHT);
		expect(Math.max(...half.map((line) => line.length))).toBeLessThanOrEqual(80);
		expect(splashLines(40, 45)).toEqual([]);
		expect(splashLines(80, 20)).toEqual([]);
	});

	test("halving keeps the most inked character of each 2x2 block", () => {
		expect(halveLogo("  >\n.  ")).toEqual([".>"]);
	});

	test("the header draws the splash above its text unless disabled", () => {
		initTheme("dark", false);
		const shown = new StartupHeader({ rows: () => 80, enabled: () => true }).render(120).map(strip);
		const quiet = new StartupHeader({ rows: () => 80, enabled: () => false }).render(120).map(strip);
		const plain = new StartupHeader().render(120).map(strip);
		expect(shown.length).toBe(plain.length + FULL_HEIGHT + 1);
		expect(quiet).toEqual(plain);
		expect(shown.some((line) => line.includes(">>>>>>>>>>>>>>>>>>>>"))).toBe(true);
		expect(shown.slice(FULL_HEIGHT + 1)).toEqual(plain);
	});
});
