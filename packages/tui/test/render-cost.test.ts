import assert from "node:assert";
import { describe, it } from "node:test";
import { Box } from "../src/components/box.ts";
import { HStack } from "../src/components/h-stack.ts";
import { ScrollView } from "../src/components/scroll-view.ts";
import { Text } from "../src/components/text.ts";
import { VStack } from "../src/components/v-stack.ts";
import { renderLayoutFrame } from "../src/layout.ts";
import { type Component, compositeTuiLine } from "../src/tui.ts";
import { TuiAltScreen } from "../src/tui-alt-screen.ts";
import { stripTerminalSequences, visibleWidth } from "../src/utils.ts";
import { VirtualTerminal } from "./virtual-terminal.ts";

class RecordingTerminal extends VirtualTerminal {
	readonly writes: string[] = [];

	override write(data: string): void {
		this.writes.push(data);
		super.write(data);
	}
}

/** Row writes in one frame: every rewritten row starts with a cursor move to its first column. */
function rowWrites(frame: string): number {
	return frame.match(/\x1b\[\d+;1H/g)?.length ?? 0;
}

function transcriptLayout(lineCount: number): { root: Component; transcript: ScrollView } {
	const transcript = new ScrollView(
		new Text(Array.from({ length: lineCount }, (_, index) => `line ${index + 1}`).join("\n"), 0, 0),
		{ follow: "end", primary: true, scrollbar: "auto" },
	);
	const root = new VStack([
		{ component: transcript, basis: 0, grow: 1, shrink: 1, minSize: 1 },
		{ component: new Text("dock", 0, 0), basis: "auto", grow: 0 },
	]);
	return { root, transcript };
}

describe("render cost", () => {
	it("scrolling the transcript moves its rows with a terminal scroll and rewrites only rows that changed", async () => {
		const terminal = new RecordingTerminal(40, 12);
		const tui = new TuiAltScreen(terminal);
		const { root, transcript } = transcriptLayout(200);
		tui.setLayoutRoot(root);
		tui.start();
		await terminal.waitForRender();
		assert.deepStrictEqual(
			terminal.getViewport().map((line) => line.trimEnd()),
			[...Array.from({ length: 11 }, (_, index) => `line ${190 + index}`), "dock"],
		);

		// The first scroll reveals the scrollbar on every row (a full rewrite); later scrolls move the rows.
		transcript.scrollBy(-1);
		await terminal.waitForRender();
		for (const [delta, scroll] of [
			[-1, "\x1b[1T"],
			[-3, "\x1b[3T"],
			[2, "\x1b[2S"],
		] as const) {
			const before = terminal.writes.length;
			const scrolls = tui.regionScrolls;
			transcript.scrollBy(delta);
			await terminal.waitForRender();
			const frame = terminal.writes.slice(before).join("");
			// The transcript's 11 rows scroll inside their own region; the dock row is left alone.
			assert.ok(frame.includes(`\x1b[1;11r${scroll}\x1b[r`), JSON.stringify(frame));
			assert.strictEqual(tui.regionScrolls, scrolls + 1);
			// The rows that came into view, plus the few where the scrollbar thumb moved, not all 11.
			assert.ok(rowWrites(frame) <= Math.abs(delta) + 4, `${rowWrites(frame)} rows written`);
		}

		// What the terminal shows is exactly what a full redraw shows.
		const shown = terminal.getViewport().map((line) => line.trimEnd());
		tui.requestRender(true);
		await terminal.waitForRender();
		assert.deepStrictEqual(
			terminal.getViewport().map((line) => line.trimEnd()),
			shown,
		);
		assert.ok(shown[0]?.startsWith("line 187 "), shown[0]);
		tui.stop();
	});

	it("appending while following the end scrolls the transcript instead of rewriting it", async () => {
		const terminal = new RecordingTerminal(30, 8);
		const tui = new TuiAltScreen(terminal);
		const lines = Array.from({ length: 50 }, (_, index) => `entry ${index + 1}`);
		const text = new Text(lines.join("\n"), 0, 0);
		const root = new VStack([
			{
				component: new ScrollView(text, { follow: "end", primary: true }),
				basis: 0,
				grow: 1,
				shrink: 1,
				minSize: 1,
			},
			{ component: new Text("dock", 0, 0), basis: "auto", grow: 0 },
		]);
		tui.setLayoutRoot(root);
		tui.start();
		await terminal.waitForRender();

		const before = terminal.writes.length;
		lines.push("entry 51");
		text.setText(lines.join("\n"));
		tui.requestRender();
		await terminal.waitForRender();
		const frame = terminal.writes.slice(before).join("");
		assert.ok(frame.includes("\x1b[1;7r\x1b[1S\x1b[r"), JSON.stringify(frame));
		assert.strictEqual(rowWrites(frame), 1);
		assert.deepStrictEqual(
			terminal.getViewport().map((line) => line.trimEnd()),
			["entry 45", "entry 46", "entry 47", "entry 48", "entry 49", "entry 50", "entry 51", "dock"],
		);
		tui.stop();
	});

	it("an idle frame writes no rows", async () => {
		const terminal = new RecordingTerminal(40, 10);
		const tui = new TuiAltScreen(terminal);
		tui.setLayoutRoot(transcriptLayout(100).root);
		tui.start();
		await terminal.waitForRender();
		const before = terminal.writes.length;
		tui.requestRender();
		await terminal.waitForRender();
		assert.strictEqual(rowWrites(terminal.writes.slice(before).join("")), 0);
		tui.stop();
	});

	it("paints side-by-side columns as compositing them would, left to right", () => {
		const styled = (text: string) => `\x1b[1m\x1b[36m${text}\x1b[39m\x1b[22m`;
		const left = new Text(["short", styled("a much longer line that is cut"), "", "wide 漢字 cell"].join("\n"), 0, 0);
		const right = new Text(["side", styled("pane text"), "third", "last"].join("\n"), 0, 0);
		const border: Component = { render: () => ["│", "│", "│", "│"], invalidate() {} };
		const frame = renderLayoutFrame(
			new HStack(
				[
					{ component: left, basis: 16, grow: 0, shrink: 0 },
					{ component: border, basis: 1, grow: 0, shrink: 0 },
					{ component: right, basis: 13, grow: 0, shrink: 0 },
				],
				{ align: "stretch" },
			),
			30,
			4,
			() => {},
		);
		const leftLines = left.render(16);
		const rightLines = right.render(13);
		for (let row = 0; row < 4; row++) {
			let expected = compositeTuiLine("", leftLines[row]!, 0, 16, 30);
			expected = compositeTuiLine(expected, "│", 16, 1, 30);
			expected = compositeTuiLine(expected, rightLines[row]!, 17, 13, 30);
			assert.strictEqual(
				stripTerminalSequences(frame.lines[row]!).trimEnd(),
				stripTerminalSequences(expected).trimEnd(),
			);
			assert.ok(visibleWidth(frame.lines[row]!) <= 30);
		}
		assert.strictEqual(stripTerminalSequences(frame.lines[1]!), "a much longer   │pane text    ");
	});
});

describe("Box render cache", () => {
	it("re-renders an unchanged box without laying out its lines again", () => {
		let backgrounds = 0;
		const box = new Box(1, 0, (text) => {
			backgrounds += 1;
			return `\x1b[44m${text}\x1b[49m`;
		});
		box.addChild(new Text("one\ntwo\nthree", 0, 0));
		const first = box.render(20);
		const afterFirst = backgrounds;
		assert.ok(afterFirst > 3);
		const second = box.render(20);
		assert.strictEqual(second, first);
		// Only the one-call sample that detects a changed background function.
		assert.strictEqual(backgrounds, afterFirst + 1);
	});

	it("notices a child that refills the array it returned", () => {
		const lines = ["first"];
		const child: Component = { render: () => lines, invalidate() {} };
		const box = new Box(0, 0);
		box.addChild(child);
		assert.deepStrictEqual(
			box.render(10).map((line) => line.trimEnd()),
			["first"],
		);
		lines[0] = "changed";
		assert.deepStrictEqual(
			box.render(10).map((line) => line.trimEnd()),
			["changed"],
		);
	});
});
