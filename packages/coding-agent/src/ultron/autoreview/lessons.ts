/**
 * What a repository taught the reviewer: the fate of every finding it posted there, and the rules drawn from them.
 *
 * A fate is observed, never inferred by a model: a finding fixed in a later commit or given a thumbs-up was
 * accepted; one whose thread a human resolved without a fix, one given a thumbs-down, or one still open when the
 * pull request merged was rejected. A reply alone decides nothing. Fates live in the reviewer's state (the ledger
 * the rules are computed from) and, when Hindsight is configured, are retained there too, as experiences of the
 * repository's memory, with the rule as an observation, so a session can ask what a repository's reviewers
 * accepted.
 *
 * A rule needs a sample: in the last 180 days, at least five findings of one category from one kind of pass
 * (`fast`, `deep:<lens>`, `structure:<shape>`, `compiled`), fewer than half of them accepted. Such a finding is
 * still posted, at `low`, with the count beside it; nothing is ever suppressed by memory alone, and the rule
 * applies after the verifier, never inside a prompt.
 */
import { createHash } from "node:crypto";
import { createHindsightBackend, type MemoryBackend } from "../memory.ts";

export const MIN_SAMPLES = 5;
export const LOWER_BELOW = 0.5;
export const FATE_DAYS = 180;
export const MAX_FATES_PER_REPO = 400;
const BANK = "ultron-autoreview";
const DAY_MS = 86_400_000;

export type FateKind = "accepted" | "rejected";

export interface Fate {
	/** The posted finding's id within its pull request. */
	readonly id: string;
	/** `host/owner/repo#N`. */
	readonly pull: string;
	readonly file: string;
	readonly line: number;
	readonly category: string;
	/** The kind of pass that raised it: `fast`, `deep:<lens>`, `structure:<shape>`, `compiled` or `hybrid`. */
	readonly kind: string;
	readonly level: string;
	readonly claim: string;
	readonly fate: FateKind;
	/** How the fate was observed. */
	readonly how: string;
	/** ISO time of the observation. */
	readonly at: string;
}

export interface Lesson {
	readonly category: string;
	readonly kind: string;
	readonly accepted: number;
	readonly rejected: number;
	/** Fewer than half accepted on a full sample: such findings are posted at `low`. */
	readonly lowered: boolean;
}

/** What a review thread shows about a posted finding. */
export interface ThreadState {
	readonly id: string;
	readonly resolved: boolean;
	readonly thumbsUp?: number;
	readonly thumbsDown?: number;
	/** Comments after the first (replies, by anyone). */
	readonly replies?: number;
}

export interface FateInput {
	readonly pull: string;
	readonly now: string;
	readonly findings: ReadonlyArray<{
		readonly id: string;
		readonly file: string;
		readonly line: number;
		readonly claim: string;
		readonly severity: string;
		readonly category?: string;
		readonly kind?: string;
		readonly commentId?: number;
		readonly status: string;
	}>;
	/** The re-review's status of each earlier finding, by id (absent: the state's own status). */
	readonly statuses?: ReadonlyMap<string, string>;
	/** Threads by the database id of their first comment. */
	readonly threads?: ReadonlyMap<number, ThreadState>;
	/** Threads this account resolved itself (a fixed finding), never a human's dismissal. */
	readonly resolvedByUs?: ReadonlySet<string>;
	/** The pull request merged: a finding still open then was passed over. */
	readonly merged?: boolean;
}

/** The fates the current observation settles. A finding with nothing new about it yields none. */
export function fatesOf(input: FateInput): Fate[] {
	const out: Fate[] = [];
	for (const finding of input.findings) {
		const status = input.statuses?.get(finding.id) ?? finding.status;
		const thread = finding.commentId === undefined ? undefined : input.threads?.get(finding.commentId);
		const base = {
			id: finding.id,
			pull: input.pull,
			file: finding.file,
			line: finding.line,
			category: finding.category ?? "unknown",
			kind: finding.kind ?? "unknown",
			level: finding.severity,
			claim: finding.claim,
			at: input.now,
		};
		const up = thread?.thumbsUp ?? 0;
		const down = thread?.thumbsDown ?? 0;
		if (down > 0 && up === 0) out.push({ ...base, fate: "rejected", how: "thumbs down on the comment" });
		else if (up > 0 && down === 0) out.push({ ...base, fate: "accepted", how: "thumbs up on the comment" });
		else if (status === "fixed") out.push({ ...base, fate: "accepted", how: "fixed in a later commit" });
		else if (thread?.resolved && !input.resolvedByUs?.has(thread.id) && status !== "not_applicable")
			out.push({ ...base, fate: "rejected", how: "the thread was resolved without a fix" });
		else if (input.merged && (status === "open" || status === "still_present"))
			out.push({ ...base, fate: "rejected", how: "merged with the finding open" });
	}
	return out;
}

