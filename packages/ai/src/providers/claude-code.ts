import { claudeCodeCliApi } from "../api/claude-code-cli.lazy.ts";
import type { ApiKeyAuth, AuthContext } from "../auth/types.ts";
import { createProvider, type Provider } from "../models.ts";
import type { Model, ThinkingLevelMap } from "../types.ts";

/**
 * Claude Code provider: model calls run through the user's installed, logged-in `claude` CLI in headless
 * mode (`claude -p --output-format stream-json`). The CLI does its own authentication (a claude.ai
 * subscription login, or whatever the user configured); Ultron never reads, copies or mints Claude
 * credentials. The provider has no tool calling: it serves tool-free lanes (inference frames, review
 * frames, judges). A tool-using root lane belongs to `ultron claude`.
 */
export const CLAUDE_CODE_PROVIDER_ID = "claude-code";
export const CLAUDE_CODE_API = "claude-code-cli";
/** Environment variable naming the `claude` executable; otherwise it is looked up on PATH. */
export const CLAUDE_CODE_BIN_ENV = "ULTRON_CLAUDE_CODE_BIN";

/** Pi thinking levels as `claude --effort` levels; "off" disables extended thinking for the call. */
const EFFORT_LEVELS: ThinkingLevelMap = {
	minimal: "low",
	low: "low",
	medium: "medium",
	high: "high",
	xhigh: "xhigh",
	max: "max",
};

function aliasModel(id: string, name: string): Model<typeof CLAUDE_CODE_API> {
	return {
		id,
		name,
		api: CLAUDE_CODE_API,
		provider: CLAUDE_CODE_PROVIDER_ID,
		baseUrl: "claude-code://cli",
		reasoning: true,
		thinkingLevelMap: EFFORT_LEVELS,
		input: ["text", "image"],
		// Subscription usage has no per-token price here; each response carries the CLI's own reported cost.
		cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
		contextWindow: 200_000,
		maxTokens: 32_000,
	};
}

/** The model aliases the CLI resolves to the latest model of each family. */
export const CLAUDE_CODE_MODELS: readonly Model<typeof CLAUDE_CODE_API>[] = [
	aliasModel("opus", "Claude Opus (Claude Code CLI)"),
	aliasModel("sonnet", "Claude Sonnet (Claude Code CLI)"),
	aliasModel("haiku", "Claude Haiku (Claude Code CLI)"),
];

/** Whether a model is served by the Claude Code CLI provider (no native tool calling). */
export function isClaudeCodeModel(model: { api?: string } | undefined): boolean {
	return model?.api === CLAUDE_CODE_API;
}

function pathEntries(pathValue: string): string[] {
	const separator = pathValue.includes(";") && !pathValue.includes(":/") ? ";" : ":";
	return pathValue.split(separator).filter((entry) => entry.length > 0);
}

/** Locate the CLI without running it: the configured path, or `claude` on PATH. */
async function findCli(ctx: AuthContext, signal: AbortSignal): Promise<string | undefined> {
	const configured = (await ctx.env(CLAUDE_CODE_BIN_ENV))?.trim();
	signal.throwIfAborted();
	if (configured) return (await ctx.fileExists(configured)) ? configured : undefined;
	const pathValue = (await ctx.env("PATH")) ?? "";
	for (const dir of pathEntries(pathValue)) {
		for (const name of ["claude", "claude.exe", "claude.cmd"]) {
			const candidate = `${dir.replace(/[\\/]+$/, "")}/${name}`;
			if (await ctx.fileExists(candidate)) return candidate;
			signal.throwIfAborted();
		}
	}
	return undefined;
}

/**
 * Ambient, key-less auth. The provider counts as configured (its models are listed) when a `claude` executable
 * is present. Requests always resolve, so an explicit `claude-code/...` choice reaches the stream
 * implementation, which explains a missing CLI and checks the login (`claude auth status --json`).
 */
function claudeCodeCliAuth(): ApiKeyAuth {
	const source = "Claude Code CLI (its own login)";
	return {
		name: "Claude Code CLI",
		check: async ({ ctx, signal }) => ((await findCli(ctx, signal)) ? { source, type: "api_key" } : undefined),
		resolve: async ({ signal }) => {
			signal.throwIfAborted();
			return { auth: {}, source };
		},
	};
}

export function claudeCodeProvider(): Provider<typeof CLAUDE_CODE_API> {
	return createProvider({
		id: CLAUDE_CODE_PROVIDER_ID,
		name: "Claude Code (CLI)",
		baseUrl: "claude-code://cli",
		auth: { apiKey: claudeCodeCliAuth() },
		models: CLAUDE_CODE_MODELS,
		api: claudeCodeCliApi(),
	});
}
