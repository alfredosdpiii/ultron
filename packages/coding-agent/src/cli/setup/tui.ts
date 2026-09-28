/**
 * The terminal side of `ultron setup`: a transcript of what each step found and did, with the current question
 * below it. Questions use Pi's components (the `/login` provider picker and login dialog, bordered selectors) and
 * a masked input for secrets. Esc answers a question with "skip/back"; Ctrl+C quits the wizard.
 */

import {
	type Component,
	Container,
	type Focusable,
	fuzzyFilter,
	getKeybindings,
	Input,
	isFocusable,
	Loader,
	matchesKey,
	type SelectItem,
	SelectList,
	Spacer,
	Text,
	type TUI,
} from "@ultron/tui";
import { type LoginRuntime, runProviderLogin } from "../../experimental/client-tui-auth.ts";
import { DynamicBorder } from "../../modes/interactive/components/dynamic-border.ts";
import { keyHint, rawKeyHint } from "../../modes/interactive/components/keybinding-hints.ts";
import {
	type AuthSelectorProvider,
	OAuthSelectorComponent,
} from "../../modes/interactive/components/oauth-selector.ts";
import { getSelectListTheme, theme } from "../../modes/interactive/theme/theme.ts";
import { type Choice, type InputOptions, SetupQuit, type SetupUi, type Tone } from "./wizard.ts";

const MAX_VISIBLE = 10;

function color(tone: Tone, text: string): string {
	switch (tone) {
		case "success":
			return theme.fg("success", text);
		case "warning":
			return theme.fg("warning", text);
		case "error":
			return theme.fg("error", text);
		case "dim":
			return theme.fg("dim", text);
		default:
			return theme.fg("text", text);
	}
}

/** A bordered list; with more than a screenful of choices, typing filters it. */
class SetupSelect extends Container implements Focusable {
	private readonly search: Input | undefined;
	private list: SelectList;
	private readonly listIndex: number;
	private readonly items: SelectItem[];
	private readonly onPick: (index: number) => void;
	private readonly onCancel: () => void;
	private _focused = false;

	get focused(): boolean {
		return this._focused;
	}

	set focused(value: boolean) {
		this._focused = value;
		if (this.search) this.search.focused = value;
	}

	constructor(
		title: string,
		labels: readonly { label: string; description?: string }[],
		onPick: (index: number) => void,
		onCancel: () => void,
		description?: string,
	) {
		super();
		this.onPick = onPick;
		this.onCancel = onCancel;
		this.items = labels.map((entry, index) => ({
			value: String(index),
			label: entry.label,
			...(entry.description ? { description: entry.description } : {}),
		}));
		this.addChild(new DynamicBorder());
		this.addChild(new Text(theme.fg("accent", theme.bold(title)), 1, 0));
		if (description) this.addChild(new Text(theme.fg("text", description), 1, 0));
		this.addChild(new Spacer(1));
		if (this.items.length > MAX_VISIBLE) {
			this.search = new Input({ prompt: "Search: " });
			this.search.onSubmit = () => this.list.handleInput("\r");
			this.addChild(this.search);
		}
		this.list = this.buildList(this.items);
		this.listIndex = this.children.length;
		this.addChild(this.list);
		this.addChild(new Spacer(1));
		this.addChild(
			new Text(
				`${rawKeyHint("↑↓", "navigate")}  ${keyHint("tui.select.confirm", "select")}  ${rawKeyHint("esc", "skip/back")}  ${rawKeyHint("ctrl+c", "quit setup")}`,
				1,
				0,
			),
		);
		this.addChild(new DynamicBorder());
	}

	private buildList(items: SelectItem[]): SelectList {
		const list = new SelectList(items, Math.min(MAX_VISIBLE, Math.max(1, items.length)), getSelectListTheme(), {
			minPrimaryColumnWidth: 16,
			maxPrimaryColumnWidth: 48,
		});
		list.onSelect = (item) => this.onPick(Number(item.value));
		list.onCancel = () => this.onCancel();
		return list;
	}

	handleInput(data: string): void {
		const kb = getKeybindings();
		const navigation =
			kb.matches(data, "tui.select.up") ||
			kb.matches(data, "tui.select.down") ||
			kb.matches(data, "tui.select.confirm") ||
			kb.matches(data, "tui.select.cancel");
		if (navigation || !this.search) {
			this.list.handleInput(data);
			return;
		}
		this.search.handleInput(data);
		const query = this.search.getValue();
		const filtered = query
			? fuzzyFilter(this.items, query, (item) => `${item.label} ${item.description ?? ""}`)
			: this.items;
		this.list = this.buildList(filtered);
		this.children[this.listIndex] = this.list;
	}
}

/** A bordered one-line question; secrets are masked and validation errors show under the input. */
class SetupInput extends Container implements Focusable {
	private readonly input: Input;
	private readonly error: Text;
	private readonly onCancel: () => void;
	private _focused = false;

	get focused(): boolean {
		return this._focused;
	}

	set focused(value: boolean) {
		this._focused = value;
		this.input.focused = value;
	}

