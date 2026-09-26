/**
 * Reverse incremental search over prompt history (bash's Ctrl+R, shown as a list).
 *
 * Typing narrows the matches (every term must occur, case-insensitive), most recent first. Up/Down or the
 * search key again (Alt+R by default; Ctrl+R also works while searching) move between matches, Enter or Tab
 * puts the selected entry in the editor, and Esc or Ctrl+C cancels and restores the draft.
 */

import {
	type Component,
	type Focusable,
	getKeybindings,
	Input,
	matchesKey,
	truncateToWidth,
	visibleWidth,
} from "@ultron/tui";
import { theme } from "../modes/interactive/theme/theme.ts";
import { searchPromptHistory } from "./prompt-history.ts";

const MAX_VISIBLE_MATCHES = 8;

export class PromptHistorySearchComponent implements Component, Focusable {
	readonly #history: readonly string[];
	readonly #input: Input;
	readonly #onAccept: (text: string) => void;
	readonly #onCancel: () => void;
	#matches: string[];
	#selected = 0;
	#focused = false;

	constructor(history: readonly string[], onAccept: (text: string) => void, onCancel: () => void, query = "") {
		this.#history = history;
		this.#onAccept = onAccept;
		this.#onCancel = onCancel;
		this.#input = new Input({ prompt: "" });
		this.#input.setValue(query);
		this.#matches = searchPromptHistory(history, query);
	}

	get focused(): boolean {
		return this.#focused;
	}

	set focused(value: boolean) {
		this.#focused = value;
		this.#input.focused = value;
	}

	get query(): string {
		return this.#input.getValue();
	}

	get matches(): readonly string[] {
		return this.#matches;
	}

	get selected(): string | undefined {
		return this.#matches[this.#selected];
	}

	handleInput(data: string): void {
		const keybindings = getKeybindings();
		if (keybindings.matches(data, "tui.select.cancel")) {
			this.#onCancel();
			return;
		}
		if (keybindings.matches(data, "tui.select.confirm") || matchesKey(data, "tab")) {
			const selected = this.selected;
			if (selected === undefined) this.#onCancel();
			else this.#onAccept(selected);
			return;
		}
		if (
			keybindings.matches(data, "app.history.search") ||
			matchesKey(data, "ctrl+r") ||
			keybindings.matches(data, "tui.select.up")
		) {
			this.#move(1);
			return;
		}
		if (matchesKey(data, "ctrl+s") || keybindings.matches(data, "tui.select.down")) {
			this.#move(-1);
			return;
		}
		const before = this.#input.getValue();
		this.#input.handleInput(data);
		const after = this.#input.getValue();
		if (after !== before) {
			this.#matches = searchPromptHistory(this.#history, after);
			this.#selected = 0;
		}
	}

	invalidate(): void {
		this.#input.invalidate();
	}

	render(width: number): string[] {
		const inner = Math.max(1, width - 2);
		const label = theme.fg("accent", "history search: ");
		const inputLines = this.#input.render(Math.max(1, inner - visibleWidth(label)));
		const count =
			this.#matches.length === 0
				? theme.fg("warning", "no match")
				: theme.fg("dim", `${this.#selected + 1}/${this.#matches.length}`);
		const lines = [theme.fg("border", "─".repeat(width)), ` ${label}${inputLines[0] ?? ""}`, ` ${count}`];
		const start = Math.min(
			Math.max(0, this.#selected - Math.floor(MAX_VISIBLE_MATCHES / 2)),
			Math.max(0, this.#matches.length - MAX_VISIBLE_MATCHES),
		);
		const terms = this.query.toLowerCase().split(/\s+/u).filter(Boolean);
		for (let index = start; index < Math.min(this.#matches.length, start + MAX_VISIBLE_MATCHES); index++) {
			const oneLine = this.#matches[index]!.replace(/\s+/gu, " ");
			const selected = index === this.#selected;
			const prefix = selected ? theme.fg("accent", "→ ") : "  ";
			const body = truncateToWidth(highlight(oneLine, terms, selected), Math.max(1, inner - 2), "…");
			lines.push(` ${prefix}${body}`);
		}
		const hint = theme.fg("dim", "↑↓ move · enter/tab use · esc cancel");
		lines.push(` ${truncateToWidth(hint, inner, "")}`);
		lines.push(theme.fg("border", "─".repeat(width)));
		return lines;
	}

	#move(delta: 1 | -1): void {
		if (this.#matches.length === 0) return;
		this.#selected = Math.min(this.#matches.length - 1, Math.max(0, this.#selected + delta));
	}
}

function highlight(text: string, terms: readonly string[], selected: boolean): string {
	const base = (value: string) => (selected ? theme.fg("accent", value) : value);
	if (terms.length === 0) return base(text);
	const lower = text.toLowerCase();
	const marks = new Array<boolean>(text.length).fill(false);
	for (const term of terms) {
		let from = lower.indexOf(term);
		while (from !== -1) {
			for (let index = from; index < from + term.length; index++) marks[index] = true;
			from = lower.indexOf(term, from + term.length);
		}
	}
	let out = "";
	let run = "";
	let runMarked = marks[0] ?? false;
	const flush = () => {
		if (run.length === 0) return;
		out += runMarked ? theme.bold(theme.fg("warning", run)) : base(run);
		run = "";
	};
	for (let index = 0; index < text.length; index++) {
		if (marks[index] !== runMarked) {
			flush();
			runMarked = marks[index]!;
		}
		run += text[index];
	}
	flush();
	return out;
}
