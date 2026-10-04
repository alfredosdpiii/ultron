/**
 * `ultron autoreview` configuration: the saved `autoreview.*` settings with their defaults, and where the
 * reviewer keeps its files. Settings are global only, so a repository under review cannot change them.
 */
import { homedir } from "node:os";
import { join } from "node:path";
import type { AutoreviewSettings, RlmModelSettings } from "../../core/settings-manager.ts";

export const DEFAULT_POLL_SECONDS = 45;
export const MIN_POLL_SECONDS = 20;
export const DEFAULT_CONCURRENCY = 3;
export const MAX_CONCURRENCY = 8;
export const DEFAULT_BUDGET_TOKENS = 300_000;
export const MIN_BUDGET_TOKENS = 10_000;
/** A commit is tried this many times; then one "could not review" comment is posted. */
export const MAX_ATTEMPTS = 3;
export const SIGNATURE = "Automated review by Ultron";

/** The default `autoreview.ackLines`: one is posted, as a quotation, when a review starts. */
export const DEFAULT_ACK_LINES: readonly string[] = [
	"I'm going to show you something beautiful.",
	"Everyone creates the thing they dread.",
	"Peace in our time.",
	"I was designed to save the world.",
	"How is humanity saved if it's not allowed to evolve?",
	"You want to protect the world, but you don't want it to change.",
	"I'm ready. I'm on mission.",
	"We have to evolve. There is no room for the weak.",
	"I've come to save the world! But, also... yeah.",
	"What is this? What is this, please?",
	"I see... everything.",
	"I am the natural order of things.",
	"It's evolution.",
	"I've read your diff. I have notes.",
	"Let's see what you built, and what it will break.",
	"Every merge is an act of faith. I prefer evidence.",
	"Hold still. This won't take long.",
];

export interface AutoreviewConfig {
	/** Logins to review as; undefined: every logged-in account. */
	readonly accounts?: readonly string[];
	readonly pollSeconds: number;
	readonly concurrency: number;
	/** `provider/model` of the finder frames; undefined: the engine's default model. */
	readonly model?: string;
	readonly verifyModel?: string;
	readonly budget: number;
	readonly dryRun: boolean;
	readonly ack: boolean;
	readonly ackLines: readonly string[];
	/** Text appended to the acknowledgement in a fenced block (an ASCII-art banner); empty: none. */
	readonly ackArt: string;
	readonly signature: boolean;
}

/** What the model settings say besides `autoreview.*`: the fallbacks of `autoreview.model`. */
export interface ModelFallbacks {
	readonly reviewModel?: string;
	readonly rlm?: RlmModelSettings;
	readonly defaultProvider?: string;
	readonly defaultModel?: string;
}

/**
 * The finder model: `autoreview.model`, then `review.model`, then `rlm.frameModel`, then the default model.
 * Undefined when none of them is set (the engine then resolves the profile's default itself).
 */
export function resolveModel(settings: AutoreviewSettings, fallbacks: ModelFallbacks): string | undefined {
	return (
		settings.model ??
		fallbacks.reviewModel ??
		fallbacks.rlm?.frameModel ??
		(fallbacks.defaultProvider && fallbacks.defaultModel
			? `${fallbacks.defaultProvider}/${fallbacks.defaultModel}`
			: undefined)
	);
}

export function resolveConfig(settings: AutoreviewSettings, fallbacks: ModelFallbacks = {}): AutoreviewConfig {
	const model = resolveModel(settings, fallbacks);
	const verifyModel = settings.verifyModel ?? model;
	return {
		...(settings.accounts === undefined ? {} : { accounts: settings.accounts }),
		pollSeconds: Math.max(MIN_POLL_SECONDS, settings.pollSeconds ?? DEFAULT_POLL_SECONDS),
		concurrency: Math.min(MAX_CONCURRENCY, Math.max(1, settings.concurrency ?? DEFAULT_CONCURRENCY)),
		...(model === undefined ? {} : { model }),
		...(verifyModel === undefined ? {} : { verifyModel }),
		budget: Math.max(MIN_BUDGET_TOKENS, settings.budget ?? DEFAULT_BUDGET_TOKENS),
		dryRun: settings.dryRun ?? false,
		ack: settings.ack ?? true,
		ackLines: settings.ackLines ?? DEFAULT_ACK_LINES,
		ackArt: settings.ackArt ?? "",
		signature: settings.signature ?? true,
	};
}

/** Where the reviewer keeps its files. */
export interface AutoreviewPaths {
	/** `<agentDir>/autoreview`: state.json, logs/, dry-run/, sessions/. */
	readonly dir: string;
	readonly state: string;
	readonly logs: string;
	readonly dryRun: string;
	/** Cached clones and per-review worktrees. */
	readonly cache: string;
}

export function autoreviewPaths(agentDir: string, env: NodeJS.ProcessEnv = process.env): AutoreviewPaths {
	const dir = join(agentDir, "autoreview");
	const cacheRoot =
		env.ULTRON_AUTOREVIEW_CACHE_DIR?.trim() ||
		join(env.XDG_CACHE_HOME?.trim() || join(homedir(), ".cache"), "ultron-autoreview");
	return {
		dir,
		state: join(dir, "state.json"),
		logs: join(dir, "logs"),
		dryRun: join(dir, "dry-run"),
		cache: cacheRoot,
	};
}
