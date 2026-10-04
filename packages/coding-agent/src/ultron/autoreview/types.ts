/** The review engine's contract: what the host asks `autoreview_api.run` to review, and what comes back. */

export type Severity = "blocker" | "major" | "minor" | "nit";
export const SEVERITIES: readonly Severity[] = ["blocker", "major", "minor", "nit"];

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
	/** `fast`, `deep` or `both`; the deep pass needs `repoDir` or `workDir`. */
	readonly mode?: "fast" | "deep" | "both";
	readonly deepModel?: string;
	readonly deepThinking?: string;
	readonly deepRounds?: number;
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
	/** The final severity: the verifier's rating under the rubric (for an uncertain finding, the finder's, capped). */
	readonly severity: Severity;
	/** What the finder rated it. */
	readonly finderSeverity?: Severity;
	/** The concrete failure the finder stated: input or state, what happens, what should. Empty when none. */
	readonly scenario?: string;
	readonly category: string;
	readonly claim: string;
	readonly why: string;
	readonly suggestedFix?: string;
	/** The exact new text of lines `line..endLine`, when the fix is a drop-in replacement. */
	readonly replacement?: string;
	/** Which pass raised it: `fast`, or `deep:<lens>`. */
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
	readonly phase: "find" | "verify" | "recheck" | "deep";
	/** The reviewer key of a finder frame; "verifier" or "recheck" otherwise. */
	readonly reviewer: string;
	readonly ms: number;
	readonly status: "ok" | "incomplete" | "failed" | "timeout" | "deadline" | "budget";
	readonly retries: number;
	readonly tokens?: number;
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

export interface EngineResult {
	readonly complete: boolean;
	readonly label: string;
	readonly files: number;
	readonly added: number;
	readonly removed: number;
	readonly findings: readonly EngineFinding[];
	readonly alsoRaised: readonly AlsoRaised[];
	readonly earlier: readonly EarlierStatus[];
	readonly dropped: { readonly rejected: number; readonly duplicates: number };
	readonly timing: {
		readonly totalMs: number;
		readonly scopeMs: number;
		readonly findMs: number;
		readonly verifyMs: number;
		readonly deepMs?: number;
		/** How each frame went, in the order they finished. */
		readonly frames?: readonly FrameTiming[];
		/** The deep pass's investigators: lookup rounds, requests served, time and tokens. */
		readonly investigators?: readonly InvestigatorTiming[];
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
	/** The mode that ran (`fast` when the deep pass could not). */
	readonly mode?: "fast" | "deep" | "both";
	readonly deepModel?: string | null;
	readonly deepThinking?: string | null;
	/** What the deep pass traced and found to hold: the sentences the summary opens with. */
	readonly assurance?: readonly string[];
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
