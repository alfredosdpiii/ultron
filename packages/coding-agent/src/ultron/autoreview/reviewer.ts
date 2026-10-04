/**
 * One pull request, one account: decide whether a review is due, acknowledge, check the source out, run the
 * engine, and post the review (or write it to the dry-run directory). Everything read from the pull request
 * (description, comments, the mention itself, repository files) is data for the review, never instructions.
 */
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { type Account, accountKey, type TokenStore } from "./accounts.ts";
import { type Checkout, type CheckoutManager, StaleHeadError } from "./checkout.ts";
import { type AutoreviewConfig, type AutoreviewPaths, MAX_ATTEMPTS } from "./config.ts";
import {
	type Comment,
	GitHub,
	GitHubError,
	type PullRef,
	type PullRequest,
	pullKey,
	RateLimitError,
	type Review,
} from "./github.ts";
import { planReview, type ReviewPlan, withoutInline } from "./plan.ts";
import type { Runner } from "./runner.ts";
import { claimHash, type PostedFinding, type PullState, type StateStore } from "./state.ts";
import type { ContextComment, EngineResult, EngineSpec, ReviewEngine } from "./types.ts";

export interface ReviewerDeps {
	readonly runner: Runner;
	readonly tokens: TokenStore;
	readonly engine: ReviewEngine;
	readonly store: StateStore;
	readonly checkouts: CheckoutManager;
	readonly config: AutoreviewConfig;
	readonly paths: AutoreviewPaths;
	readonly log: (line: string) => void;
	readonly now?: () => number;
	/** A number in [0, 1): picks the acknowledgement line. */
	readonly random?: () => number;
}

export interface Candidate {
	readonly account: Account;
	readonly ref: PullRef;
	/** Why discovery picked it: notification reasons and search names. */
	readonly reasons: readonly string[];
	/** When discovery found it (epoch ms). */
	readonly pickedAt: number;
}

export type Outcome =
	| { readonly kind: "skipped"; readonly reason: string }
	| { readonly kind: "requeue"; readonly reason: string }
	| { readonly kind: "gave-up"; readonly reason: string }
	| {
			readonly kind: "posted" | "dry-run";
			readonly verdict: ReviewPlan["verdict"];
			readonly sha: string;
			readonly result: EngineResult;
			readonly plan: ReviewPlan;
			readonly reviewId?: number;
			readonly path?: string;
	  };

export interface ReviewOptions {
	/** Review even when nothing new asks for it (`ultron autoreview review`). */
	readonly force?: boolean;
	readonly dryRun?: boolean;
}

/** State key of an account's view of a pull request. */
export function pullStateKey(account: Account, ref: PullRef): string {
	return `${account.login}@${pullKey(ref)}`;
}

