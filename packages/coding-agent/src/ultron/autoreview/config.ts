/**
 * `ultron autoreview` configuration: the saved `autoreview.*` settings with their defaults, and where the
 * reviewer keeps its files. Settings are global only, so a repository under review cannot change them.
 */
import { homedir } from "node:os";
import { join } from "node:path";
import type { AutoreviewSettings, FrameThinkingLevel, RlmModelSettings } from "../../core/settings-manager.ts";
import { logoText } from "../../experimental/ultron-logo.ts";

export const DEFAULT_POLL_SECONDS = 45;
export const MIN_POLL_SECONDS = 20;
export const DEFAULT_CONCURRENCY = 3;
export const MAX_CONCURRENCY = 8;
/** 0: no token cap. */
export const DEFAULT_BUDGET_TOKENS = 0;
export const MIN_BUDGET_TOKENS = 10_000;
/** Model requests of one review in flight at once; more trips subscription rate limits. */
export const DEFAULT_FRAME_CONCURRENCY = 8;
export const MAX_FRAME_CONCURRENCY = 16;
export const DEFAULT_THINKING: FrameThinkingLevel = "low";
/**
 * `both` (default): the fast pass, then the deep pass with its findings as leads, each finding verified by a
 * verifier frame. `hybrid` and `compiled` are experimental: host-written check programs verify discovery's
 * candidates, or a planner writes the whole review program.
 */
export type ReviewMode = "fast" | "deep" | "both" | "compiled" | "hybrid";
export const REVIEW_MODES: readonly ReviewMode[] = ["fast", "deep", "both", "compiled", "hybrid"];
export const DEFAULT_MODE: ReviewMode = "both";
export const DEFAULT_VERIFY_CANDIDATES = 12;
export const MAX_VERIFY_CANDIDATES = 40;
export const DEFAULT_DEEP_THINKING: FrameThinkingLevel = "medium";
export const DEFAULT_PLAN_THINKING: FrameThinkingLevel = "high";
export const DEFAULT_ASK_THINKING: FrameThinkingLevel = "low";
/** The planner as sandboxed Python cells over the `rv` API (`cell`), or as one JSON-program frame (`frame`). */
export type PlanStyle = "cell" | "frame";
export const PLAN_STYLES: readonly PlanStyle[] = ["cell", "frame"];
export const DEFAULT_PLAN_STYLE: PlanStyle = "cell";
export const DEFAULT_PLAN_CELLS = 4;
export const MAX_PLAN_CELLS = 12;
export type BlockLevel = "critical" | "high" | "medium" | "low" | "nit";
export const BLOCK_LEVELS: readonly BlockLevel[] = ["critical", "high", "medium", "low", "nit"];
/** A confirmed finding at this level or above asks for changes. */
export const DEFAULT_BLOCK_AT: BlockLevel = "medium";
/** Few, heavy comments: inline comments per review. */
export const DEFAULT_MAX_COMMENTS = 5;
export const MAX_MAX_COMMENTS = 30;
export const DEFAULT_DEEP_ROUNDS = 3;
/** Findings of one file a verifier frame judges together. */
export const DEFAULT_VERIFY_BATCH = 4;
export const MAX_VERIFY_BATCH = 8;
/** Frames in flight per model at most (`autoreview.modelConcurrency` sets a model's own limit, up to this). */
export const DEFAULT_MODEL_CONCURRENCY = 8;
export const MAX_MODEL_CONCURRENCY = 16;
/**
 * The recommended model: used for every stage when the user's catalog has it and no setting names another
 * (see "Recommended models" in docs/autoreview.md).
 */
