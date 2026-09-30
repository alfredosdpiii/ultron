/**
 * The context files (AGENTS.md / CLAUDE.md) and Agent Skills that native Ultron puts in its system prompt, loaded for
 * `ultron claude` and its Claude Code subagents with the same resource loader, settings and project-trust rules as
 * the session worker, and rendered exactly as native renders them (`<project_context>` and `<skills>` sections, the
 * skills listed by name, description and SKILL.md path for the model to read in a cell when a task matches).
 *
 * Extensions are not loaded here (loading one runs its code); skills that only an extension contributes at runtime
 * are left out. Claude Code's own CLAUDE.md discovery stays off (`--setting-sources ""`), so CLAUDE.md files reach
 * the model only through Ultron's loader, as in native Ultron.
 */
import { getAgentDir } from "../../config.ts";
import { DefaultResourceLoader } from "../../core/resource-loader.ts";
import { SettingsManager } from "../../core/settings-manager.ts";
import type { Skill } from "../../core/skills.ts";
import { buildSystemPromptSections } from "../../core/system-prompt.ts";
import { workerProjectTrusted } from "../../experimental/services/worker-settings.ts";

export interface ClaudePromptResources {
	readonly contextFiles: ReadonlyArray<{ readonly path: string; readonly content: string }>;
	readonly skills: readonly Skill[];
}

export interface ClaudeResourceFlags {
	/** `--no-context-files`: no AGENTS.md / CLAUDE.md. */
	readonly noContextFiles?: boolean;
	/** `--no-skills`: no discovered skills. */
	readonly noSkills?: boolean;
}

export const NO_PROMPT_RESOURCES: ClaudePromptResources = { contextFiles: [], skills: [] };

/** Load the context files and skills native Ultron would use in `cwd`. */
export async function loadClaudePromptResources(
	cwd: string,
	flags: ClaudeResourceFlags = {},
	agentDir: string = getAgentDir(),
): Promise<ClaudePromptResources> {
	if (flags.noContextFiles === true && flags.noSkills === true) return NO_PROMPT_RESOURCES;
	// The session worker's rule: a project the user distrusted (`/trust`, `defaultProjectTrust: "never"`) loads no
	// project settings or project skills.
	const settingsManager = SettingsManager.create(cwd, agentDir, {
		projectTrusted: workerProjectTrusted(cwd, agentDir),
	});
	const loader = new DefaultResourceLoader({
		cwd,
		agentDir,
		settingsManager,
		noExtensions: true,
		noPromptTemplates: true,
		noThemes: true,
		...(flags.noContextFiles === true ? { noContextFiles: true } : {}),
		...(flags.noSkills === true ? { noSkills: true } : {}),
	});
	await loader.reload();
	return { contextFiles: loader.getAgentsFiles().agentsFiles, skills: loader.getSkills().skills };
}

/** The `<project_context>` and `<skills>` sections as native Ultron renders them for the REPL-only tool set. */
export function renderClaudePromptResources(resources: ClaudePromptResources): string[] {
	if (resources.contextFiles.length === 0 && resources.skills.length === 0) return [];
	const sections = buildSystemPromptSections({
		// Only the two sections are used; the preamble is a placeholder.
		customPrompt: "-",
		cwd: "",
		selectedTools: ["rlm"],
		contextFiles: resources.contextFiles.map((file) => ({ ...file })),
		skills: [...resources.skills],
	});
	return [sections.project_context, sections.skills].filter((section): section is string => section !== undefined);
}

/** What `--print-config` shows: paths and names only, never file contents. */
export function describeClaudePromptResources(resources: ClaudePromptResources): {
	contextFiles: string[];
	skills: string[];
} {
	return {
		contextFiles: resources.contextFiles.map((file) => file.path),
		skills: resources.skills.filter((skill) => !skill.disableModelInvocation).map((skill) => skill.name),
	};
}
