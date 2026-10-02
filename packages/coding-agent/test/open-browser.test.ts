import { EventEmitter } from "node:events";
import { afterEach, describe, expect, test, vi } from "vitest";

const spawn = vi.hoisted(() => vi.fn());
vi.mock("node:child_process", () => ({ spawn }));

import { openBrowser } from "../src/utils/open-browser.ts";

function fakeChild(): EventEmitter & { unref: () => void } {
	return Object.assign(new EventEmitter(), { unref: vi.fn() });
}

describe("openBrowser", () => {
	afterEach(() => {
		spawn.mockReset();
		vi.unstubAllGlobals();
	});

	test("starts the platform's opener with the URL as one argument, never through a shell", () => {
		for (const [platform, command, args] of [
			["darwin", "open", ["https://example.invalid/a?b=1&c=2"]],
			["linux", "xdg-open", ["https://example.invalid/a?b=1&c=2"]],
			["win32", "rundll32", ["url.dll,FileProtocolHandler", "https://example.invalid/a?b=1&c=2"]],
		] as const) {
			spawn.mockReset();
			spawn.mockReturnValue(fakeChild());
			const original = Object.getOwnPropertyDescriptor(process, "platform")!;
			Object.defineProperty(process, "platform", { value: platform });
			try {
				openBrowser("https://example.invalid/a?b=1&c=2");
			} finally {
				Object.defineProperty(process, "platform", original);
			}
			expect(spawn).toHaveBeenCalledWith(command, args, { stdio: "ignore", detached: true });
			expect(spawn.mock.calls[0]?.[2]).not.toHaveProperty("shell");
		}
	});

	test("a missing or failing opener never fails the caller: the login keeps the URL on screen", () => {
		// The opener is not installed: spawn reports it on the child.
		const child = fakeChild();
		spawn.mockReturnValue(child);
		expect(() => openBrowser("https://example.invalid/")).not.toThrow();
		expect(() =>
			child.emit("error", Object.assign(new Error("spawn open ENOENT"), { code: "ENOENT" })),
		).not.toThrow();

		// spawn itself throws (no process slots, an argument the platform rejects).
		spawn.mockImplementation(() => {
			throw Object.assign(new Error("spawn EAGAIN"), { code: "EAGAIN" });
		});
		expect(() => openBrowser("https://example.invalid/")).not.toThrow();
	});
});
