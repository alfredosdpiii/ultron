/**
 * Pi's interactive-mode commands and help text for the native TUI: `/name`, `/session`, `/copy`, `/export`,
 * `/hotkeys`, `/changelog`, `/quit`, and the startup header with keybinding hints.
 *
 * Each command does what Pi's does, over the native Session: reads go through the worker's tree read (as Pi
 * entries, so Pi's stats, export and HTML renderer apply unchanged) and writes through `SessionControl`.
 */

import { writeFileSync } from "node:fs";
import { basename, resolve } from "node:path";
import type { LaneSnapshot } from "@ultron/agent-core";
import { type Component, Markdown, Spacer, Text } from "@ultron/tui";
import { APP_NAME, VERSION } from "../config.ts";
import { generateHtml } from "../core/export-html/index.ts";
import { CURRENT_SESSION_VERSION, type SessionHeader } from "../core/session-manager.ts";
import { formatTokens } from "../modes/interactive/components/footer.ts";
import { keyDisplayText, keyHint, keyText, rawKeyHint } from "../modes/interactive/components/keybinding-hints.ts";
import { getMarkdownTheme, theme } from "../modes/interactive/theme/theme.ts";
import { getChangelogPath, normalizeChangelogLinks, parseChangelog } from "../utils/changelog.ts";
import { copyToClipboard } from "../utils/clipboard.ts";
import { ExperimentalChatView } from "./client-tui-chat.ts";
import type { PiSessionView } from "./pi-session-view.ts";
import type { ModelSummary } from "./services/models.ts";
import type { SessionControl } from "./services/session-control.ts";
import type { SlashCommandContribution } from "./services/slash-commands.ts";
import { splashLines } from "./ultron-logo.ts";

/** What the commands need from the TUI. */
export interface NativeCommandHost {
	showStatus(text: string): void;
	/** Append a component to the transcript (Pi adds command output to its chat container). */
	notice(component: Component): void;
	snapshot(): LaneSnapshot | undefined;
	sessionId(): string | undefined;
	control(): SessionControl | undefined;
	/** The Session's worker tree as Pi entries; shows an error and returns undefined when unavailable. */
	readSessionView(): Promise<PiSessionView | undefined>;
	/** The selected model's catalog record (for its context window). */
	currentModel(): ModelSummary | undefined;
	sessionName(): string | undefined;
	setSessionName(name: string | undefined): void;
	quit(): void;
	/** Copy to the clipboard; injectable so tests do not touch the real clipboard. */
	copy?(text: string): Promise<void>;
}

export function nativeCommands(host: NativeCommandHost): SlashCommandContribution[] {
	return [
		{
			name: "name",
			description: "Set session display name",
			argumentHint: "<name>",
			async run(args, context) {
				const name = args.trim();
				if (!name) {
					const current = host.sessionName();
					if (current) host.notice(new Text(theme.fg("dim", `Session name: ${current}`), 1, 0));
					else host.showStatus("Usage: /name <name>");
					return undefined;
				}
				const control = host.control();
				if (control === undefined) throw new Error("No Session is attached");
				await control.setName(name, context);
				host.setSessionName(name);
				host.notice(new Text(theme.fg("dim", `Session name set: ${name}`), 1, 0));
				return undefined;
			},
		},
		{
			name: "session",
			description: "Show session info and stats",
			async run() {
				const view = await host.readSessionView();
				const sessionId = host.sessionId();
				if (view === undefined || sessionId === undefined) return undefined;
				host.notice(new Text(sessionInfo(view, sessionId, host.sessionName(), host.currentModel()), 1, 0));
				return undefined;
			},
		},
		{
			name: "copy",
			description: "Copy last agent message to clipboard",
			async run() {
				await copyLastAssistantMessage(host);
				return undefined;
			},
		},
		{
			name: "export",
			description: "Export session (HTML default, or specify path: .html/.jsonl)",
			argumentHint: "[path]",
			async run(args) {
				const view = await host.readSessionView();
				const sessionId = host.sessionId();
				if (view === undefined || sessionId === undefined) return undefined;
				try {
					const path = exportSession(view, sessionId, process.cwd(), pathArgument(args));
					host.showStatus(`Session exported to: ${path}`);
				} catch (error) {
					host.showStatus(
						`Error: Failed to export session: ${error instanceof Error ? error.message : String(error)}`,
					);
				}
				return undefined;
			},
		},
		{
			name: "hotkeys",
			description: "Show all keyboard shortcuts",
			run() {
				host.notice(markdownNotice("Keyboard Shortcuts", hotkeysMarkdown()));
				return undefined;
			},
		},
		{
			name: "changelog",
			description: "Show changelog entries",
			run() {
				const entries = parseChangelog(getChangelogPath());
				const markdown =
					entries.length > 0
						? entries
								.reverse()
								.map((entry) => normalizeChangelogLinks(entry.content, entry))
								.join("\n\n")
						: "No changelog entries found.";
				host.notice(markdownNotice("What's New", markdown));
				return undefined;
			},
		},
		{
			name: "quit",
			description: `Quit ${APP_NAME}`,
			run() {
				host.quit();
				return undefined;
			},
		},
	];
}

