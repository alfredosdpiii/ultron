/**
 * What `ultron autoreview` takes from the user's own machine for a review: an existing local checkout of the
 * reviewed repository (only its prepared environment directories are ever used, read-only, for sandboxed test
 * runs) and private review guides (given to the review's frames, never to appear in what is posted).
 */
import { existsSync } from "node:fs";
import { homedir } from "node:os";
import { basename, isAbsolute, join, resolve } from "node:path";
import type { PullRef } from "./github.ts";
import type { PlannedComment, ReviewPlan } from "./plan.ts";
import type { Runner } from "./runner.ts";

/** `~` and relative paths made absolute. */
export function expandPath(path: string, home: string = homedir()): string {
	const text = path.trim();
	if (text === "~") return home;
	if (text.startsWith("~/")) return join(home, text.slice(2));
	return isAbsolute(text) ? text : resolve(text);
}

/** Whether a git remote URL names `owner/repo` on `host` (https, ssh or scp-like syntax; `.git` optional). */
export function remoteMatches(url: string, ref: Pick<PullRef, "host" | "owner" | "repo">): boolean {
	const match =
		/^(?:https?:\/\/|ssh:\/\/|git:\/\/)(?:[^@/]+@)?([^/:]+)(?::\d+)?\/(.+?)(?:\.git)?\/?$/.exec(url.trim()) ??
		/^(?:[^@/]+@)?([^/:]+):(.+?)(?:\.git)?\/?$/.exec(url.trim());
	if (!match) return false;
	return (
		match[1]!.toLowerCase() === ref.host.toLowerCase() &&
		match[2]!.toLowerCase() === `${ref.owner}/${ref.repo}`.toLowerCase()
	);
}

/**
 * An existing local checkout of the repository under one of `roots` (`<root>/<repo>`), accepted only when one of
 * its git remotes is that repository. Undefined when there is none.
 */
export async function findCheckout(
	runner: Runner,
	roots: readonly string[],
	ref: Pick<PullRef, "host" | "owner" | "repo">,
): Promise<string | undefined> {
	if (!/^[A-Za-z0-9._-]+$/.test(ref.repo) || ref.repo === "." || ref.repo === "..") return undefined;
	for (const root of roots) {
		const dir = join(expandPath(root), ref.repo);
		if (!existsSync(join(dir, ".git"))) continue;
		const result = await runner(["git", "-C", dir, "config", "--get-regexp", "^remote\\..*\\.url$"], {
			env: { GIT_OPTIONAL_LOCKS: "0" },
			timeoutMs: 20_000,
		});
		if (result.code !== 0) continue;
		const urls = result.stdout
			.split("\n")
			.map((line) => line.trim().split(/\s+/)[1])
			.filter((url): url is string => url !== undefined);
		if (urls.some((url) => remoteMatches(url, ref))) return dir;
	}
	return undefined;
}

function escapeRegExp(text: string): string {
	return text.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

/** The guide names (file names and paths) that appear in `text`. */
export function guideMentions(text: string, guideNames: readonly string[]): string[] {
	const lower = text.toLowerCase();
	return guideNames.filter((name) => {
		const needle = name.toLowerCase();
		if (needle.length < 4) return false;
		// A file name must stand alone; a path is matched anywhere.
		return needle.includes("/")
			? lower.includes(needle)
			: new RegExp(`(^|[^a-z0-9_.-])${escapeRegExp(needle)}($|[^a-z0-9_-])`).test(lower);
	});
}

/**
 * The plan with nothing that names a private review guide: an inline comment that mentions one is not posted, and
 * a sentence of the body that does is removed. Returns the plan and what was withheld.
 */
export function withoutGuideMentions(
	plan: ReviewPlan,
	guideNames: readonly string[],
): { plan: ReviewPlan; withheld: string[] } {
	if (guideNames.length === 0) return { plan, withheld: [] };
	const withheld: string[] = [];
	const comments: PlannedComment[] = [];
	const dropped: number[] = [];
	for (const comment of plan.comments) {
		const found = guideMentions(comment.body, guideNames);
		if (found.length === 0) comments.push(comment);
		else {
			dropped.push(comment.finding);
			withheld.push(
				`an inline comment on ${comment.path}:${comment.line} (${found.map((name) => basename(name)).join(", ")})`,
			);
		}
	}
	let body = plan.body;
	if (guideMentions(body, guideNames).length > 0) {
		body = body
			.split("\n")
			.map((line) =>
				guideMentions(line, guideNames).length === 0
					? line
					: line
							.split(/(?<=[.!?])\s+/)
							.filter((part) => guideMentions(part, guideNames).length === 0)
							.join(" "),
			)
			.join("\n");
		withheld.push("part of the review body");
	}
	return {
		plan: { ...plan, body, comments, overCap: [...plan.overCap, ...dropped] },
		withheld,
	};
}
