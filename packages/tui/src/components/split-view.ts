import { LAYOUT_NODE, type StackLayoutEntry, type StackLayoutNode } from "../layout-node.ts";
import { type Component, Container, compositeTuiLine } from "../tui.ts";

export interface SplitViewOptions {
	/** Terminal columns (the split is the layout root, so its width is the terminal's). */
	columns(): number;
	/** Terminal rows: the border and the side pane fill the full height. */
	rows(): number;
	/** Whether the side pane shows at this width (it is hidden, and the main column full width, otherwise). */
	open(width: number): boolean;
	/** Side pane width (without the border) for this total width. */
	sideWidth(width: number): number;
	/** Styles the vertical border between the columns. */
	borderStyle?: (text: string) => string;
}

/** The one-column vertical border, as tall as the terminal. */
class SplitBorder implements Component {
	private readonly options: SplitViewOptions;

	constructor(options: SplitViewOptions) {
		this.options = options;
	}

	render(): string[] {
		const glyph = this.options.borderStyle?.("│") ?? "│";
		return Array.from({ length: Math.max(1, this.options.rows()) }, () => glyph);
	}

	invalidate(): void {}
}

/**
 * Two columns side by side with a vertical border: the main column (its own layout: scroll views, docks, the
 * focused editor) and a side pane on the right. As a layout node it is an hstack, so the TUI lays out, clips and
 * hit-tests each column on its own and the editor's cursor marker survives compositing; `render` merges the two
 * columns line by line for callers that render without the layout engine.
 */
export class SplitView extends Container {
	readonly #main: Component;
	readonly #side: Component;
	readonly #border: SplitBorder;
	readonly #options: SplitViewOptions;

	constructor(main: Component, side: Component, options: SplitViewOptions) {
		super();
		this.#main = main;
		this.#side = side;
		this.#options = options;
		this.#border = new SplitBorder(options);
		this.addChild(main);
		this.addChild(this.#border);
		this.addChild(side);
	}

	/** Column widths for a total width: [main, side]; side is 0 while the pane is closed. */
	widths(width: number): readonly [number, number] {
		const total = Math.max(1, Math.floor(width));
		if (!this.#options.open(total)) return [total, 0];
		const side = Math.max(1, Math.min(total - 2, Math.floor(this.#options.sideWidth(total))));
		return [total - side - 1, side];
	}

	[LAYOUT_NODE](): StackLayoutNode {
		const [main, side] = this.widths(this.#options.columns());
		const entries: StackLayoutEntry[] =
			side === 0
				? [{ component: this.#main, basis: main, grow: 1, shrink: 1 }]
				: [
						{ component: this.#main, basis: main, grow: 1, shrink: 1, minSize: 1 },
						{ component: this.#border, basis: 1, grow: 0, shrink: 0, minSize: 1 },
						{ component: this.#side, basis: side, grow: 0, shrink: 0, minSize: 1 },
					];
		return { type: "hstack", entries, gap: 0, align: "stretch" };
	}

	override render(width: number): string[] {
		const total = Math.max(1, Math.floor(width));
		const [mainWidth, sideWidth] = this.widths(total);
		const main = this.#main.render(mainWidth);
		if (sideWidth === 0) return main;
		const side = this.#side.render(sideWidth);
		const border = this.#options.borderStyle?.("│") ?? "│";
		const height = Math.max(main.length, side.length);
		const lines: string[] = [];
		for (let row = 0; row < height; row++) {
			let line = compositeTuiLine("", main[row] ?? "", 0, mainWidth, total);
			line = compositeTuiLine(line, border, mainWidth, 1, total);
			lines.push(compositeTuiLine(line, side[row] ?? "", mainWidth + 1, sideWidth, total));
		}
		return lines;
	}

	override invalidate(): void {
		this.#main.invalidate();
		this.#side.invalidate();
	}
}
