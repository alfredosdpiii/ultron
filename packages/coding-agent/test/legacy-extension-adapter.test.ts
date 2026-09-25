import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { BACKGROUND_CONTEXT } from "@earendil-works/chord/context";
import { NodeExecutionEnv } from "@earendil-works/pi-agent-core/node";
import { describe, expect, test } from "vitest";
import { DefaultResourceLoader } from "../src/core/resource-loader.ts";
import { SettingsManager } from "../src/core/settings-manager.ts";
import { LegacyExtensionAdapter } from "../src/experimental/legacy-extension-adapter.ts";

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
});
