/** The review engine's contract: what the host asks `autoreview_api.run` to review, and what comes back. */

/** The levels of a finding, most serious first. */
export type Level = "critical" | "high" | "medium" | "low" | "nit";
export const LEVELS: readonly Level[] = ["critical", "high", "medium", "low", "nit"];

/** The older four-name scale, still written as `severity` for consumers that know only that one. */
export type Severity = "blocker" | "major" | "minor" | "nit";
export const SEVERITIES: readonly Severity[] = ["blocker", "major", "minor", "nit"];

const OLD_TO_LEVEL: Readonly<Record<string, Level>> = { blocker: "critical", major: "high", minor: "low" };

/** A finding's level: its `level`, or its old-scale `severity` (blocker, major, minor) mapped to one. */
export function levelOf(finding: { readonly level?: string; readonly severity?: string }): Level {
	for (const name of [finding.level, finding.severity]) {
		const level = LEVELS.find((item) => item === name) ?? (name === undefined ? undefined : OLD_TO_LEVEL[name]);
		if (level !== undefined) return level;
	}
	return "low";
}

/**
 * A level on the old scale. Medium has no equal there: it is written as "minor", the nearest in meaning (a real
 * gap, not a demonstrated wrong result), although a medium finding asks for changes by default.
 */
export function severityOf(level: Level): Severity {
	return ({ critical: "blocker", high: "major", medium: "minor", low: "minor", nit: "nit" } as const)[level];
}

export interface ContextComment {
	readonly author: string;
	readonly body: string;
	readonly path?: string;
	readonly line?: number;
}

/** Untrusted pull request material given to the finder frames as data. */
export interface ReviewContext {
	readonly title?: string;
	readonly description?: string;
	readonly ci?: string;
	readonly comments?: readonly ContextComment[];
}

export interface EarlierFinding {
	readonly id: string;
	readonly file: string;
	readonly line: number;
	readonly severity: string;
	readonly claim: string;
}

export interface EngineSpec {
	/** Offline: a local repository and the two commits to diff (source is read from `head`). */
	readonly repoDir?: string;
	readonly base?: string;
	readonly head?: string;
	/** Pull request: a checked-out worktree at the head commit, or absent when only the diff is known. */
	readonly workDir?: string;
	/** The diff to review, as a file. */
	readonly diffPath?: string;
	/** The whole pull request diff (inline comments are validated against it); default: the reviewed diff. */
	readonly postDiffPath?: string;
	readonly label?: string;
	readonly model?: string;
	readonly verifyModel?: string;
	readonly budget?: number;
	/** Model requests in flight at once. */
	readonly concurrency?: number;
	/** Thinking level of the finder and of the verifier frames. */
	readonly thinking?: string;
	readonly verifyThinking?: string;
	/** `fast`, `deep`, `both` or `compiled`; the deep pass and the compiled mode need `repoDir` or `workDir`. */
	readonly mode?: "fast" | "deep" | "both" | "compiled";
	readonly deepModel?: string;
	readonly deepThinking?: string;
	readonly deepRounds?: number;
	/** `compiled` mode: the planner frame's model and thinking, and the small model the program's questions go to. */
	readonly planModel?: string;
	readonly planThinking?: string;
	readonly askModel?: string;
	readonly askThinking?: string;
	/** `compiled` mode: execute this saved program instead of calling the planner (benchmarking the interpreter). */
	readonly programPath?: string;
	/** `compiled` mode: write the validated program here for inspection. */
	readonly dumpProgramPath?: string;
	/** The deep pass may run the project's tests, sandboxed (the host has checked eligibility). */
	readonly runTests?: boolean;
	readonly testRuns?: number;
	readonly testTimeoutSeconds?: number;
	/** A pre-built environment directory to bind read-only into the sandbox. */
	readonly testEnv?: string;
	readonly testImage?: string;
	/**
	 * A local checkout of the same repository (the host has checked its remote): its prepared environment
	 * directories are bound read-only into the test sandbox. `testEnv` takes its place when given.
	 */
	readonly testCheckout?: string;
	/** The user's private review guides (files or directories), and `owner/repo` to pick the specific ones. */
	readonly guides?: readonly string[];
	readonly repo?: string;
	/** With `workDir`: the commit the pull request branched from (tests failing at head are re-run there). */
	readonly baseSha?: string;
	/** Seconds the review may take (0: no deadline), and seconds one frame may take. */
	readonly deadlineSeconds?: number;
	readonly frameTimeoutSeconds?: number;
	/** First retry delay for a transient frame failure (tests shorten it). */
	readonly retryBaseSeconds?: number;
	readonly only?: readonly string[];
	readonly context?: ReviewContext;
	/** Findings an earlier review posted, to re-check. */
	readonly earlier?: readonly EarlierFinding[];
	/** The diff from the earlier reviewed commit to this one, as a file (absent after a force-push). */
	readonly earlierDiffPath?: string;
	/** Offline: the earlier reviewed commit. */
	readonly earlierBase?: string;
}

