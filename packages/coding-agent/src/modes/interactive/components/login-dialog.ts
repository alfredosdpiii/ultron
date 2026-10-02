import type { AuthInfoLink, OAuthDeviceCodeInfo } from "@ultron/ai";
import { Container, type Focusable, getKeybindings, Input, Spacer, Text, type TUI } from "@ultron/tui";
import { openBrowser } from "../../../utils/open-browser.ts";
import { type BrowserReach, browserReach } from "../../../utils/remote-browser.ts";
import { theme } from "../theme/theme.ts";
import { DynamicBorder } from "./dynamic-border.ts";
import { keyHint } from "./keybinding-hints.ts";

const UNREACHABLE_CALLBACK_STEP = "copy that page's full address from the address bar and paste it here.";

/**
 * What to do when the browser cannot load the login's `localhost` callback page; shown with every paste prompt.
 * In a container or over SSH the browser is known to be out of reach of this process's `localhost`, so the failed
 * page is announced as the expected outcome rather than as a possibility.
 */
export function unreachableCallbackHint(reach: BrowserReach): string {
	if (reach === "local")
		return `If the browser ends on a page that can't be reached (localhost refused to connect), ${UNREACHABLE_CALLBACK_STEP}`;
	const where = reach === "container" ? "in a container" : "over SSH";
	return `Ultron is running ${where}, so the browser on your computer cannot reach this login's callback: after you sign in it will end on a page that can't be reached (localhost refused to connect). That is expected; ${UNREACHABLE_CALLBACK_STEP}`;
}

/**
 * Login dialog component - replaces editor during OAuth login flow
 */
export class LoginDialogComponent extends Container implements Focusable {
	private contentContainer: Container;
	private input: Input;
	private secretInput = false;
	/** The active prompt has no meaningful empty answer (paste-the-code), so Enter on an empty input is ignored. */
	private requireValue = false;
	private tui: TUI;
	private readonly reach: BrowserReach;
	private abortController = new AbortController();
	private inputResolver?: (value: string) => void;
	private inputRejecter?: (error: Error) => void;
	private onComplete: (success: boolean, message?: string) => void;

	// Focusable implementation - propagate to input for IME cursor positioning
	private _focused = false;
	get focused(): boolean {
		return this._focused;
	}
	set focused(value: boolean) {
		this._focused = value;
		this.input.focused = value;
	}

	constructor(
		tui: TUI,
		providerId: string,
		onComplete: (success: boolean, message?: string) => void,
		providerNameOverride?: string,
		titleOverride?: string,
		reach: BrowserReach = browserReach(),
	) {
		super();
		this.tui = tui;
		this.reach = reach;
		this.onComplete = onComplete;

		const providerName = providerNameOverride || providerId;
		const title = titleOverride ?? `Login to ${providerName}`;

		// Top border
		this.addChild(new DynamicBorder());

		// Title
		this.addChild(new Text(theme.fg("accent", theme.bold(title)), 1, 0));

		// Dynamic content area
		this.contentContainer = new Container();
		this.addChild(this.contentContainer);

		// Input (always present, used when needed)
		this.input = this.createInput(false);

		// Bottom border
		this.addChild(new DynamicBorder());
	}

	/** A plain input, or a masked one for secrets (API keys) that never shows the value, even once submitted. */
	private createInput(secret: boolean): Input {
		const input = new Input(secret ? { mask: "•" } : {});
		input.onSubmit = () => {
			if (this.inputResolver) {
				const value = input.getValue();
				// A stray Enter while the browser sign-in is still under way must not end the login: the provider
				// would take the empty answer as the pasted code and fail with "Missing authorization code".
				if (this.requireValue && value.trim() === "") return;
				this.replaceInputWithSubmittedText(secret ? "•".repeat(Math.min(value.length, 12)) : value);
				this.inputResolver(value);
				this.inputResolver = undefined;
				this.inputRejecter = undefined;
			}
		};
		input.onEscape = () => {
			this.cancel();
		};
		this.secretInput = secret;
		return input;
	}

	get signal(): AbortSignal {
		return this.abortController.signal;
	}

	private replaceInputWithSubmittedText(value: string): void {
		this.contentContainer.children = this.contentContainer.children.map((child) =>
			child === this.input ? new Text(`> ${value}`, 0, 0) : child,
		);
	}

	private cancel(): void {
		this.abortController.abort();
		if (this.inputRejecter) {
			this.inputRejecter(new Error("Login cancelled"));
			this.inputResolver = undefined;
			this.inputRejecter = undefined;
		}
		this.onComplete(false, "Login cancelled");
	}

	/**
	 * Called by onAuth callback - show URL and optional instructions
	 */
	showAuth(url: string, instructions?: string): void {
		this.contentContainer.clear();
		this.contentContainer.addChild(new Spacer(1));
		const linkedUrl = `\x1b]8;;${url}\x07${url}\x1b]8;;\x07`;
		this.contentContainer.addChild(new Text(theme.fg("accent", linkedUrl), 1, 0));

		const clickHint = process.platform === "darwin" ? "Cmd+click to open" : "Ctrl+click to open";
		const hyperlink = `\x1b]8;;${url}\x07${clickHint}\x1b]8;;\x07`;
		this.contentContainer.addChild(new Text(theme.fg("dim", hyperlink), 1, 0));
		if (this.reach !== "local") {
			// No browser can be launched from here; "a browser window should open" would leave the user waiting.
			this.contentContainer.addChild(
				new Text(theme.fg("warning", "Open this address in a browser on your computer."), 1, 0),
			);
		}

		// Away from the browser the paste prompt carries the full instruction; the provider's own line would only
		// repeat it and push the input off a small terminal.
		if (instructions && this.reach === "local") {
			this.contentContainer.addChild(new Spacer(1));
			this.contentContainer.addChild(new Text(theme.fg("warning", instructions), 1, 0));
		}

		openBrowser(url);
		this.tui.requestRender();
	}