export const RECOMMENDED_MODEL = "cliproxyapi/gpt-6-luna";
export const DEFAULT_TEST_RUNS = 6;
export const MAX_TEST_RUNS = 30;
/** A safety limit, on by default: one test execution may not run longer. */
export const DEFAULT_TEST_TIMEOUT_SECONDS = 300;
export const MAX_DEEP_ROUNDS = 8;
/** 0: no deadline, the review waits for every frame. With one, what was found by then is verified and posted. */
export const DEFAULT_DEADLINE_SECONDS = 0;
export const MIN_DEADLINE_SECONDS = 30;
/** 0: no per-frame timeout from the pipeline. */
export const DEFAULT_FRAME_TIMEOUT_SECONDS = 0;
export const MIN_FRAME_TIMEOUT_SECONDS = 10;
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
	/** Token cap of one review; 0: none. */
	readonly budget: number;
	readonly frameConcurrency: number;
	readonly thinking: FrameThinkingLevel;
	readonly verifyThinking: FrameThinkingLevel;
	readonly mode: ReviewMode;
	/** `provider/model` of the investigator frames; undefined: the finder model. */
	readonly deepModel?: string;
	readonly deepThinking: FrameThinkingLevel;
	readonly deepRounds: number;
	/** `compiled` mode: the planner frame's model (undefined: the finder model) and thinking level. */
	readonly planModel?: string;
	readonly planThinking: FrameThinkingLevel;
	/** `compiled` mode: the small model the program's questions go to (undefined: the finder model). */
	readonly askModel?: string;
	readonly askThinking: FrameThinkingLevel;
	readonly planStyle: PlanStyle;
	readonly planCells: number;
	/** `hybrid` mode: candidates verified per review at most. */
	readonly verifyCandidates: number;
	/** Findings of one file one verifier frame judges together (1: one frame per finding). */
	readonly verifyBatch: number;
	/** Frames in flight per model at most, by `provider/model`; unset models get DEFAULT_MODEL_CONCURRENCY. */
	readonly modelConcurrency: Readonly<Record<string, number>>;
	/** Experimental: send each frame's shared context as its system prompt (measured to lose discovery; off). */
	readonly systemPrefix: boolean;
	readonly blockAt: BlockLevel;
	readonly maxComments: number;
	/** Run the reviewed project's tests in the deep pass, where the repository is eligible and a sandbox exists. */
	readonly runTests: boolean;
	/** Lower-cased owners whose repositories' tests may run even without push access. */
	readonly testOwners: readonly string[];
	readonly testRuns: number;
	readonly testTimeoutSeconds: number;
	/** Lower-cased `owner/repo` -> pre-built environment directory. */
	readonly testEnv: Readonly<Record<string, string>>;
	readonly testImage?: string;
	/** Prepare test environments (uv, npm ci, mise toolchains) before a repository's first review and on lockfile changes. */
	readonly prepareEnvs: boolean;
	/** Use mise for toolchains during prepare: true/false, or the path of the mise binary. */
	readonly mise: boolean | string;
	/** Directories of local checkouts whose prepared environments may serve sandboxed test runs. */
	readonly checkoutRoots: readonly string[];
	/** Private review guides: markdown files or directories of them. */
	readonly guides: readonly string[];
	/** 0: no deadline. */
	readonly deadlineSeconds: number;
	readonly frameTimeoutSeconds: number;
	readonly dryRun: boolean;
	readonly ack: boolean;
	readonly ackLines: readonly string[];
	/** Art appended to the acknowledgement in a fenced block (Ultron's logo by default); empty: none. */
	readonly ackArt: string;
	readonly signature: boolean;
}

/** What the model settings say besides `autoreview.*`: the fallbacks of `autoreview.model`. */
export interface ModelFallbacks {
	readonly reviewModel?: string;
	readonly rlm?: RlmModelSettings;
	readonly defaultProvider?: string;
	readonly defaultModel?: string;
	/** Whether the user's catalog has a `provider/model`; given, RECOMMENDED_MODEL leads the chain when it does. */
	readonly hasModel?: (ref: string) => boolean;
}

