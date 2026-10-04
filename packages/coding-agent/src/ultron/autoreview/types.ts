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
	readonly severity: Severity;
	readonly category: string;
	readonly claim: string;
	readonly why: string;
	readonly suggestedFix?: string;
	/** The exact new text of lines `line..endLine`, when the fix is a drop-in replacement. */
	readonly replacement?: string;
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
	};
	readonly usage: {
		readonly inputTokens: number;
		readonly outputTokens: number;
		readonly costUsd: number;
		readonly frames: number;
		readonly tokens: number;
		readonly budget: number;
	};
	readonly model: string | null;
	readonly verifyModel: string | null;
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
