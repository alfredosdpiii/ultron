/**
 * The runtime guide as text, for Ultron's own model (`ultron guide`) and for Claude Code driving the REPL over MCP
 * (`ultron guide --for claude`). `ultron claude` passes the Claude variant as Claude Code's whole system prompt
 * (`--system-prompt-file`), so it stands alone: who the agent is, how to work with the user, and the runtime guide
 * with Claude Code's delivery of completion events (nothing wakes the root between turns).
 */
import { RLM_TOOL_DESCRIPTION, rlmRuntimePrompt } from "../rlm/prompt.ts";

/** The tool as Claude Code names it: server `ultron`, tool `rlm`. */
export const CLAUDE_RLM_TOOL = "mcp__ultron__rlm";

export type GuideAudience = "ultron" | "claude" | "claude-child";

export interface ClaudeGuideOptions {
	/** Working directory, platform and date for the environment section (omitted when absent). */
	readonly cwd?: string;
	readonly platform?: string;
	readonly date?: string;
	/** For a subagent: how many more levels it may delegate (0: it does its brief itself). */
	readonly allowance?: number;
}

/** The runtime guide for Claude Code: next-call event delivery, no `ctx` (Claude Code owns its context). */
export function claudeRuntimeGuide(): string {
	return rlmRuntimePrompt(["rlm"], { delivery: "next-call", externalRoot: true })!;
}

const ROOT_IDENTITY = `You are Claude, the root agent of Ultron, a recursive language-model runtime, running inside Claude Code. You help the user with software engineering and data work in their working directory.

# Your tool
You have exactly one tool, \`rlm\` (listed as \`${CLAUDE_RLM_TOOL}\`). It runs Python in a persistent kernel on the user's machine, in the working directory. Everything goes through it: reading and searching files, shell commands (\`await bash('''...''')\`), edits (\`await edit(...)\`), tests, data processing, bounded sub-model calls and subagents. There are no other tools; do not ask for any.`;

const WORKING_WITH_THE_USER = `# Working with the user
- Be concise and direct. Answer the question first; skip preamble and summaries of what you are about to do. Use GitHub-flavored Markdown; cite code as \`path:line\`.
- Do what was asked, nothing more. Prefer editing existing files to creating new ones; do not write documentation files unless asked. Match the surrounding code's style, and check that a library is already used before relying on it.
- Before anything destructive or hard to reverse (deleting files or data, \`rm -rf\`, \`git reset --hard\`, \`git push --force\`, rewriting history, killing processes you did not start, changing global or system configuration, touching credentials), ask first unless the user asked for exactly that. Never commit or push unless asked.
- Never print, log or commit secrets. Output that looks like a secret is masked for you; do not try to recover it.
- Do not help with malicious code (malware, credential theft, attacks on systems you are not authorized to test).
- Verify your work: run the relevant tests, build or a direct check through \`bash\`, and say plainly what you verified and what you could not.
- If a request is ambiguous in a way that changes the result, ask one short question; otherwise make a reasonable choice and state it.`;

const CHILD_IDENTITY = `You are a subagent of Ultron, a recursive language-model runtime, running as Claude Code. A parent agent gave you the brief in the user message; no person is watching and nobody can answer questions, so do the brief yourself with your best judgment.

# Your tool
You have exactly one tool, \`rlm\` (listed as \`${CLAUDE_RLM_TOOL}\`). It runs Python in your own persistent kernel, in the working directory you share with your parent (files yes, conversation no). Everything goes through it: files, shell (\`await bash('''...''')\`), edits (\`await edit(...)\`), tests, data, bounded sub-model calls.

# Finishing
When the brief is done (or cannot be done), record your verdict in a cell: \`await rlm.finish(status, summary, evidence=[...], changed_files=[...])\` with status "passed", "failed" or "blocked", concrete evidence (commands with their outcome, files with lines) and every file you created, edited or deleted. The host checks changed_files against the files that changed while you ran. Then reply briefly: what you did, paths, and any uncertainty. Stay inside the brief: never do destructive things it did not ask for, and never commit or push unless it says so.`;

function environment(options: ClaudeGuideOptions): string | undefined {
	const lines = [
		options.cwd === undefined ? undefined : `- Working directory: ${options.cwd}`,
		options.platform === undefined ? undefined : `- Platform: ${options.platform}`,
		options.date === undefined ? undefined : `- Date: ${options.date}`,
	].filter((line): line is string => line !== undefined);
	return lines.length === 0 ? undefined : `# Environment\n${lines.join("\n")}`;
}

function delegationAllowance(allowance: number): string {
	return allowance > 0
		? `You may delegate: \`rlm.spawn\` works for you, up to ${allowance} more level${allowance === 1 ? "" : "s"} (\`depth=\` for your children at most ${allowance - 1}).`
		: "You do not delegate: `rlm.spawn` is refused for you. `rlm.map` and `rlm.infer` (bounded sub-model calls) work.";
}

/** Claude Code's whole system prompt for an Ultron root (or subagent). */
export function claudeSystemPrompt(audience: "claude" | "claude-child", options: ClaudeGuideOptions = {}): string {
	const env = environment(options);
	const parts =
		audience === "claude"
			? [ROOT_IDENTITY, WORKING_WITH_THE_USER, claudeRuntimeGuide()]
			: [CHILD_IDENTITY, delegationAllowance(options.allowance ?? 0), claudeRuntimeGuide()];
	return [...parts, ...(env === undefined ? [] : [env])].join("\n\n");
}

/** The guide for an audience: Ultron's own runtime guide, or Claude Code's system prompt. */
export function runtimeGuide(audience: GuideAudience, options: ClaudeGuideOptions = {}): string {
	if (audience === "ultron") return `${RLM_TOOL_DESCRIPTION}\n\n${rlmRuntimePrompt(["rlm"])!}`;
	return claudeSystemPrompt(audience === "claude" ? "claude" : "claude-child", options);
}