	constructor(title: string, options: InputOptions, onDone: (value: string) => void, onCancel: () => void) {
		super();
		this.onCancel = onCancel;
		this.addChild(new DynamicBorder());
		this.addChild(new Text(theme.fg("accent", theme.bold(title)), 1, 0));
		if (options.description) this.addChild(new Text(theme.fg("text", options.description), 1, 0));
		if (options.secret)
			this.addChild(new Text(theme.fg("dim", "Input is hidden. Paste the value and press Enter."), 1, 0));
		this.addChild(new Spacer(1));
		this.input = new Input(options.secret ? { mask: "•" } : {});
		if (options.initial) {
			this.input.setValue(options.initial);
			// Start at the end of the suggestion, so typing appends and Ctrl+U clears it.
			this.input.moveCursorToEnd();
		}
		this.input.onSubmit = (value) => {
			const problem = options.validate?.(value);
			if (problem) {
				this.error.setText(theme.fg("error", problem));
				return;
			}
			onDone(value);
		};
		this.input.onEscape = () => this.onCancel();
		this.addChild(this.input);
		this.error = new Text("", 1, 0);
		this.addChild(this.error);
		this.addChild(
			new Text(
				`${keyHint("tui.select.confirm", "submit")}  ${rawKeyHint("esc", "skip/back")}  ${rawKeyHint("ctrl+c", "quit setup")}`,
				1,
				0,
			),
		);
		this.addChild(new DynamicBorder());
	}

	handleInput(data: string): void {
		if (getKeybindings().matches(data, "tui.select.cancel")) {
			this.onCancel();
			return;
		}
		this.error.setText("");
		this.input.handleInput(data);
	}
}

/** `SetupUi` on a running TUI: notes go into a transcript, the current question sits in a slot below it. */
export class TuiSetupUi implements SetupUi {
	private readonly transcript = new Container();
	private readonly slot = new Container();
	private quit: (() => void) | undefined;
	private readonly removeListener: () => void;
	private readonly ui: TUI;

	constructor(ui: TUI) {
		this.ui = ui;
		ui.addChild(new Text(theme.fg("accent", theme.bold("Ultron setup")), 1, 0));
		ui.addChild(this.transcript);
		ui.addChild(this.slot);
		this.removeListener = ui.addInputListener((data) => {
			if (!matchesKey(data, "ctrl+c")) return undefined;
			this.quit?.();
			return { consume: true };
		});
	}

	dispose(): void {
		this.removeListener();
	}

	heading(title: string): void {
		this.transcript.addChild(new Spacer(1));
		this.transcript.addChild(new Text(theme.fg("accent", theme.bold(title)), 1, 0));
		this.ui.requestRender();
	}

	note(text: string, tone: Tone = "info"): void {
		this.transcript.addChild(new Text(color(tone, text), 2, 0));
		this.ui.requestRender();
	}

	/** Show `component` in the slot until the returned promise settles; Ctrl+C rejects it with `SetupQuit`. */
	private ask<T>(build: (resolve: (value: T) => void) => Component): Promise<T> {
		return new Promise<T>((resolve, reject) => {
			let settled = false;
			const finish = (): void => {
				settled = true;
				this.quit = undefined;
				this.slot.clear();
				this.ui.setFocus(null);
				this.ui.requestRender();
			};
			const component = build((value) => {
				if (settled) return;
				finish();
				resolve(value);
			});
			this.quit = () => {
				if (settled) return;
				finish();
				reject(new SetupQuit());
			};
			this.slot.clear();
			this.slot.addChild(component);
			this.ui.setFocus(component);
			this.ui.requestRender();
		});
	}

	select<T>(title: string, choices: readonly Choice<T>[], options?: { description?: string }): Promise<T | undefined> {
		return this.ask<T | undefined>(
			(done) =>
				new SetupSelect(
					title,
					choices,
					(index) => done(choices[index]?.value),
					() => done(undefined),
					options?.description,
				),
		);
	}

	input(title: string, options: InputOptions = {}): Promise<string | undefined> {
		return this.ask<string | undefined>(
			(done) =>
				new SetupInput(
					title,
					options,
					(value) => done(value),
					() => done(undefined),
				),
		);
	}

	pickLoginProvider(
		_runtime: LoginRuntime,
		providers: readonly AuthSelectorProvider[],
		_title: string, // Pi's picker has its own title.
	): Promise<AuthSelectorProvider | undefined> {
		return this.ask<AuthSelectorProvider | undefined>(
			(done) =>
				new OAuthSelectorComponent(
					"login",
					[...providers],
					(providerId, authType) =>
						done(providers.find((provider) => provider.id === providerId && provider.authType === authType)),
					() => done(undefined),
				),
		);
	}

	async login(runtime: LoginRuntime, option: AuthSelectorProvider) {
		let cancel: (() => void) | undefined;
		this.quit = () => cancel?.();
		try {
			const outcome = await runProviderLogin(
				{
					ui: this.ui,
					requestRender: () => this.ui.requestRender(),
					showComponent: (component, focus, options) => {
						cancel = options?.cancel;
						this.slot.clear();
						this.slot.addChild(component);
						this.ui.setFocus(focus);
						if (isFocusable(focus)) focus.focused = true;
						this.ui.requestRender();
						return () => {
							this.slot.clear();
							this.ui.setFocus(null);
							this.ui.requestRender();
						};
					},
				},
				runtime,
				option,
			);
			return outcome;
		} finally {
			this.quit = undefined;
		}
	}

	async busy<T>(message: string, task: () => Promise<T>): Promise<T> {
		const loader = new Loader(
			this.ui,
			(text) => theme.fg("accent", text),
			(text) => theme.fg("muted", text),
			message,
		);
		this.slot.clear();
		this.slot.addChild(loader);
		this.ui.requestRender();
		try {
			return await new Promise<T>((resolve, reject) => {
				this.quit = () => reject(new SetupQuit());
				task().then(resolve, reject);
			});
		} finally {
			this.quit = undefined;
			loader.stop();
			this.slot.clear();
			this.ui.requestRender();
		}
	}
}
