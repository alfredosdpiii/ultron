import { setKeybindings, type TUI } from "@ultron/tui";
import { beforeAll, beforeEach, describe, expect, test, vi } from "vitest";
import { KeybindingsManager } from "../src/core/keybindings.ts";
import { LoginDialogComponent } from "../src/modes/interactive/components/login-dialog.ts";
import { initTheme } from "../src/modes/interactive/theme/theme.ts";
import { stripAnsi } from "../src/utils/ansi.ts";
import { openBrowser } from "../src/utils/open-browser.ts";
import { type BrowserReach, browserReach, isRunningInContainer } from "../src/utils/remote-browser.ts";

vi.mock("../src/utils/open-browser.ts", () => ({ openBrowser: vi.fn() }));

function createDialog(reach: BrowserReach = "local"): LoginDialogComponent {
	return new LoginDialogComponent(
		{ requestRender: vi.fn() } as unknown as TUI,
		"provider",
		() => {},
		"Provider",
		undefined,
		reach,
	);
}

function rendered(dialog: LoginDialogComponent): string {
	return stripAnsi(dialog.render(120).join("\n"));
}

/** Resolves to the prompt's answer, or "pending" while the dialog is still waiting. */
function settled(prompt: Promise<string>): Promise<string> {
	return Promise.race([prompt, new Promise<string>((resolve) => setTimeout(() => resolve("pending"), 20))]);
}

describe("LoginDialogComponent: the paste-the-code prompt of browser logins", () => {
	beforeAll(() => initTheme("dark"));
	beforeEach(() => setKeybindings(new KeybindingsManager()));

	test("Enter on the empty prompt keeps waiting for the browser instead of submitting an empty code", async () => {
		const dialog = createDialog();
		dialog.showAuth("https://example.invalid/authorize?state=abc", "Complete login in your browser.");
		const manual = dialog.showManualInput("Paste the authorization code / redirect URL here:");

		dialog.handleInput("\r");
		dialog.handleInput("   ");
		dialog.handleInput("\r");
		expect(await settled(manual)).toBe("pending");
		// The prompt is still open, with the sign-in URL above it.
		expect(rendered(dialog)).toContain("https://example.invalid/authorize?state=abc");
		expect(rendered(dialog)).toContain("to cancel");

		dialog.handleInput("\x1b[200~http://localhost:1455/auth/callback?code=the-code&state=abc\x1b[201~");
		dialog.handleInput("\r");
		expect((await manual).trim()).toBe("http://localhost:1455/auth/callback?code=the-code&state=abc");
	});

	test("a pasted redirect URL longer than the terminal is submitted whole", async () => {
		const dialog = createDialog();
		const manual = dialog.showManualInput("Paste the authorization code / redirect URL here:");
		const url = `http://localhost:53692/callback?code=${"c".repeat(600)}&state=${"s".repeat(43)}`;
		dialog.handleInput(`\x1b[200~${url}\n\x1b[201~`);
		dialog.render(40);
		dialog.handleInput("\r");
		expect(await manual).toBe(url);
	});

	test("prompts that allow an empty answer still take one", async () => {
		const dialog = createDialog();
		const prompt = dialog.showPrompt("GitHub Enterprise URL/domain (blank for github.com)", "company.ghe.com");
		dialog.handleInput("\r");
		expect(await settled(prompt)).toBe("");

		// After a paste prompt, a later ordinary prompt accepts an empty answer again.
		const manual = dialog.showManualInput("Paste the code:");
		dialog.handleInput("code");
		dialog.handleInput("\r");
		expect(await manual).toBe("code");
		const again = dialog.showPrompt("Optional value");
		dialog.handleInput("\r");
		expect(await settled(again)).toBe("");
	});

	test("the pasted code is shown as typed, even after a masked API-key prompt", async () => {
		const dialog = createDialog();
		const secret = dialog.showPrompt("API key", undefined, { secret: true });
		dialog.handleInput("sk-hidden");
		expect(rendered(dialog)).not.toContain("sk-hidden");
		dialog.handleInput("\r");
		expect(await secret).toBe("sk-hidden");

		const manual = dialog.showManualInput("Paste the authorization code:");
		dialog.handleInput("visible-code");
		expect(rendered(dialog)).toContain("> visible-code");
		dialog.handleInput("\r");
		expect(await manual).toBe("visible-code");
		expect(rendered(dialog)).not.toContain("sk-hidden");
	});

	test("Esc cancels the paste prompt and aborts the login", async () => {
		const dialog = createDialog();
		const manual = dialog.showManualInput("Paste the code:");
		dialog.handleInput("\x1b");
		await expect(manual).rejects.toThrow("Login cancelled");
		expect(dialog.signal.aborted).toBe(true);
	});
});

/** Rendered text with wrapped lines rejoined, so a sentence can be matched whatever the width. */
function flowed(dialog: LoginDialogComponent, width = 80): string {
	return stripAnsi(dialog.render(width).join("\n"))
		.split("\n")
		.map((line) => line.trim())
		.join(" ")
		.replace(/\s+/g, " ");
}