/** Pi's `/copy` and Ctrl+X: the last assistant message's text to the clipboard. */
export async function copyLastAssistantMessage(host: NativeCommandHost): Promise<void> {
	const snapshot = host.snapshot();
	const text = snapshot === undefined ? undefined : ExperimentalChatView.lastAssistantText(snapshot);
	if (!text) {
		host.showStatus("Error: No agent messages to copy yet.");
		return;
	}
	try {
		await (host.copy ?? copyToClipboard)(text);
		host.showStatus("Copied last agent message to clipboard");
	} catch (error) {
		host.showStatus(`Error: ${error instanceof Error ? error.message : String(error)}`);
	}
}

/** Pi's `/session` text. */
export function sessionInfo(
	view: PiSessionView,
	sessionId: string,
	name: string | undefined,
	model: ModelSummary | undefined,
): string {
	const stats = view.stats(sessionId, model?.model);
	let info = `${theme.bold("Session Info")}\n\n`;
	if (name) info += `${theme.fg("dim", "Name:")} ${name}\n`;
	info += `${theme.fg("dim", "File:")} ${stats.sessionFile ?? "In-memory"}\n`;
	info += `${theme.fg("dim", "ID:")} ${stats.sessionId}\n\n`;
	info += `${theme.bold("Messages")}\n`;
	info += `${theme.fg("dim", "Total:")} ${stats.totalMessages}\n`;
	info += `${theme.fg("dim", "User:")} ${stats.userMessages}\n`;
	info += `${theme.fg("dim", "Assistant:")} ${stats.assistantMessages}\n`;
	info += `${theme.fg("dim", "Tools:")} ${stats.toolCalls} calls, ${stats.toolResults} results\n\n`;
	info += `${theme.bold("Tokens")}\n`;
	const { input, cacheRead, cacheWrite } = stats.tokens;
	const promptTokens = input + cacheRead + cacheWrite;
	info += `${theme.fg("dim", "Input:")} ${promptTokens.toLocaleString()}\n`;
	if (promptTokens > 0 && (cacheRead > 0 || cacheWrite > 0)) {
		const hitRate = theme.fg("dim", `(${((cacheRead / promptTokens) * 100).toFixed(1)}%)`);
		info += `  ${theme.fg("dim", "Cached:")} ${cacheRead.toLocaleString()} ${hitRate}\n`;
		info += `  ${theme.fg("dim", "Uncached:")} ${(input + cacheWrite).toLocaleString()}\n`;
	}
	info += `${theme.fg("dim", "Output:")} ${stats.tokens.output.toLocaleString()}\n`;
	info += `${theme.fg("dim", "Total:")} ${stats.tokens.total.toLocaleString()}\n`;
	const context = stats.contextUsage;
	if (context !== undefined) {
		const used = context.tokens === null ? "?" : formatTokens(context.tokens);
		const percent = context.percent === null ? "" : ` (${context.percent.toFixed(1)}%)`;
		info += `\n${theme.bold("Context")}\n${theme.fg("dim", "Used:")} ${used}/${formatTokens(context.contextWindow)}${percent}\n`;
	}
	if (stats.cost > 0) info += `\n${theme.bold("Cost")}\n${theme.fg("dim", "Total:")} $${stats.cost.toFixed(3)}`;
	return info.trimEnd();
}