	/**
	 * Called by onDeviceCode callback - show URL and user code.
	 */
	showDeviceCode(info: OAuthDeviceCodeInfo): void {
		this.contentContainer.clear();
		this.contentContainer.addChild(new Spacer(1));
		const linkedUrl = `\x1b]8;;${info.verificationUri}\x07${info.verificationUri}\x1b]8;;\x07`;
		this.contentContainer.addChild(new Text(theme.fg("accent", linkedUrl), 1, 0));

		const clickHint = process.platform === "darwin" ? "Cmd+click to open" : "Ctrl+click to open";
		const hyperlink = `\x1b]8;;${info.verificationUri}\x07${clickHint}\x1b]8;;\x07`;
		this.contentContainer.addChild(new Text(theme.fg("dim", hyperlink), 1, 0));
		this.contentContainer.addChild(new Spacer(1));
		this.contentContainer.addChild(new Text(theme.fg("warning", `Enter code: ${info.userCode}`), 1, 0));

		this.tui.requestRender();
	}

	/**
	 * Show input for manual code/URL entry (for callback server providers)
	 */
	showManualInput(prompt: string): Promise<string> {
		// A pasted code or redirect URL is not a secret to hide, whatever an earlier prompt asked for.
		if (this.secretInput) {
			this.input = this.createInput(false);
			this.input.focused = this._focused;
		}
		this.requireValue = true;
		this.input.setValue("");
		// The paste prompt belongs to logins that redirect the browser to a callback server on localhost. When the
		// browser cannot reach it (a container, SSH, a blocked port) the redirect fails in the browser; say what to
		// do then, since nothing in the browser does.
		this.contentContainer.addChild(new Spacer(1));
		this.contentContainer.addChild(new Text(theme.fg("dim", prompt), 1, 0));
		this.contentContainer.addChild(
			new Text(theme.fg(this.reach === "local" ? "dim" : "warning", unreachableCallbackHint(this.reach)), 1, 0),
		);
		this.contentContainer.addChild(this.input);
		this.contentContainer.addChild(new Text(`(${keyHint("tui.select.cancel", "to cancel")})`, 1, 0));
		this.tui.requestRender();

		return new Promise((resolve, reject) => {
			this.inputResolver = resolve;
			this.inputRejecter = reject;
		});
	}

	/**
	 * Called by onPrompt callback - show prompt and wait for input
	 * Note: Does NOT clear content, appends to existing (preserves URL from showAuth)
	 */
	showPrompt(message: string, placeholder?: string, options: { secret?: boolean } = {}): Promise<string> {
		const secret = options.secret === true;
		if (secret !== this.secretInput) {
			this.input = this.createInput(secret);
			this.input.focused = this._focused;
		}
		// Some prompts take an empty answer (GitHub Copilot: "blank for github.com").
		this.requireValue = false;
		this.contentContainer.addChild(new Spacer(1));
		this.contentContainer.addChild(new Text(theme.fg("text", message), 1, 0));
		if (placeholder) {
			this.contentContainer.addChild(new Text(theme.fg("dim", `e.g., ${placeholder}`), 1, 0));
		}
		this.contentContainer.addChild(this.input);
		this.contentContainer.addChild(
			new Text(
				`(${keyHint("tui.select.cancel", "to cancel,")} ${keyHint("tui.select.confirm", "to submit")})`,
				1,
				0,
			),
		);

		this.input.setValue("");
		this.tui.requestRender();

		return new Promise((resolve, reject) => {
			this.inputResolver = resolve;
			this.inputRejecter = reject;
		});
	}

	/** Show informational text before another login step. */
	showDetails(lines: string[]): void {
		this.contentContainer.clear();
		this.contentContainer.addChild(new Spacer(1));
		for (const line of lines) {
			this.contentContainer.addChild(new Text(line, 1, 0));
		}
		this.tui.requestRender();
	}

	/** Show provider-owned information and links without starting an auth callback flow. */
	showInfo(message: string, links: readonly AuthInfoLink[] = [], showCloseHint = false): void {
		this.contentContainer.addChild(new Spacer(1));
		this.contentContainer.addChild(new Text(theme.fg("text", message), 1, 0));
		for (const link of links) {
			const text = link.label ? `${link.label}: ${link.url}` : link.url;
			const hyperlink = `\x1b]8;;${link.url}\x07${text}\x1b]8;;\x07`;
			this.contentContainer.addChild(new Text(theme.fg("accent", hyperlink), 1, 0));
		}
		if (showCloseHint) {
			this.contentContainer.addChild(new Spacer(1));
			this.contentContainer.addChild(new Text(`(${keyHint("tui.select.cancel", "to close")})`, 1, 0));
		}
		this.tui.requestRender();
	}

	/**
	 * Show waiting message (for polling flows like GitHub Copilot)
	 */
	showWaiting(message: string): void {
		this.contentContainer.addChild(new Spacer(1));
		this.contentContainer.addChild(new Text(theme.fg("dim", message), 1, 0));
		this.contentContainer.addChild(new Text(`(${keyHint("tui.select.cancel", "to cancel")})`, 1, 0));
		this.tui.requestRender();
	}

	/**
	 * Called by onProgress callback
	 */
	showProgress(message: string): void {
		this.contentContainer.addChild(new Text(theme.fg("dim", message), 1, 0));
		this.tui.requestRender();
	}

	handleInput(data: string): void {
		const kb = getKeybindings();

		if (kb.matches(data, "tui.select.cancel")) {
			this.cancel();
			return;
		}

		// Pass to input
		this.input.handleInput(data);
	}
}
