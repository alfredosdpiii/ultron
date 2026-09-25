import { BACKGROUND_CONTEXT } from "@ultron/chord/context";
import { describe, expect, test } from "vitest";
import type { ExtensionUIContext } from "../src/core/extensions/types.ts";
import { ExtensionUIBridge } from "../src/experimental/services/extension-ui-provider.ts";

const fallback = {} as ExtensionUIContext;

describe("ExtensionUIBridge", () => {
	test("with nobody serving, dialogs resolve at once with Pi's defaults and notifications are dropped", async () => {
		const bridge = new ExtensionUIBridge();
		const ui = bridge.createContext(fallback);
		expect(bridge.serving).toBe(false);
		await expect(ui.select("t", ["a"])).resolves.toBeUndefined();
		await expect(ui.confirm("t", "m")).resolves.toBe(false);
		await expect(ui.input("t")).resolves.toBeUndefined();
		await expect(ui.editor("t")).resolves.toBeUndefined();
		ui.notify("dropped");
		const polled = await bridge.service.poll(null, 0, BACKGROUND_CONTEXT);
		expect(polled.requests).toEqual([]);
	});

	test("a serving client receives Pi-format requests in order and its answers resolve the dialogs", async () => {
		const bridge = new ExtensionUIBridge();
		const ui = bridge.createContext(fallback);
		let { cursor } = await bridge.service.poll(null, 0, BACKGROUND_CONTEXT);
		const waiting = bridge.service.poll(cursor, 5_000, BACKGROUND_CONTEXT);
		ui.notify("hello", "warning");
		const picked = ui.select("Pick", ["a", "b"], { timeout: 5_000 });
		const first = await waiting;
		expect(first.requests.map((item) => item.request.method)).toEqual(["notify", "select"]);
		expect(first.requests[0]!.request).toMatchObject({ type: "extension_ui_request", message: "hello" });
		const select = first.requests[1]!.request;
		expect(select).toMatchObject({ method: "select", title: "Pick", options: ["a", "b"], timeout: 5_000 });
		// Delivered requests are not repeated; the open dialog is still offered to a client that starts serving now.
		expect((await bridge.service.poll(first.cursor, 0, BACKGROUND_CONTEXT)).requests).toEqual([]);
		expect((await bridge.service.poll(null, 0, BACKGROUND_CONTEXT)).requests.map((item) => item.request.id)).toEqual([
			select.id,
		]);
		cursor = first.cursor;
		await bridge.service.respond(select.id, { value: "b" }, BACKGROUND_CONTEXT);
		await expect(picked).resolves.toBe("b");

		const confirmed = ui.confirm("Sure?", "really");
		const third = await bridge.service.poll(cursor, 1_000, BACKGROUND_CONTEXT);
		await bridge.service.respond(third.requests[0]!.request.id, { cancelled: true }, BACKGROUND_CONTEXT);
		await expect(confirmed).resolves.toBe(false);
	});

	test("dialogs honour the extension's timeout and signal, and settle with defaults once the client leaves", async () => {
		const bridge = new ExtensionUIBridge();
		const ui = bridge.createContext(fallback);
		await bridge.service.poll(null, 0, BACKGROUND_CONTEXT);
		await expect(ui.select("t", ["a"], { timeout: 50 })).resolves.toBeUndefined();
		const abort = new AbortController();
		const aborted = ui.input("t", undefined, { signal: abort.signal });
		abort.abort();
		await expect(aborted).resolves.toBeUndefined();
		// Nobody polls again: after the serving grace the open dialog gets its default instead of hanging.
		const abandoned = ui.confirm("t", "m");
		await expect(abandoned).resolves.toBe(false);
		expect(bridge.serving).toBe(false);
	});
});
