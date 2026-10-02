import { chmodSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, describe, expect, it, vi } from "vitest";
import { createProfileRuntime, hasUsableModel, noModelHint } from "../src/cli/setup/command.ts";
import { findInitialModel } from "../src/core/model-resolver.ts";
import { SettingsManager } from "../src/core/settings-manager.ts";

const dirs: string[] = [];
afterEach(() => {
	for (const dir of dirs.splice(0)) rmSync(dir, { recursive: true, force: true });
});

function profile(settingsJson?: Record<string, unknown>): {
	root: string;
	agentDir: string;
	settings: SettingsManager;
} {
	const root = mkdtempSync(join(tmpdir(), "ultron-start-model-"));
	dirs.push(root);
	const agentDir = join(root, "agent");
	mkdirSync(agentDir, { recursive: true });
	if (settingsJson) writeFileSync(join(agentDir, "settings.json"), JSON.stringify(settingsJson));
	return { root, agentDir, settings: SettingsManager.create(root, agentDir, { projectTrusted: false }) };
}

/** A `claude` executable on this machine, as on any machine with Claude Code installed. */
function installFakeClaude(root: string): void {
	const bin = join(root, "claude");
	writeFileSync(bin, "#!/bin/sh\nexit 1\n");
	chmodSync(bin, 0o755);
	vi.stubEnv("ULTRON_CLAUDE_CODE_BIN", bin);
}

/** What the session worker resolves at start (session-worker.ts), over the same profile. */
async function workerStartModel(agentDir: string, settings: SettingsManager): Promise<string | undefined> {
	const modelRuntime = await createProfileRuntime(agentDir);
	const resolved = await findInitialModel({
		scopedModels: [],
		isContinuing: true,
		defaultProvider: settings.getDefaultProvider(),
		defaultModelId: settings.getDefaultModel(),
		defaultThinkingLevel: settings.getDefaultThinkingLevel(),
		modelRuntime,
	});
	return resolved.model ? `${resolved.model.provider}/${resolved.model.id}` : undefined;
}

describe("whether a start has a model to start on", () => {
	it("an installed Claude Code CLI alone is not one: setup is offered instead of a start that cannot work", async () => {
		const { root, agentDir, settings } = profile();
		installFakeClaude(root);
		// The CLI makes the claude-code models "available" ...
		const available = (await createProfileRuntime(agentDir)).getAvailableSnapshot();
		expect(available.length).toBeGreaterThan(0);
		expect(available.every((model) => model.provider === "claude-code")).toBe(true);
		// ... but the worker never starts on them, so the start used to die with "Internal server error" and no hint.
		expect(await workerStartModel(agentDir, settings)).toBeUndefined();
		expect(await hasUsableModel(agentDir, settings, {})).toBe(false);
		expect(await noModelHint(agentDir, {})).toContain('Run "ultron setup"');
	});

	it("under ultron --claude the Claude Code models do count", async () => {
		const { root, agentDir, settings } = profile();
		installFakeClaude(root);
		expect(await hasUsableModel(agentDir, settings, { ULTRON_ROOT: "claude" })).toBe(true);
		expect(await noModelHint(agentDir, { ULTRON_ROOT: "claude" })).toBeUndefined();
	});

	it("nothing configured: setup is offered and a failed start says how to get a model", async () => {
		const { agentDir, settings } = profile();
		expect(await hasUsableModel(agentDir, settings, {})).toBe(false);
		expect(await noModelHint(agentDir, {})).toBe(
			'No model is configured. Run "ultron setup", or set a provider key such as ANTHROPIC_API_KEY or OPENAI_API_KEY.',
		);
	});

	it("stored credentials make their provider's models usable, as the worker sees them", async () => {
		const { agentDir, settings } = profile();
		writeFileSync(
			join(agentDir, "auth.json"),
			JSON.stringify({ anthropic: { type: "oauth", access: "a", refresh: "r", expires: Date.now() + 3_600_000 } }),
		);
		expect(await workerStartModel(agentDir, settings)).toMatch(/^anthropic\//);
		expect(await hasUsableModel(agentDir, settings, {})).toBe(true);
		expect(await noModelHint(agentDir, {})).toBeUndefined();
	});

	it("a saved default whose credentials are gone is named in the hint of the failed start", async () => {
		const modelId = (await createProfileRuntime(profile().agentDir))
			.getModels()
			.find((model) => model.provider === "anthropic")!.id;
		const { agentDir, settings } = profile({ defaultProvider: "anthropic", defaultModel: modelId });
		// A saved default is taken on trust before the start (the catalog is not loaded on a normal start) ...
		expect(await hasUsableModel(agentDir, settings, {})).toBe(true);
		// ... and the worker then finds nothing to start on.
		expect(await workerStartModel(agentDir, settings)).toBeUndefined();
		const hint = await noModelHint(agentDir, {});
		expect(hint).toContain(`The default model anthropic/${modelId} has no credentials`);
		expect(hint).toContain('Run "ultron setup"');
	});

	it("a default from a provider this profile does not know (an extension's) gets no misleading hint", async () => {
		const { agentDir } = profile({ defaultProvider: "my-extension-provider", defaultModel: "m1" });
		expect(await noModelHint(agentDir, {})).toBeUndefined();
	});
});