/** Pi's `/export`: HTML through Pi's exporter, or the Pi-format JSONL, from the worker's tree read. */
export function exportSession(view: PiSessionView, sessionId: string, cwd: string, outputPath?: string): string {
	const header: SessionHeader = {
		type: "session",
		version: CURRENT_SESSION_VERSION,
		id: sessionId,
		timestamp: new Date(view.entries[0]?.timestamp ?? Date.now()).toISOString(),
		cwd,
	};
	const stem = view.sessionFile ? basename(view.sessionFile).replace(/\.[^.]+$/u, "") : sessionId;
	if (outputPath?.endsWith(".jsonl")) {
		const path = resolve(cwd, outputPath);
		writeFileSync(path, `${[header, ...view.entries].map((entry) => JSON.stringify(entry)).join("\n")}\n`, "utf8");
		return path;
	}
	const html = generateHtml({ header, entries: view.entries, leafId: view.leafId }, theme.name);
	const path = resolve(cwd, outputPath ?? `${APP_NAME}-session-${stem}.html`);
	writeFileSync(path, html, "utf8");
	return path;
}

/** Pi's path argument: the first word, or a quoted string. */
export function pathArgument(args: string): string | undefined {
	const text = args.trimStart();
	if (!text) return undefined;
	const quote = text[0];
	if (quote === '"' || quote === "'") {
		const end = text.indexOf(quote, 1);
		return end < 0 ? undefined : text.slice(1, end);
	}
	const space = text.search(/\s/u);
	return space < 0 ? text : text.slice(0, space);
}

function markdownNotice(title: string, markdown: string): Component {
	const border = (): Component => ({
		render: (width) => [theme.fg("border", "─".repeat(Math.max(1, width)))],
		invalidate() {},
	});
	const parts: Component[] = [
		border(),
		new Text(theme.bold(theme.fg("accent", title)), 1, 0),
		new Spacer(1),
		new Markdown(markdown.trim(), 1, 1, getMarkdownTheme()),
		border(),
	];
	return {
		render: (width) => parts.flatMap((part) => part.render(width)),
		invalidate: () => {
			for (const part of parts) part.invalidate();
		},
	};
}

/** Pi's `/hotkeys` table, with the native TUI's own keys (RLM and Jev panels, history search). */
export function hotkeysMarkdown(): string {
	const k = keyDisplayText;
	return `
**Navigation**
| Key | Action |
|-----|--------|
| \`${k("tui.editor.cursorUp")}\` / \`${k("tui.editor.cursorDown")}\` | Move cursor / browse prompt history |
| \`${k("app.history.search")}\` | Reverse-search prompt history |
| \`${k("tui.editor.cursorWordLeft")}\` / \`${k("tui.editor.cursorWordRight")}\` | Move by word |
| \`${k("tui.editor.cursorLineStart")}\` | Start of line |
| \`${k("tui.editor.cursorLineEnd")}\` | End of line |
| \`${k("tui.editor.jumpForward")}\` | Jump forward to character |
| \`${k("tui.editor.jumpBackward")}\` | Jump backward to character |
| \`${k("tui.editor.pageUp")}\` / \`${k("tui.editor.pageDown")}\` | Scroll by page |

**Editing**
| Key | Action |
|-----|--------|
| \`${k("tui.input.submit")}\` | Send message (steers while a turn runs) |
| \`${k("tui.input.newLine")}\` | New line |
| \`${k("tui.editor.deleteWordBackward")}\` | Delete word backwards |
| \`${k("tui.editor.deleteWordForward")}\` | Delete word forwards |
| \`${k("tui.editor.deleteToLineStart")}\` | Delete to start of line |
| \`${k("tui.editor.deleteToLineEnd")}\` | Delete to end of line |
| \`${k("tui.editor.yank")}\` | Paste the most-recently-deleted text |
| \`${k("tui.editor.yankPop")}\` | Cycle through the deleted text after pasting |
| \`${k("tui.editor.undo")}\` | Undo |

**Other**
| Key | Action |
|-----|--------|
| \`${k("tui.input.tab")}\` | Path completion / accept autocomplete |
| \`${k("app.interrupt")}\` | Cancel autocomplete / abort the turn |
| \`${k("app.clear")}\` | Clear editor (first) / exit (second) |
| \`${k("app.exit")}\` | Exit (when editor is empty) |
| \`${k("app.suspend")}\` | Suspend to background |
| \`${k("app.thinking.cycle")}\` | Cycle thinking level |
| \`${k("app.model.cycleForward")}\` / \`${k("app.model.cycleBackward")}\` | Cycle models |
| \`${k("app.model.select")}\` | Open model selector |
| \`${k("app.tools.expand")}\` | Toggle tool output expansion |
| \`${k("app.thinking.toggle")}\` | Toggle thinking block visibility |
| \`${k("app.editor.external")}\` | Edit message in external editor |
| \`${k("app.message.copy")}\` | Copy last assistant message |
| \`${k("app.message.followUp")}\` | Queue follow-up message |
| \`${k("app.message.dequeue")}\` | Restore queued messages |
| \`${k("app.clipboard.pasteImage")}\` | Paste image or text from clipboard |
| \`${k("app.rlm.toggle")}\` | Toggle the live RLM panel |
| \`${k("app.jev.toggle")}\` | Toggle the Jev decisions panel |
| \`${k("app.rlm.focus")}\` | Open the full-screen RLM graph |
| \`${k("app.jev.notes.toggle")}\` | Expand or fold Jev's memory notes |
| \`/\` | Slash commands |
| \`!\` | Run bash command |
| \`!!\` | Run bash command (excluded from context) |
`;
}