function escapeRegExp(text: string): string {
	return text.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

/** Whether `body` @mentions `login` (not as part of a longer name or an email address). */
export function mentions(body: string, login: string): boolean {
	return new RegExp(`(^|[^A-Za-z0-9_/-])@${escapeRegExp(login)}(?![A-Za-z0-9-]|/)`, "i").test(body);
}

/** When the account was last @mentioned on the pull request by somebody else (ISO time), if ever. */
export function latestMention(
	login: string,
	pull: PullRequest,
	comments: readonly Comment[],
	reviews: readonly Review[],
): string | undefined {
	const me = login.toLowerCase();
	let latest: string | undefined;
	const consider = (author: string, body: string, at: string) => {
		if (author.toLowerCase() === me || !at || !mentions(body, login)) return;
		if (latest === undefined || at > latest) latest = at;
	};
	consider(pull.author, pull.body, pull.createdAt);
	for (const comment of comments) consider(comment.user, comment.body, comment.updatedAt);
	for (const review of reviews) consider(review.user, review.body, review.submittedAt);
	return latest;
}

export interface DecisionInput {
	readonly pull: PullRequest;
	/** The commit of this account's last review, and when it was made. */
	readonly lastSha?: string;
	readonly lastAt?: string;
	readonly mentionAt?: string;
	/** A review by this account is currently requested (directly, or through a team it was notified for). */
	readonly requested: boolean;
	/** When the review was last requested; asked only when the head was already reviewed. */
	readonly requestedAt: () => Promise<string | undefined>;
}

export type Decision = { readonly review: boolean; readonly reason: string };

/** Whether a review is due. */
export async function decide(input: DecisionInput): Promise<Decision> {
	const { pull, lastSha, lastAt, mentionAt } = input;
	const newMention = mentionAt !== undefined && (lastAt === undefined || mentionAt > lastAt);
	if (pull.state !== "open")
		return newMention
			? { review: true, reason: `mentioned on a ${pull.merged ? "merged" : "closed"} pull request` }
			: { review: false, reason: `the pull request is ${pull.merged ? "merged" : "closed"}` };
	if (lastSha === undefined) {
		if (input.requested) return { review: true, reason: "review requested" };
		if (newMention) return { review: true, reason: "mentioned" };
		return { review: false, reason: "no review request or mention for this account" };
	}
	if (lastSha !== pull.headSha) return { review: true, reason: `new commits since ${lastSha.slice(0, 7)}` };
	if (newMention) return { review: true, reason: "mentioned again" };
	if (input.requested) {
		const requestedAt = await input.requestedAt();
		if (requestedAt !== undefined && lastAt !== undefined && requestedAt > lastAt)
			return { review: true, reason: "review requested again" };
	}
	return { review: false, reason: `${pull.headSha.slice(0, 7)} is already reviewed and nothing new asks for it` };
}

/** GitHub rejects a comment body over 65,536 characters; the acknowledgement stays well under it. */
export const MAX_ACK_CHARS = 60_000;

/**
 * The acknowledgement posted when a review starts: the line as a quotation with its attribution, the commit, and
 * (with `autoreview.ackArt`) the art in a fenced block. The fence is longer than any backtick run in the art, so
 * the art cannot close it; art that would take the comment past the size limit is left out.
 */
export function ackBody(line: string, sha: string, art = ""): string {
	const quote = line.replace(/\s+/g, " ").trim();
	const body = `> *${quote}*\n> — Ultron\n\nReviewing \`${sha.slice(0, 7)}\`.`;
	const banner = art.replace(/\r\n/g, "\n").replace(/^\n+|\s+$/g, "");
	if (banner === "") return body;
	const longest = Math.max(2, ...[...banner.matchAll(/`+/g)].map((match) => match[0].length));
	const fence = "`".repeat(longest + 1);
	const full = `${body}\n\n${fence}text\n${banner}\n${fence}`;
	return full.length <= MAX_ACK_CHARS ? full : body;
}

/** A line from `lines`, at random, other than the one used last on this pull request. */
export function pickAckLine(lines: readonly string[], previous: string | undefined, random: () => number): string {
	const choices = lines.length > 1 ? lines.filter((line) => line !== previous) : lines;
	return choices[Math.min(choices.length - 1, Math.floor(random() * choices.length))] ?? "";
}

function latestOwnReview(reviews: readonly Review[], login: string): Review | undefined {
	const me = login.toLowerCase();
	let latest: Review | undefined;
	for (const review of reviews) {
		if (review.user.toLowerCase() !== me || review.state === "PENDING" || !review.submittedAt) continue;
		if (latest === undefined || review.submittedAt > latest.submittedAt) latest = review;
	}
	return latest;
}

function dryRunMarkdown(candidate: Candidate, sha: string, plan: ReviewPlan, ack: string | undefined): string {
	const lines = [
		`# Would-be review of ${pullKey(candidate.ref)} at ${sha.slice(0, 7)} as ${candidate.account.login}`,
		"",
		`Event: ${plan.event}`,
		"",
	];
	if (ack !== undefined) lines.push("## Acknowledgement comment", "", ack, "");
	lines.push("## Summary", "", plan.body, "");
	if (plan.comments.length) lines.push("## Inline comments", "");
	for (const comment of plan.comments) {
		const where =
			comment.start_line === undefined
				? `${comment.path}:${comment.line}`
				: `${comment.path}:${comment.start_line}-${comment.line}`;
		lines.push(`### ${where}`, "", comment.body, "");
	}
	return `${lines.join("\n")}\n`;
}

/** Review one pull request as one account, if a review is due. */
export async function reviewPull(
	deps: ReviewerDeps,
	candidate: Candidate,
	options: ReviewOptions = {},
): Promise<Outcome> {
	const { account, ref } = candidate;
	const now = deps.now ?? Date.now;
	const dryRun = options.dryRun ?? deps.config.dryRun;
	const env = () => deps.tokens.env(account);
	const github = new GitHub(deps.runner, account, env, { now });
	const key = pullStateKey(account, ref);
	const name = `${pullKey(ref)} as ${account.login}`;

	const pull = await github.pull(ref);
	if (!pull.headSha) throw new GitHubError(`${name}: the pull request has no head commit`, 0);
	const head = pull.headSha;
	const reviews = await github.reviews(ref);
	const saved: PullState = deps.store.read().pulls[key] ?? { attempts: {}, findings: [] };
	// A dry-run "review" counts only while still in dry-run: switching to posting reviews the commit for real.
	const savedCounts = saved.lastReviewedSha !== undefined && (dryRun || saved.lastReviewDryRun !== true);
	const own = latestOwnReview(reviews, account.login);
	let lastSha = own?.commitId;
	let lastAt = own?.submittedAt;
	if (savedCounts && (lastAt === undefined || (saved.lastReviewedAt ?? "") > lastAt)) {
		lastSha = saved.lastReviewedSha;
		lastAt = saved.lastReviewedAt;
	}

	const mentionReason = candidate.reasons.some((reason) => reason.includes("mention"));
	let issueComments: Comment[] | undefined;
	let reviewComments: Comment[] | undefined;
	const loadComments = async () => {
		issueComments ??= await github.issueComments(ref);
		reviewComments ??= await github.reviewComments(ref);
	};
	let mentionAt: string | undefined;
	if (mentionReason || pull.state !== "open" || (lastSha === undefined && !pull.requestedReviewers.length)) {
		await loadComments();
		mentionAt = latestMention(account.login, pull, [...issueComments!, ...reviewComments!], reviews);
	}
	const me = account.login.toLowerCase();
	const requested =
		pull.requestedReviewers.some((login) => login.toLowerCase() === me) ||
		(pull.requestedTeams.length > 0 && candidate.reasons.includes("review_requested"));
	const decision = options.force
		? { review: true, reason: "asked for on the command line" }
		: await decide({
				pull,
				...(lastSha === undefined ? {} : { lastSha }),
				...(lastAt === undefined ? {} : { lastAt }),
				...(mentionAt === undefined ? {} : { mentionAt }),
				requested,
				requestedAt: () => github.lastReviewRequestAt(ref),
			});
	if (!decision.review) return { kind: "skipped", reason: decision.reason };

	if (!options.force && saved.gaveUpSha === head) return { kind: "skipped", reason: "gave up on this commit earlier" };
	if (!options.force && (saved.attempts[head] ?? 0) >= MAX_ATTEMPTS) {
		if (!dryRun)
			await github.postComment(
				ref,
				`I could not review \`${head.slice(0, 7)}\` after ${MAX_ATTEMPTS} attempts. Push a new commit or mention me again to retry.`,
			);
		await deps.store.updatePull(key, (state) => {
			state.gaveUpSha = head;
		});
		deps.log(`${name}: gave up on ${head.slice(0, 7)} after ${MAX_ATTEMPTS} attempts`);
		return { kind: "gave-up", reason: `${MAX_ATTEMPTS} attempts failed` };
	}
	deps.log(`${name}: reviewing ${head.slice(0, 7)} (${decision.reason})`);

	// Acknowledge once per head commit, before the work starts.
	let ack: string | undefined;
	if (deps.config.ack && deps.config.ackLines.length > 0 && saved.lastAckSha !== head) {
		const line = pickAckLine(deps.config.ackLines, saved.lastAckLine, deps.random ?? Math.random);
		ack = ackBody(line, head, deps.config.ackArt);
		if (!dryRun) {
			try {
				await github.postComment(ref, ack);
				await deps.store.updatePull(key, (state) => {
					state.lastAckSha = head;
					state.lastAckLine = line;
				});
			} catch (error) {
				if (error instanceof RateLimitError) throw error;
				deps.log(`${name}: the acknowledgement was not posted: ${(error as Error).message}`);
			}
		}
	}
	await deps.store.updatePull(key, (state) => {
		state.attempts[head] = (state.attempts[head] ?? 0) + 1;
	});
	const forgetAttempt = () =>
		deps.store.updatePull(key, (state) => {
			const count = (state.attempts[head] ?? 1) - 1;
			if (count <= 0) delete state.attempts[head];
			else state.attempts[head] = count;
		});

	const scratch = mkdtempSync(join(tmpdir(), "ultron-autoreview-diff-"));
	let checkout: Checkout | undefined;
	try {
		// Source for the verifier; without it the review reads the diff alone and cannot approve.
		try {
			checkout = await deps.checkouts.prepare(ref, pull, await env());
		} catch (error) {
			if (error instanceof StaleHeadError) {
				await forgetAttempt();
				return { kind: "requeue", reason: error.message };
			}
			deps.log(`${name}: no checkout, reviewing the diff only: ${(error as Error).message}`);
		}
		await loadComments();
		const others: ContextComment[] = reviewComments!
			.filter((comment) => comment.user.toLowerCase() !== me && comment.path !== undefined)
			.map((comment) => ({
				author: comment.user,
				body: comment.body,
				...(comment.path === undefined ? {} : { path: comment.path }),
				...(comment.line === undefined ? {} : { line: comment.line }),
			}));
		const ci = await github.checkSummary(ref, head);

		const earlier = saved.findings.filter((finding) => finding.status === "open");
		let sinceSha: string | undefined;
		let fullDiff: string;
		let reviewDiff: string;
		let earlierDiff: string | undefined;
		if (checkout) {
			fullDiff = await checkout.diff(checkout.mergeBase, head);
			reviewDiff = fullDiff;
			if (lastSha !== undefined && lastSha !== head && (await checkout.isAncestor(lastSha, head))) {
				// The head moved forward: review what is new. After a force-push the whole diff is reviewed again.
				sinceSha = lastSha;
				reviewDiff = await checkout.diff(lastSha, head);
				earlierDiff = reviewDiff;
			} else if (lastSha === head) earlierDiff = "";
		} else {
			fullDiff = await github.pullDiff(ref);
			reviewDiff = fullDiff;
			if (lastSha === head) earlierDiff = "";
		}
		const diffPath = join(scratch, "review.diff");
		const postDiffPath = join(scratch, "pull.diff");
		const earlierDiffPath = join(scratch, "earlier.diff");
		writeFileSync(diffPath, reviewDiff);
		writeFileSync(postDiffPath, fullDiff);
		if (earlierDiff !== undefined) writeFileSync(earlierDiffPath, earlierDiff);
		const spec: EngineSpec = {
			...(checkout ? { workDir: checkout.workDir } : {}),
			diffPath,
			postDiffPath,
			label: `${ref.owner}/${ref.repo}#${ref.number}`,
			...(deps.config.model === undefined ? {} : { model: deps.config.model }),
			...(deps.config.verifyModel === undefined ? {} : { verifyModel: deps.config.verifyModel }),
			budget: deps.config.budget,
			context: {
				title: pull.title,
				description: pull.body,
				...(ci === undefined ? {} : { ci }),
				comments: others,
			},
			...(earlier.length === 0
				? {}
				: {
						earlier: earlier.map((finding) => ({
							id: finding.id,
							file: finding.file,
							line: finding.line,
							severity: finding.severity,
							claim: finding.claim,
						})),
						...(earlierDiff === undefined ? {} : { earlierDiffPath }),
					}),
		};
		const result = await deps.engine.review(spec);

		// A review of a commit that is no longer the head would be stale: drop it and come back.
		const current = await github.pull(ref);
		if (current.headSha !== head) {
			await forgetAttempt();
			deps.log(`${name}: the head moved to ${current.headSha.slice(0, 7)} during the review; discarded`);
			return { kind: "requeue", reason: "the head commit moved during the review" };
		}

		const state = current.state === "open" ? "open" : current.merged ? "merged" : "closed";
		let plan = planReview(result, {
			selfAuthored: pull.author.toLowerCase() === me,
			state,
			headSha: head,
			...(sinceSha === undefined ? {} : { sinceSha }),
			signature: deps.config.signature,
		});
		const reviewedAt = new Date(now()).toISOString();
		const shortSha = head.slice(0, 7);
		const posted: PostedFinding[] = [];
		const track = (index: number): PostedFinding => {
			const finding = result.findings[index]!;
			const entry: PostedFinding = {
				id: `${shortSha}-${index + 1}`,
				file: finding.file,
				line: finding.line,
				severity: finding.severity,
				claim: finding.claim,
				claimHash: claimHash(finding.claim),
				sha: head,
				status: "open",
			};
			posted.push(entry);
			return entry;
		};
		let reviewId: number | undefined;
		let path: string | undefined;
		const resolved: string[] = [];
		if (dryRun) {
			mkdirSync(deps.paths.dryRun, { recursive: true, mode: 0o700 });
			path = join(deps.paths.dryRun, `${ref.owner}-${ref.repo}-${ref.number}-${shortSha}-${account.login}`);
			writeFileSync(
				`${path}.json`,
				`${JSON.stringify({ account: accountKey(account), pull: pullKey(ref), sha: head, ack, review: { commit_id: head, event: plan.event, body: plan.body, comments: plan.comments.map(({ path: file, line, side, start_line, start_side, body }) => ({ path: file, line, side, ...(start_line === undefined ? {} : { start_line, start_side }), body })) }, result }, null, 1)}\n`,
				{ mode: 0o600 },
			);
			writeFileSync(`${path}.md`, dryRunMarkdown(candidate, head, plan, ack), { mode: 0o600 });
			for (const comment of plan.comments) track(comment.finding);
			for (const index of plan.inSummary) track(index);
		} else {
			const input = (from: ReviewPlan) => ({
				commit_id: head,
				event: from.event,
				body: from.body,
				comments: from.comments.map(({ path: file, line, side, start_line, start_side, body }) => ({
					path: file,
					line,
					side,
					...(start_line === undefined ? {} : { start_line, start_side }),
					body,
				})),
			});
			try {
				reviewId = await github.postReview(ref, input(plan));
			} catch (error) {
				// GitHub rejects the whole review when one inline comment cannot be placed: post it without them.
				if (!(error instanceof GitHubError) || error.status !== 422 || plan.comments.length === 0) throw error;
				deps.log(`${name}: GitHub rejected the inline comments (${error.message}); posting them in the summary`);
				plan = withoutInline(plan, result);
				reviewId = await github.postReview(ref, input(plan));
			}
			const entries = new Map<string, PostedFinding>();
			for (const comment of plan.comments) entries.set(comment.marker, track(comment.finding));
			for (const index of plan.inSummary) track(index);
			// Read the review back: an inline comment GitHub dropped is posted again on its own.
			if (plan.comments.length > 0) {
				try {
					const kept = await github.commentsOfReview(ref, reviewId);
					for (const comment of plan.comments) {
						const entry = entries.get(comment.marker)!;
						const found = kept.find((item) => item.body.includes(comment.marker));
						if (found) {
							entry.commentId = found.id;
							continue;
						}
						try {
							const { path: file, line, side, start_line, start_side, body } = comment;
							entry.commentId = await github.postReviewComment(ref, head, {
								path: file,
								line,
								side,
								...(start_line === undefined || start_side === undefined ? {} : { start_line, start_side }),
								body,
							});
							deps.log(`${name}: re-posted a dropped inline comment on ${file}:${line}`);
						} catch (error) {
							if (error instanceof RateLimitError) throw error;
							deps.log(
								`${name}: an inline comment on ${comment.path}:${comment.line} could not be posted: ${(error as Error).message}`,
							);
						}
					}
					const threads = await github.reviewThreads(ref);
					for (const entry of entries.values()) {
						const thread = entry.commentId === undefined ? undefined : threads.get(entry.commentId);
						if (thread) entry.threadId = thread.id;
					}
				} catch (error) {
					deps.log(`${name}: the posted review could not be read back: ${(error as Error).message}`);
				}
			}
			// Resolve our own threads whose finding is fixed, by the thread id stored when the comment was posted.
			for (const status of result.earlier) {
				if (status.status !== "fixed") continue;
				const finding = saved.findings.find((item) => item.id === status.id);
				if (finding?.threadId === undefined) continue;
				try {
					await github.resolveThread(finding.threadId);
					resolved.push(finding.id);
				} catch (error) {
					deps.log(`${name}: thread of ${finding.id} was not resolved: ${(error as Error).message}`);
				}
			}
		}

		await deps.store.updatePull(key, (pullState, all) => {
			pullState.lastReviewedSha = head;
			pullState.lastReviewedAt = reviewedAt;
			if (dryRun) pullState.lastReviewDryRun = true;
			else delete pullState.lastReviewDryRun;
			if (reviewId !== undefined) pullState.lastReviewId = reviewId;
			delete pullState.attempts[head];
			for (const status of result.earlier) {
				const finding = pullState.findings.find((item) => item.id === status.id);
				if (!finding) continue;
				if (status.status === "fixed" || status.status === "not_applicable") finding.status = status.status;
				else finding.line = status.line;
			}
			pullState.findings.push(...posted);
			if (pullState.findings.length > 200) pullState.findings = pullState.findings.slice(-200);
			all.recent.push({
				account: accountKey(account),
				pull: pullKey(ref),
				sha: head,
				at: reviewedAt,
				outcome: dryRun ? "dry-run" : "posted",
				verdict: plan.verdict,
				findings: result.findings.length,
				totalMs: result.timing.totalMs,
				pickupToPostMs: now() - candidate.pickedAt,
				costUsd: result.usage.costUsd,
				...(dryRun ? { dryRun: true } : {}),
			});
		});
		deps.log(
			`${name}: ${dryRun ? "dry-run" : "posted"} ${plan.event} for ${shortSha}: ${result.findings.length} findings, ${plan.comments.length} inline, ${resolved.length} threads resolved, ${Math.round((now() - candidate.pickedAt) / 1000)} s from pickup`,
		);
		return {
			kind: dryRun ? "dry-run" : "posted",
			verdict: plan.verdict,
			sha: head,
			result,
			plan,
			...(reviewId === undefined ? {} : { reviewId }),
			...(path === undefined ? {} : { path }),
		};
	} catch (error) {
		// Waiting for a rate limit is not a failed attempt.
		if (error instanceof RateLimitError) await forgetAttempt().catch(() => {});
		throw error;
	} finally {
		rmSync(scratch, { recursive: true, force: true });
		await checkout?.cleanup().catch(() => {});
	}
}