/** The ledger with these fates added; a finding's later fate replaces its earlier one, bounded per repository. */
export function mergeFates(ledger: readonly Fate[], fates: readonly Fate[]): Fate[] {
	const byKey = new Map(ledger.map((fate) => [`${fate.pull}:${fate.id}`, fate]));
	for (const fate of fates) byKey.set(`${fate.pull}:${fate.id}`, fate);
	const merged = [...byKey.values()].sort((a, b) => a.at.localeCompare(b.at));
	return merged.length > MAX_FATES_PER_REPO ? merged.slice(-MAX_FATES_PER_REPO) : merged;
}

/** The rules a repository's recent fates support. */
export function lessonsFor(ledger: readonly Fate[], now: string): Lesson[] {
	const since = Date.parse(now) - FATE_DAYS * DAY_MS;
	const groups = new Map<string, { category: string; kind: string; accepted: number; rejected: number }>();
	for (const fate of ledger) {
		const at = Date.parse(fate.at);
		if (Number.isNaN(at) || at < since) continue;
		const key = `${fate.category}\u0000${fate.kind}`;
		const group = groups.get(key) ?? { category: fate.category, kind: fate.kind, accepted: 0, rejected: 0 };
		if (fate.fate === "accepted") group.accepted += 1;
		else group.rejected += 1;
		groups.set(key, group);
	}
	return [...groups.values()]
		.filter((group) => group.accepted + group.rejected >= MIN_SAMPLES)
		.map((group) => ({ ...group, lowered: group.accepted / (group.accepted + group.rejected) < LOWER_BELOW }))
		.sort((a, b) => a.category.localeCompare(b.category) || a.kind.localeCompare(b.kind));
}

/** The Hindsight URL to use: the environment, else the setting, else the default; "off" and its kin: none. */
export function hindsightUrlFrom(
	env: string | undefined,
	setting: string | undefined,
	fallback: string,
): string | undefined {
	const value = env?.trim() ? env.trim() : setting?.trim();
	if (!value) return fallback;
	return ["off", "none", "0", "false"].includes(value.toLowerCase()) ? undefined : value;
}

function repoIdentity(repo: string): string {
	return createHash("sha256").update(repo.toLowerCase()).digest("hex").slice(0, 16);
}

/** Where fates and rules are also kept as memory. */
export interface LessonMemory {
	retain(repo: string, fates: readonly Fate[], lessons: readonly Lesson[]): Promise<void>;
}

export interface HindsightLessonsOptions {
	readonly baseUrl: string;
	readonly fetch?: typeof globalThis.fetch;
	readonly now?: () => number;
}

/** Fates and rules retained in Hindsight, tagged per repository, as experiences and observations of the bank. */
export class HindsightLessons implements LessonMemory {
	readonly #backend: MemoryBackend;
	readonly #now: () => number;

	constructor(options: HindsightLessonsOptions) {
		this.#now = options.now ?? Date.now;
		this.#backend = createHindsightBackend({
			baseUrl: options.baseUrl,
			bankId: BANK,
			ensureBank: true,
			...(options.fetch === undefined ? {} : { fetch: options.fetch }),
			scopeTags: { project: ["ultron:autoreview:project:bank"] },
		});
	}

	async retain(repo: string, fates: readonly Fate[], lessons: readonly Lesson[]): Promise<void> {
		const retain = this.#backend.retain;
		if (retain === undefined) return;
		const tag = `ultron:autoreview:project:${repoIdentity(repo)}`;
		const items: Array<{ documentId: string; content: string }> = fates.map((fate) => ({
			documentId: `autoreview:${repoIdentity(repo)}:${fate.pull}:${fate.id}`,
			content:
				`In ${repo}, a ${fate.level} ${fate.category} finding of the ${fate.kind} pass at ${fate.file}:${fate.line} ` +
				`was ${fate.fate} (${fate.how}): ${fate.claim}`,
		}));
		for (const lesson of lessons) {
			const total = lesson.accepted + lesson.rejected;
			items.push({
				documentId: `autoreview:${repoIdentity(repo)}:lesson:${lesson.category}:${lesson.kind}`,
				content:
					`In ${repo}, ${lesson.category} findings of the ${lesson.kind} pass were accepted ${lesson.accepted} of ${total} ` +
					`times in the last ${FATE_DAYS} days` +
					(lesson.lowered ? "; the reviewer now posts such findings as low, non-blocking notes." : "."),
			});
		}
		for (const item of items) {
			const operation = createHash("sha256").update(`${item.documentId}:${this.#now()}`).digest("hex").slice(0, 32);
			await retain({
				async: true,
				operation_id: operation,
				items: [
					{
						content: item.content,
						document_id: item.documentId,
						tags: [tag],
						observation_scopes: [[tag]],
						update_mode: "replace",
						metadata: { ultron_operation: operation, ultron_evidence_class: "tool_evidence" },
					},
				],
			});
		}
	}
}