/** Pi's startup header: compact hints, expanded to the full list by the tool expansion key. */
/** Whether and how large to draw the splash logo; omitted means no splash (tests, embedded uses). */
export interface SplashOptions {
	/** Terminal height in rows, when known. */
	rows(): number | undefined;
	/** False when the user asked for a quiet startup. */
	enabled(): boolean;
}

export class StartupHeader implements Component {
	#expanded = false;
	#details = "";
	readonly #text = new Text("", 1, 0);
	readonly #splash: SplashOptions | undefined;

	constructor(splash?: SplashOptions) {
		this.#splash = splash;
	}

	/** Lines under the hints (server and Session ids). */
	setDetails(details: string): void {
		this.#details = details;
		this.#update();
	}

	setExpanded(expanded: boolean): void {
		this.#expanded = expanded;
		this.#update();
	}

	render(width: number): string[] {
		if (this.#text.render(width).length === 0) this.#update();
		const text = this.#text.render(width);
		const splash = this.#splash?.enabled() ? splashLines(width, this.#splash.rows()) : [];
		return splash.length === 0 ? text : [...splash.map((line) => theme.fg("accent", line)), "", ...text];
	}

	invalidate(): void {
		this.#update();
		this.#text.invalidate();
	}

	#update(): void {
		const logo = theme.bold(theme.fg("accent", APP_NAME)) + theme.fg("dim", ` v${VERSION}`);
		const hints = this.#expanded
			? [
					keyHint("app.interrupt", "to interrupt"),
					keyHint("app.clear", "to clear"),
					rawKeyHint(`${keyText("app.clear")} twice`, "to exit"),
					keyHint("app.exit", "to exit (empty)"),
					keyHint("app.suspend", "to suspend"),
					rawKeyHint(
						`${keyText("tui.editor.cursorUp")}/${keyText("tui.editor.cursorDown")}`,
						"for prompt history",
					),
					keyHint("app.history.search", "to search prompt history"),
					keyHint("app.thinking.cycle", "to cycle thinking level"),
					rawKeyHint(
						`${keyText("app.model.cycleForward")}/${keyText("app.model.cycleBackward")}`,
						"to cycle models",
					),
					keyHint("app.model.select", "to select model"),
					keyHint("app.tools.expand", "to expand tools"),
					keyHint("app.thinking.toggle", "to expand thinking"),
					keyHint("app.editor.external", "for external editor"),
					rawKeyHint("/", "for commands"),
					rawKeyHint("!", "to run bash"),
					rawKeyHint("!!", "to run bash (no context)"),
					keyHint("app.message.followUp", "to queue follow-up"),
					keyHint("app.message.dequeue", "to edit all queued messages"),
					keyHint("app.clipboard.pasteImage", "to paste image (with text fallback)"),
					keyHint("app.rlm.toggle", "for the RLM panel"),
					keyHint("app.jev.toggle", "for the Jev panel"),
				].join("\n")
			: [
					keyHint("app.interrupt", "interrupt"),
					rawKeyHint(`${keyText("app.clear")}/${keyText("app.exit")}`, "clear/exit"),
					rawKeyHint(`${keyText("tui.editor.cursorUp")}/${keyText("app.history.search")}`, "history"),
					rawKeyHint("/", "commands"),
					rawKeyHint("!", "bash"),
					keyHint("app.tools.expand", "more"),
				].join(theme.fg("muted", " · "));
		const details = this.#details ? `\n${this.#details}` : "";
		this.#text.setText(`${logo}\n${hints}${details}`);
	}
}