/**
 * The finder model: `autoreview.model`, then the recommended model when the catalog has it, then `review.model`,
 * then `rlm.frameModel`, then the default model. Undefined when none of them is set (the engine then resolves the
 * profile's default itself).
 */
export function resolveModel(settings: AutoreviewSettings, fallbacks: ModelFallbacks): string | undefined {
	return (
		settings.model ??
		(fallbacks.hasModel?.(RECOMMENDED_MODEL) ? RECOMMENDED_MODEL : undefined) ??
		fallbacks.reviewModel ??
		fallbacks.rlm?.frameModel ??
		(fallbacks.defaultProvider && fallbacks.defaultModel
			? `${fallbacks.defaultProvider}/${fallbacks.defaultModel}`
			: undefined)
	);
}

/** `autoreview.ackArt`: unset or "logo" is Ultron's half-size logo, "none" or false is no art, other text is itself. */
export function resolveAckArt(setting: string | false | undefined): string {
	if (setting === false) return "";
	const name = setting?.trim().toLowerCase();
	if (name === undefined || name === "" || name === "logo") return logoText();
	return name === "none" ? "" : setting!;
}

/** Dry-run files and logs older than this are removed. */
export const RETENTION_DAYS = 14;

/** The engine spec fields every review takes from the configuration. */
export function engineSettings(config: AutoreviewConfig): {
	model?: string;
	verifyModel?: string;
	budget?: number;
	concurrency: number;
	thinking: FrameThinkingLevel;
	verifyThinking: FrameThinkingLevel;
	mode: ReviewMode;
	deepModel?: string;
	deepThinking: FrameThinkingLevel;
	deepRounds: number;
	planModel?: string;
	planThinking: FrameThinkingLevel;
	askModel?: string;
	askThinking: FrameThinkingLevel;
	planStyle: PlanStyle;
	planCells: number;
	verifyCandidates: number;
	verifyBatch: number;
	modelConcurrency: Record<string, number>;
	systemPrefix: boolean;
	testRuns: number;
	testTimeoutSeconds: number;
	testImage?: string;
	deadlineSeconds: number;
	frameTimeoutSeconds: number;
} {
	return {
		...(config.model === undefined ? {} : { model: config.model }),
		...(config.verifyModel === undefined ? {} : { verifyModel: config.verifyModel }),
		...(config.budget > 0 ? { budget: config.budget } : {}),
		concurrency: config.frameConcurrency,
		thinking: config.thinking,
		verifyThinking: config.verifyThinking,
		mode: config.mode,
		...(config.deepModel === undefined ? {} : { deepModel: config.deepModel }),
		deepThinking: config.deepThinking,
		deepRounds: config.deepRounds,
		...(config.planModel === undefined ? {} : { planModel: config.planModel }),
		planThinking: config.planThinking,
		...(config.askModel === undefined ? {} : { askModel: config.askModel }),
		askThinking: config.askThinking,
		planStyle: config.planStyle,
		planCells: config.planCells,
		verifyCandidates: config.verifyCandidates,
		verifyBatch: config.verifyBatch,
		modelConcurrency: { ...config.modelConcurrency },
		systemPrefix: config.systemPrefix,
		testRuns: config.testRuns,
		testTimeoutSeconds: config.testTimeoutSeconds,
		...(config.testImage === undefined ? {} : { testImage: config.testImage }),
		deadlineSeconds: config.deadlineSeconds,
		frameTimeoutSeconds: config.frameTimeoutSeconds,
	};
}

/**
 * Whether a repository's tests may be run: the setting is on, and the reviewing account can push to the
 * repository or its owner is listed in `autoreview.testOwners`. Running a stranger's code is never the default.
 */