describe("LoginDialogComponent: when the browser cannot reach the login's localhost callback", () => {
	beforeAll(() => initTheme("dark"));
	beforeEach(() => setKeybindings(new KeybindingsManager()));

	// The report: Ultron in a Docker container on a Mac. The browser finished the OpenAI sign-in and was sent to
	// localhost:1455/auth/callback?code=..., which is the Mac's localhost, not the container's: "localhost refused to
	// connect". The paste prompt was on screen, but nothing said that this failed page's address is what to paste.
	test("every paste prompt says what to do with a callback page that can't be reached", () => {
		const dialog = createDialog("local");
		dialog.showAuth("https://auth.openai.com/oauth/authorize?state=abc", "Sign in, then come back.");
		void dialog.showManualInput(
			"Complete login in your browser, or paste the authorization code / redirect URL here:",
		);
		const text = flowed(dialog);
		expect(text).toContain(
			"If the browser ends on a page that can't be reached (localhost refused to connect), copy that page's full address from the address bar and paste it here.",
		);
		expect(text).toContain("Sign in, then come back.");
		expect(text).not.toContain("Open this address in a browser on your computer");
		expect(openBrowser).toHaveBeenCalledWith("https://auth.openai.com/oauth/authorize?state=abc");
	});

	test.each([
		["container", "Ultron is running in a container"],
		["ssh", "Ultron is running over SSH"],
	] as const)("in a %s the failed page is announced as expected, with the same instruction", (reach, opening) => {
		const dialog = createDialog(reach);
		dialog.showAuth("https://auth.openai.com/oauth/authorize?state=abc", "Sign in, then come back.");
		void dialog.showManualInput(
			"Complete login in your browser, or paste the authorization code / redirect URL here:",
		);
		const text = flowed(dialog);
		expect(text).toContain("Open this address in a browser on your computer.");
		expect(text).toContain(
			`${opening}, so the browser on your computer cannot reach this login's callback: after you sign in it will end on a page that can't be reached (localhost refused to connect). That is expected; copy that page's full address from the address bar and paste it here.`,
		);
		// The provider's own "complete login in your browser" line is dropped: with a real sign-in URL (five or six
		// lines at 80 columns) the dialog must still show its input on a 24-row terminal.
		expect(text).not.toContain("Sign in, then come back.");
		expect(dialog.render(80).length).toBeLessThanOrEqual(16);
	});

	test("device-code logins have no callback, so they carry no such instruction", () => {
		const dialog = createDialog("container");
		dialog.showDeviceCode({ userCode: "ABCD-EFGH", verificationUri: "https://github.com/login/device" });
		dialog.showWaiting("Waiting for authentication...");
		expect(flowed(dialog)).not.toContain("can't be reached");
	});
});

describe("where the browser is relative to this process", () => {
	const nothing = { env: {}, exists: () => false, read: () => "0::/\n" };

	test("a container is recognised by the marker its runtime leaves", () => {
		expect(isRunningInContainer(nothing)).toBe(false);
		expect(isRunningInContainer({ ...nothing, exists: (path) => path === "/.dockerenv" })).toBe(true);
		expect(isRunningInContainer({ ...nothing, exists: (path) => path === "/run/.containerenv" })).toBe(true);
		expect(isRunningInContainer({ ...nothing, env: { container: "podman" } })).toBe(true);
		expect(isRunningInContainer({ ...nothing, env: { KUBERNETES_SERVICE_HOST: "10.0.0.1" } })).toBe(true);
		expect(isRunningInContainer({ ...nothing, read: () => "12:devices:/docker/0123abcd\n" })).toBe(true);
		expect(
			isRunningInContainer({
				...nothing,
				read: () => {
					throw new Error("ENOENT"); // macOS and Windows have no /proc
				},
			}),
		).toBe(false);
	});

	test("a container wins over SSH; SSH is recognised by its session variables", () => {
		expect(browserReach(nothing)).toBe("local");
		expect(browserReach({ ...nothing, env: { SSH_CONNECTION: "10.0.0.2 50000 10.0.0.3 22" } })).toBe("ssh");
		expect(browserReach({ ...nothing, env: { SSH_TTY: "/dev/pts/3" } })).toBe("ssh");
		expect(
			browserReach({ ...nothing, env: { SSH_TTY: "/dev/pts/3" }, exists: (path) => path === "/.dockerenv" }),
		).toBe("container");
	});

	test("a callback server put on another interface on purpose is expected to be reached", () => {
		const inDocker = { ...nothing, exists: (path: string) => path === "/.dockerenv" };
		expect(browserReach({ ...inDocker, env: { ULTRON_OAUTH_CALLBACK_HOST: "0.0.0.0" } })).toBe("local");
		expect(browserReach({ ...inDocker, env: { PI_OAUTH_CALLBACK_HOST: "0.0.0.0" } })).toBe("local");
	});
});
