import {
	type Component,
	CURSOR_MARKER,
	ScrollView,
	SplitView,
	stripTerminalSequences,
	Text,
	TuiAltScreen,
	VStack,
} from "@ultron/tui";
import { describe, expect, test } from "vitest";
import { VirtualTerminal } from "../../tui/test/virtual-terminal.ts";

/** A one-line editor stand-in that emits the cursor marker after its text while focused. */
class FakeEditor implements Component {
	focused = true;
	text = "hello";

	render(width: number): string[] {
		const line = `> ${this.text}${this.focused ? CURSOR_MARKER : ""}`;
		return ["─".repeat(width), line, "─".repeat(width)];
	}

	invalidate(): void {}
}

/** Records what the TUI writes, to check that a frame redraws only the rows that changed. */
class RecordingTerminal extends VirtualTerminal {
	writes: string[] = [];

	override write(data: string): void {
		this.writes.push(data);
		super.write(data);
	}
}

/** A side pane that fills the height it is given. */
class FakePane implements Component {
	renders: number[] = [];
	private readonly rows: () => number;

	constructor(rows: () => number) {
		this.rows = rows;
	}

	render(width: number): string[] {
		this.renders.push(width);
		return Array.from({ length: this.rows() }, (_, index) => `pane ${index}`.padEnd(width, "."));
	}

	invalidate(): void {}
}

function splitOptions(terminal: VirtualTerminal, open: { value: boolean }) {
	return {
		columns: () => terminal.columns,
		rows: () => terminal.rows,
		open: (width: number) => open.value && width >= 60,
		sideWidth: (width: number) => Math.max(20, Math.round(width * 0.4)),
	};
}

describe("SplitView", () => {
	test("render merges two columns line by line with a vertical border", () => {
		const main = new Text("left one\nleft two", 0, 0);
		const side = new Text("right", 0, 0);
		const open = { value: true };
		const view = new SplitView(main, side, {
			columns: () => 30,
			rows: () => 2,
			open: () => open.value,
			sideWidth: () => 10,
		});
		expect(view.widths(30)).toEqual([19, 10]);
		expect(view.render(30).map(stripTerminalSequences)).toEqual([
			`${"left one".padEnd(19)}│${"right".padEnd(10)}`,
			`${"left two".padEnd(19)}│${"".padEnd(10)}`,
		]);
		open.value = false;
		expect(view.widths(30)).toEqual([30, 0]);
		expect(view.render(30).map((line) => line.trimEnd())).toEqual(["left one", "left two"]);
	});

	test("in the full-screen layout, the chat column keeps its scroll view, editor cursor and width", async () => {
		const terminal = new RecordingTerminal(80, 12);
		const tui = new TuiAltScreen(terminal);
		const transcript = new ScrollView(
			new Text(Array.from({ length: 30 }, (_, index) => `message ${index + 1}`).join("\n"), 0, 0),
			{ follow: "end", primary: true },
		);
		const editor = new FakeEditor();
		const chat = new VStack([
			{ component: transcript, basis: 0, grow: 1, shrink: 1, minSize: 1 },
			{ component: editor, basis: "auto", grow: 0, shrink: 0 },
		]);
		const pane = new FakePane(() => terminal.rows);
		const open = { value: true };
		tui.setLayoutRoot(new SplitView(chat, pane, splitOptions(terminal, open)));
		tui.start();
		await terminal.waitForRender();

		// 80 columns: the pane takes 32 (40%), the border one, the chat the other 47.
		const viewport = terminal.getViewport();
		expect(viewport).toHaveLength(12);
		expect(viewport[0]).toBe(`${"message 22".padEnd(47)}│${"pane 0".padEnd(32, ".")}`);
		expect(viewport[8]).toBe(`${"message 30".padEnd(47)}│${"pane 8".padEnd(32, ".")}`);
		expect(viewport[9]).toBe(`${"─".repeat(47)}│${"pane 9".padEnd(32, ".")}`);
		expect(viewport[10]).toBe(`${"> hello".padEnd(47)}│${"pane 10".padEnd(32, ".")}`);
		expect(pane.renders.at(-1)).toBe(32);
		// The hardware cursor sits after the editor's text in the left column, not in the pane.
		expect(terminal.getCursorPosition()).toEqual({ x: 7, y: 10 });

		// Typing redraws only the editor row; the pane and the border stay as they were.
		editor.text = "hello world";
		terminal.writes = [];
		tui.requestRender();
		await terminal.waitForRender();
		const frame = terminal.writes.join("");
		expect([...frame.matchAll(/\u001b\[(\d+);1H/g)].map((match) => match[1])).toEqual(["11"]);
		expect(terminal.getViewport()[10]).toBe(`${"> hello world".padEnd(47)}│${"pane 10".padEnd(32, ".")}`);
		expect(terminal.getCursorPosition()).toEqual({ x: 13, y: 10 });

		// Closing the pane gives the chat the full width back.
		open.value = false;
		tui.requestRender();
		await terminal.waitForRender();
		expect(terminal.getViewport()[10]!.trimEnd()).toBe("> hello world");
		expect(terminal.getViewport()[0]!.trimEnd()).toBe("message 22");
		expect(terminal.getCursorPosition()).toEqual({ x: 13, y: 10 });

		// Narrower than the split's minimum, an open pane stays hidden.
		open.value = true;
		terminal.resize(50, 12);
		await terminal.waitForRender();
		expect(terminal.getViewport()[10]!.trimEnd()).toBe("> hello world");
		expect(terminal.getViewport().some((line) => line.includes("pane"))).toBe(false);
		tui.stop();
	});

	test("overlays still draw over both columns", async () => {
		const terminal = new VirtualTerminal(80, 8);
		const tui = new TuiAltScreen(terminal);
		const pane = new FakePane(() => terminal.rows);
		tui.setLayoutRoot(new SplitView(new Text("chat", 0, 0), pane, splitOptions(terminal, { value: true })));
		tui.start();
		tui.showOverlay(new Text("OVERLAY", 0, 0), { anchor: "center", width: 20 });
		await terminal.waitForRender();
		const viewport = terminal.getViewport();
		expect(viewport.some((line) => line.includes("OVERLAY"))).toBe(true);
		expect(viewport[0]).toContain("│pane 0");
		tui.stop();
	});
});