export function testsEligible(config: AutoreviewConfig, owner: string, canPush: boolean): boolean {
	return config.runTests && (canPush || config.testOwners.includes(owner.toLowerCase()));
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
		budget: settings.budget === undefined ? DEFAULT_BUDGET_TOKENS : Math.max(MIN_BUDGET_TOKENS, settings.budget),
		frameConcurrency: Math.min(
			MAX_FRAME_CONCURRENCY,
			Math.max(1, settings.frameConcurrency ?? DEFAULT_FRAME_CONCURRENCY),
		),
		thinking: settings.thinking ?? DEFAULT_THINKING,
		verifyThinking: settings.verifyThinking ?? DEFAULT_THINKING,
		mode: settings.mode ?? DEFAULT_MODE,
		...((settings.deepModel ?? model) === undefined ? {} : { deepModel: settings.deepModel ?? model }),
		deepThinking: settings.deepThinking ?? DEFAULT_DEEP_THINKING,
		deepRounds: Math.min(MAX_DEEP_ROUNDS, Math.max(1, settings.deepRounds ?? DEFAULT_DEEP_ROUNDS)),
		...((settings.planModel ?? model) === undefined ? {} : { planModel: settings.planModel ?? model }),
		planThinking: settings.planThinking ?? DEFAULT_PLAN_THINKING,
		...((settings.askModel ?? model) === undefined ? {} : { askModel: settings.askModel ?? model }),
		askThinking: settings.askThinking ?? DEFAULT_ASK_THINKING,
		planStyle: settings.planStyle ?? DEFAULT_PLAN_STYLE,
		planCells: Math.min(MAX_PLAN_CELLS, Math.max(1, settings.planCells ?? DEFAULT_PLAN_CELLS)),
		verifyCandidates: Math.min(
			MAX_VERIFY_CANDIDATES,
			Math.max(1, settings.verifyCandidates ?? DEFAULT_VERIFY_CANDIDATES),
		),
		verifyBatch: Math.min(MAX_VERIFY_BATCH, Math.max(1, settings.verifyBatch ?? DEFAULT_VERIFY_BATCH)),
		systemPrefix: settings.systemPrefix === true,
		modelConcurrency: Object.fromEntries(
			Object.entries(settings.modelConcurrency ?? {}).map(([name, limit]) => [
				name,
				Math.min(MAX_MODEL_CONCURRENCY, Math.max(1, limit)),
			]),
		),
		blockAt: settings.blockAt ?? DEFAULT_BLOCK_AT,
		maxComments: Math.min(MAX_MAX_COMMENTS, settings.maxComments ?? DEFAULT_MAX_COMMENTS),
		runTests: settings.runTests ?? true,
		testOwners: (settings.testOwners ?? []).map((owner) => owner.toLowerCase()),
		testRuns: Math.min(MAX_TEST_RUNS, settings.testRuns ?? DEFAULT_TEST_RUNS),
		testTimeoutSeconds: Math.max(5, settings.testTimeoutSeconds ?? DEFAULT_TEST_TIMEOUT_SECONDS),
		testEnv: settings.testEnv ?? {},
		prepareEnvs: settings.prepareEnvs ?? true,
		mise: settings.mise ?? true,
		checkoutRoots: settings.checkoutRoots ?? [],
		guides: settings.guides ?? [],
		...(settings.testImage === undefined ? {} : { testImage: settings.testImage }),
		deadlineSeconds:
			settings.deadlineSeconds === undefined || settings.deadlineSeconds === 0
				? DEFAULT_DEADLINE_SECONDS
				: Math.max(MIN_DEADLINE_SECONDS, settings.deadlineSeconds),
		frameTimeoutSeconds:
			settings.frameTimeoutSeconds === undefined
				? DEFAULT_FRAME_TIMEOUT_SECONDS
				: Math.max(MIN_FRAME_TIMEOUT_SECONDS, settings.frameTimeoutSeconds),
		dryRun: settings.dryRun ?? false,
		ack: settings.ack ?? true,
		ackLines: settings.ackLines ?? DEFAULT_ACK_LINES,
		ackArt: resolveAckArt(settings.ackArt),
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