export interface EngineFinding {
	readonly id?: number;
	readonly file: string;
	readonly line: number;
	readonly endLine?: number;
	/** The final level: the verifier's rating under the rubric (for an uncertain finding, the finder's, capped). */
	readonly level?: Level;
	/** The level on the old four-name scale. */
	readonly severity: Severity;
	/** What the finder rated it. */
	readonly finderLevel?: Level;
	readonly finderSeverity?: Severity;
	/** How strong the evidence is: a test the host ran, source quoted from outside the diff, or the diff alone. */
	readonly strength?: "test" | "outside" | "diff";
	/** A tests finding: the behaviour, the change no test would notice, the nearest test, and how a run settled it. */
	readonly unpinned?: {
		readonly behaviour: string;
		readonly change: string;
		readonly closestTest?: { readonly path: string; readonly line: number } | null;
		readonly mutation?: { readonly path: string; readonly line: number; readonly replacement: string };
		readonly proof?: "proven";
	};
	/** A maintainability finding: the problem that exists now. */
	readonly consequence?: string;
	/** The verifier called it a judgement call: never posted, never blocking. */
	readonly unclear?: boolean;
	/** Other places with the same root cause, folded into this finding. */
	readonly alsoAt?: ReadonlyArray<{ readonly file: string; readonly line: number }>;
	/** The concrete failure the finder stated: input or state, what happens, what should. Empty when none. */
	readonly scenario?: string;
	readonly category: string;
	readonly claim: string;
	readonly why: string;
	readonly suggestedFix?: string;
	/** The exact new text of lines `line..endLine`, when the fix is a drop-in replacement. */
	readonly replacement?: string;
	/** Which pass raised it: `fast`, `deep:<lens>`, or `compiled:<step id>`. */
	readonly source?: string;
	/** A deep finding's citations, each quote checked by the host at its line of the reviewed commit. */
	readonly citations?: ReadonlyArray<{ readonly path: string; readonly line: number; readonly quote: string }>;
	/** One line on how the finding was verified. */
	readonly howVerified?: string;
	readonly verification: "confirmed" | "uncertain";
	readonly confidence: number;
	readonly evidence?: string;
	readonly note?: string;
}

export interface EarlierStatus {
	readonly id: string;
	readonly file: string;
	readonly line: number;
	readonly claim: string;
	readonly severity: string;
	readonly status: "fixed" | "still_present" | "not_applicable" | "unknown";
	readonly evidence: string;
}

export interface AlsoRaised {
	readonly file: string;
	readonly line: number;
	readonly severity: string;
	readonly claim: string;
	readonly by: readonly string[];
}

export interface FrameTiming {
	readonly phase: "find" | "verify" | "recheck" | "deep" | "plan" | "ask";
	/** The reviewer key of a finder frame; "verifier", "recheck", "planner" or an ask step's id otherwise. */
	readonly reviewer: string;
	readonly ms: number;
	readonly status: "ok" | "incomplete" | "failed" | "timeout" | "deadline" | "budget";
	readonly retries: number;
	readonly tokens?: number;
}

export interface TestRun {
	readonly n: number;
	/** `automatic`, `base` (the comparison run), `run` or `mutation`. */
	readonly kind: string;
	readonly rev: string;
	readonly command: string;
	readonly paths: readonly string[];
	/** `passed`, `failed`, `unavailable` (it could not run: missing dependencies) or `timeout`. */
	readonly status: string;
	readonly passed: number;
	readonly failed: number;
	readonly ms: number;
}

export interface TestReport {
	/** Whether the host allowed test execution for this review. */
	readonly enabled: boolean;
	/** `bwrap`, `unshare`, `docker`, or null when nothing ran. */
	readonly mechanism: string | null;
	/** Why tests did not run, when they did not. */
	readonly note: string | null;
	readonly runs: readonly TestRun[];
}

export interface InvestigatorTiming {
	readonly lens: string;
	readonly rounds: number;
	/** Lookups served, and requests refused by validation. */
	readonly requests: number;
	readonly rejected: number;
	readonly ms: number;
	readonly tokens: number;
	readonly findings: number;
	readonly status: string;
	readonly error?: string;
}

/** One step of a review program as the host ran it. */
export interface ProgramStep {
	readonly id: string;
	readonly op: string;
	readonly status: "ok" | "failed" | "skipped";
	readonly ms: number;
	readonly tokens: number;
	readonly input: string;
	readonly output: string;
	readonly detail?: string;
	/** A finding whose check was unknown or contradicted the planner's expectation was put to the small model. */
	readonly resolved?: "ask";
	readonly ask?: string;
	readonly answer?: string;
}

