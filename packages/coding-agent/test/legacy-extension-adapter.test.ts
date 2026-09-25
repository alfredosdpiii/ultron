import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { NodeExecutionEnv } from "@ultron/agent-core/node";
import { BACKGROUND_CONTEXT } from "@ultron/chord/context";
import { describe, expect, test } from "vitest";
import { DefaultResourceLoader } from "../src/core/resource-loader.ts";
import { SettingsManager } from "../src/core/settings-manager.ts";
import { LegacyExtensionAdapter } from "../src/experimental/legacy-extension-adapter.ts";
import { ExtensionUIBridge } from "../src/experimental/services/extension-ui-provider.ts";

describe("LegacyExtensionAdapter", () => {
	test("loads a Pi extension and exposes its tool and command registrations", async () => {
		const root = await mkdtemp(join(tmpdir(), "ultron-extension-adapter-"));
		const extension = join(root, "extension.ts");
		await writeFile(
			extension,
			`export default function (pi) {
				pi.registerCommand("hello", { description: "Say hello", handler: async (_args, ctx) => ctx.ui.notify("hello", "info") });
				pi.registerTool({ name: "test_tool", label: "Test", description: "Test tool", parameters: {}, async execute() { return { content: [{ type: "text", text: "ok" }] }; } });
			}`,
		);
		const loader = new DefaultResourceLoader({
			cwd: root,
			agentDir: root,
			settingsManager: SettingsManager.create(root, root),
			additionalExtensionPaths: [extension],
		});
		await loader.reload();
		const env = new NodeExecutionEnv({ cwd: root });
		const adapter = new LegacyExtensionAdapter({
			session: {} as never,
			lane: {} as never,
			harness: { hooks: { on: () => () => {} }, events: { on: () => () => {} } } as never,
			modelRuntime: {} as never,
			resourceLoader: loader,
			cwd: root,
			model: {} as never,
			systemPrompt: "test",
		});
		try {
			adapter.bind();
			expect(adapter.commands).toContainEqual(expect.objectContaining({ name: "hello", description: "Say hello" }));
			expect(adapter.tools.map((tool) => tool.name)).toContain("test_tool");
			await expect(adapter.runCommand("hello", "")).resolves.toEqual({ notifications: [] });
		} finally {
			await adapter.close();
			await env.cleanup(BACKGROUND_CONTEXT);
			await rm(root, { recursive: true, force: true });
		}
	});

	test("ctx.hasUI is true only while an interactive client serves the extension UI, as in Pi's modes", async () => {
		const root = await mkdtemp(join(tmpdir(), "ultron-extension-hasui-"));
		const extension = join(root, "extension.ts");
		await writeFile(
			extension,
			`export default function (pi) {
				pi.registerCommand("probe", { description: "Record hasUI", handler: async (_args, ctx) => { globalThis.__ultronHasUI.push(ctx.hasUI); } });
			}`,
		);
		const seen: boolean[] = [];
		(globalThis as { __ultronHasUI?: boolean[] }).__ultronHasUI = seen;
		const loader = new DefaultResourceLoader({
			cwd: root,
			agentDir: root,
			settingsManager: SettingsManager.create(root, root),
			additionalExtensionPaths: [extension],
		});
		await loader.reload();
		const ui = new ExtensionUIBridge();
		const adapter = new LegacyExtensionAdapter({
			session: {} as never,
			lane: {} as never,
			harness: { hooks: { on: () => () => {} }, events: { on: () => () => {} } } as never,
			modelRuntime: {} as never,
			resourceLoader: loader,
			cwd: root,
			model: {} as never,
			systemPrompt: "test",
			ui,
		});
		try {
			adapter.bind();
			// A print or JSON run never polls: no UI.
			await adapter.runCommand("probe", "");
			// The TUI and RPC clients poll the extension UI: UI.
			await ui.service.poll(null, 0, BACKGROUND_CONTEXT);
			await adapter.runCommand("probe", "");
			expect(seen).toEqual([false, true]);
		} finally {
			ui.close();
			await adapter.close();
			delete (globalThis as { __ultronHasUI?: boolean[] }).__ultronHasUI;
			await rm(root, { recursive: true, force: true });
		}
	});
});
