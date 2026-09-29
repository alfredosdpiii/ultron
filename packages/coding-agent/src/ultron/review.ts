/**
 * `/review`: an optional, user-invoked code review. The review itself is Python (`rlm/review_api.py`, with the
 * reviewer and verifier instructions in `rlm/review_prompts.py`): it scopes the diff with git or `gh`, runs the
 * finder and verifier frames through `rlm.map` under one token cap, and renders the report. This module only turns
 * the typed command into a short prompt that asks the model to run that one cell and relay the report, so nothing
 * about `/review` is in the system prompt of ordinary turns.
 *
 * The command is expanded where every client's prompt enters the worker (the AgentController), so the native
 * TUI, `ultron -p "/review"` and RPC prompts all behave the same.
 */

export const REVIEW_COMMAND = {
	name: "review",
	description: "Review code changes: specialist reviewer frames, then a verifier per finding",
	argumentHint: "[base-ref|PR|path...] [--only bugs,security,arch,tests,ai] [--budget 300k] [--model p/m] [--post]",
} as const;

const REVIEW_INVOCATION = /^\/review(?:\s+([\s\S]*))?$/;

/** The model-facing prompt for `/review <args>`. */
export function reviewPrompt(args: string): string {
	const trimmed = args.trim();
	const lines = [
		`/review${trimmed ? ` ${trimmed}` : ""}`,
		"",
		"Run Ultron's built-in code review in one REPL cell, exactly:",
		"",
		"import review_api",
		// A JSON string literal is a valid Python string literal for any argument text.
		`review = await review_api.run(rlm, ${JSON.stringify(trimmed)})`,
		"print(review.report)",
		"",
		"Then reply with the printed report as it is. Do not review the code yourself, edit files, or run the review again unless the cell raised an error.",
	];
	if (/(^|\s)--post(\s|=|$)/.test(trimmed)) {
		lines.push(
			"",
			"If the report says a post is pending, ask me whether to post it to the pull request. Post only after I answer yes in a later message, with `await review_api.post(review, confirm=True)`; without that answer, never post.",
		);
	}
	return lines.join("\n");
}

/** Expand `/review [args]` into the review prompt; any other text is returned unchanged. */
export function expandReviewCommand(text: string): string {
	const match = REVIEW_INVOCATION.exec(text.trim());
	return match ? reviewPrompt(match[1] ?? "") : text;
}