/** The `compiled` mode's account of its program. */
export interface ProgramStats {
	/** Steps as the planner wrote them, steps `for_each` added, and how they ended. */
	readonly planned?: number;
	readonly expanded?: number;
	readonly executed?: number;
	readonly failed?: number;
	readonly skipped?: number;
	/** Asks in all, and those the host generated for findings whose check was unknown or contradicted. */
	readonly asks?: number;
	readonly autoAsks?: number;
	readonly tests?: number;
	/** The asserts: true, false, unknown, and those that came out against the planner's expectation. */
	readonly checks?: {
		readonly held: number;
		readonly failed: number;
		readonly unknown: number;
		readonly contradicted: number;
	};
	readonly findings?: {
		readonly deterministic: number;
		readonly asked: number;
		/** Findings the small model decided after their check was unknown or contradicted. */
		readonly resolved: number;
		readonly dropped: number;
		readonly refuted: number;
		/** Finding steps that emitted nothing: gate false, undecided, or the small model said no or unclear. */
		readonly notEmitted?: {
			readonly gateFalse: number;
			readonly undecided: number;
			readonly askedNo: number;
			readonly askedUnclear: number;
		};
	};
	readonly truncated?: readonly string[];
	/** What the map said the program must cover, and the items the planner declared it could not. */
	readonly coverage?: { readonly items: number; readonly covered: number; readonly uncovered: readonly string[] };
	/** The step limits this review ran under (larger for a diff of more than 100 changed lines). */
	readonly limits?: { readonly planned: number; readonly expanded: number };
	readonly planner: {
		readonly ms: number;
		readonly tokens: number;
		readonly repairs: number;
		readonly status: string;
	};
	readonly summary?: string;
	/** Why the compiled mode gave way to the fast and deep passes, when it did. */
	readonly fallback?: string;
}

export interface EngineResult {
	readonly complete: boolean;
	readonly label: string;
	readonly files: number;
	readonly added: number;
	readonly removed: number;
	readonly findings: readonly EngineFinding[];
	readonly alsoRaised: readonly AlsoRaised[];
	readonly earlier: readonly EarlierStatus[];
	readonly dropped: {
		readonly rejected: number;
		readonly duplicates: number;
		/** Generic findings dropped by rule: tests findings naming no unpinned change, maintainability without a present problem. */
		readonly generic?: number;
		/** Tests findings whose named change an existing test did catch when the host ran it. */
		readonly refutedByTest?: number;
	};
	readonly timing: {
		readonly totalMs: number;
		readonly scopeMs: number;
		readonly findMs: number;
		readonly verifyMs: number;
		readonly deepMs?: number;
		readonly programMs?: number;
		/** How each frame went, in the order they finished. */
		readonly frames?: readonly FrameTiming[];
		/** The deep pass's investigators: lookup rounds, requests served, time and tokens. */
		readonly investigators?: readonly InvestigatorTiming[];
		/** The compiled mode's steps, in the order they finished. */
		readonly program?: readonly ProgramStep[];
	};
	readonly usage: {
		readonly inputTokens: number;
		readonly outputTokens: number;
		readonly costUsd: number;
		readonly frames: number;
		readonly tokens: number;
		/** The token cap, or null when there was none. */
		readonly budget: number | null;
	};
	readonly model: string | null;
	readonly verifyModel: string | null;
	readonly thinking?: string | null;
	readonly verifyThinking?: string | null;
	/** The mode that ran (`fast` when the deep pass could not; `both` when the compiled mode fell back). */
	readonly mode?: "fast" | "deep" | "both" | "compiled";
	readonly deepModel?: string | null;
	readonly deepThinking?: string | null;
	readonly planModel?: string | null;
	readonly planThinking?: string | null;
	readonly askModel?: string | null;
	readonly askThinking?: string | null;
	/** The compiled mode's program stats; null when it did not run. */
	readonly program?: ProgramStats | null;
	/** What the deep pass traced and found to hold: the sentences the summary opens with. */
	readonly assurance?: readonly string[];
	/** The test executions of the deep pass. */
	readonly tests?: TestReport;
	/** How many review guides were used, and their file names and paths (which must not appear in what is posted). */
	readonly guides?: number;
	readonly guideNames?: readonly string[];
	readonly notChecked: readonly string[];
	/** Why coverage is incomplete; empty when `complete`. */
	readonly incomplete: readonly string[];
	/** New-file line ranges of the diff's hunks, by path. */
	readonly diffLines: Readonly<Record<string, ReadonlyArray<readonly [number, number]>>>;
}

/** Runs reviews. The real one hosts a kernel and frames (engine.ts); tests use a fake. */
export interface ReviewEngine {
	review(spec: EngineSpec, signal?: AbortSignal): Promise<EngineResult>;
	/** The model frames run on when a spec names none. */
	readonly defaultModel?: string;
	close(): Promise<void>;
}
