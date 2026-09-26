/**
 * Transcript rendering for the `rlm` tool: the cell's Python, syntax-highlighted, and its output.
 *
 * Follows Pi's built-in renderers (bash, write): collapsed, the code shows its first lines and the output its
 * last lines, each with a hint for the expand key (Ctrl+O by default); expanded, both are shown in full.
 * Without this the transcript showed only the word "rlm" and the output, never the code that produced it.
 */

import { Container, Text, truncateToWidth } from "@ultron/tui";
import type { ToolRenderContext, ToolRenderResultOptions } from "../core/extensions/types.ts";
import { getTextOutput } from "../core/tools/render-utils.ts";
import type { ToolRenderers } from "../core/tools/renderers/index.ts";
import { keyHint } from "../modes/interactive/components/keybinding-hints.ts";
import { truncateToVisualLines } from "../modes/interactive/components/visual-truncate.ts";
import { highlightCode, theme } from "../modes/interactive/theme/theme.ts";

/** Code lines shown while collapsed. */
export const RLM_CODE_PREVIEW_LINES = 12;
/** Output visual lines shown while collapsed (the tail, where results and tracebacks end). */
export const RLM_OUTPUT_PREVIEW_LINES = 10;

interface RlmRenderState {
	code?: string;
	highlighted?: string[];
}

function codeOf(args: unknown): string | undefined {
	if (typeof args !== "object" || args === null) return undefined;
	const code = (args as { code?: unknown }).code;
	return typeof code === "string" ? code : undefined;
}

function highlighted(state: RlmRenderState, code: string): string[] {
	if (state.code !== code || state.highlighted === undefined) {
		state.code = code;
		state.highlighted = highlightCode(code.replace(/\t/g, "    "), "python");
	}
	return state.highlighted;
}

function renderCall(args: unknown, _theme: unknown, context: ToolRenderContext<RlmRenderState>): Container {
	const container = new Container();
	const code = codeOf(args);
	const lineCount = code === undefined ? 0 : code.split("\n").length;
	const meta = code === undefined ? "" : theme.fg("muted", ` python · ${lineCount} line${lineCount === 1 ? "" : "s"}`);
	container.addChild(new Text(theme.fg("toolTitle", theme.bold("rlm")) + meta, 0, 0));
	if (code === undefined || code.trim().length === 0) {
		if (!context.argsComplete) container.addChild(new Text(theme.fg("toolOutput", "..."), 0, 0));
		return container;
	}
	const lines = highlighted(context.state, code.replace(/\s+$/u, ""));
	const shown = context.expanded ? lines : lines.slice(0, RLM_CODE_PREVIEW_LINES);
	const gutterWidth = String(lines.length).length;
	const body = shown
		.map((line, index) => `${theme.fg("dim", `${String(index + 1).padStart(gutterWidth)} │`)} ${line}`)
		.join("\n");
	let text = `\n${body}`;
	const hidden = lines.length - shown.length;
	if (hidden > 0) {
		text += `\n${theme.fg("muted", `... (${hidden} more line${hidden === 1 ? "" : "s"},`)} ${keyHint("app.tools.expand", "to expand")}${theme.fg("muted", ")")}`;
	}
	container.addChild(new Text(text, 0, 0));
	return container;
}

function renderResult(
	result: { content: Array<{ type: string; text?: string; data?: string; mimeType?: string }> },
	options: ToolRenderResultOptions,
	_theme: unknown,
	context: ToolRenderContext<RlmRenderState>,
): Container {
	const container = new Container();
	const output = getTextOutput(result, context.showImages).replace(/\s+$/u, "");
	if (output.length === 0) {
		if (options.isPartial) container.addChild(new Text(`\n${theme.fg("muted", "running…")}`, 0, 0));
		return container;
	}
	const color = context.isError ? "error" : "toolOutput";
	const styled = output
		.split("\n")
		.map((line) => theme.fg(color, line))
		.join("\n");
	const label = theme.fg("muted", context.isError ? "error" : options.isPartial ? "output (running)" : "output");
	if (options.expanded) {
		container.addChild(new Text(`\n${label}\n${styled}`, 0, 0));
		return container;
	}
	let cachedWidth: number | undefined;
	let cachedLines: string[] = [];
	container.addChild({
		render(width: number): string[] {
			if (cachedWidth !== width) {
				const preview = truncateToVisualLines(styled, RLM_OUTPUT_PREVIEW_LINES, width);
				cachedWidth = width;
				cachedLines = ["", label];
				if (preview.skippedCount > 0) {
					const skipped = `... (${preview.skippedCount} earlier line${preview.skippedCount === 1 ? "" : "s"},`;
					const hint = `${theme.fg("muted", skipped)} ${keyHint("app.tools.expand", "to expand")}${theme.fg("muted", ")")}`;
					cachedLines.push(truncateToWidth(hint, width, "..."));
				}
				cachedLines.push(...preview.visualLines);
			}
			return cachedLines;
		},
		invalidate() {
			cachedWidth = undefined;
		},
	});
	return container;
}

export const rlmToolRenderers: ToolRenderers = { renderCall, renderResult };
