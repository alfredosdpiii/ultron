/**
 * The session report as text: the full report of one session (`/usage`, `ultron usage`) and the one-line-per-session
 * table (`ultron usage --last N`). A number the session did not keep is printed as `not recorded` (`n/r` in the
 * table), and a cost nobody reported as `unknown`: neither is ever printed as zero.
 */
import { homedir } from "node:os";
import type { ReportCells, ReportCost, SessionReport, UsageBucket } from "./session-report.ts";

export interface ReportStyle {
	bold(text: string): string;
	dim(text: string): string;
}

export const PLAIN_REPORT_STYLE: ReportStyle = { bold: (text) => text, dim: (text) => text };

export interface ReportTextOptions {
	style?: ReportStyle;
	/** Print times in UTC instead of local time. */
	utc?: boolean;
	/** The home directory shortened to `~` in paths; default the user's. */
	home?: string;
}

const NOT_RECORDED = "not recorded";

export function formatTokens(tokens: number): string {
	if (tokens < 1000) return String(tokens);
	if (tokens < 999_950) return `${(tokens / 1000).toFixed(1)}k`;
	return `${(tokens / 1_000_000).toFixed(tokens < 99_950_000 ? 2 : 1)}M`;
}

export function formatElapsed(ms: number): string {
	const seconds = Math.round(ms / 1000);
	if (seconds < 60) return `${seconds}s`;
	const minutes = Math.floor(seconds / 60);
	if (minutes < 60) return `${minutes}m${String(seconds % 60).padStart(2, "0")}s`;
	return `${Math.floor(minutes / 60)}h${String(minutes % 60).padStart(2, "0")}m`;
}

function usd(amount: number): string {
	return `$${amount.toFixed(amount >= 10 ? 2 : 3)}`;
}

/** `$1.234`, `$3.287 (sub)`, `unknown`, or a sum of those; `-` when nothing was spent. */
export function formatCost(cost: ReportCost, tokens: number): string {
	const parts: string[] = [];
	if (cost.reportedUsd !== null) parts.push(usd(cost.reportedUsd));
	if (cost.subscriptionUsd !== null)
		parts.push(cost.subscriptionUsd > 0 ? `${usd(cost.subscriptionUsd)} (sub)` : "subscription");
	if (cost.unpricedResponses > 0)
		parts.push(parts.length === 0 ? "unknown" : `unknown (${cost.unpricedResponses} unpriced)`);
	if (parts.length === 0) return tokens > 0 ? "unknown" : "-";
	return parts.join(" + ");
}

function formatDate(at: number, utc: boolean): string {
	const date = new Date(at);
	const two = (value: number) => String(value).padStart(2, "0");
	return utc
		? `${date.getUTCFullYear()}-${two(date.getUTCMonth() + 1)}-${two(date.getUTCDate())} ${two(date.getUTCHours())}:${two(date.getUTCMinutes())}`
		: `${date.getFullYear()}-${two(date.getMonth() + 1)}-${two(date.getDate())} ${two(date.getHours())}:${two(date.getMinutes())}`;
}

function shortPath(path: string, home: string): string {
	return home && (path === home || path.startsWith(`${home}/`)) ? `~${path.slice(home.length)}` : path;
}

function formatBytes(bytes: number): string {
	if (bytes < 1024) return `${bytes} B`;
	if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} kB`;
	return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

function plural(count: number, one: string, many = `${one}s`): string {
	return `${count} ${count === 1 ? one : many}`;
}

function nonZero(pairs: Array<[string, number]>): string {
	return pairs
		.filter(([, count]) => count > 0)
		.map(([label, count]) => `${count} ${label}`)
		.join(", ");
}

/** `bash 12 · read 3 · edit 2 · rlm.map 1`: how many cells name each API, in the report's order. */
function apisLine(apis: Record<string, number>): string {
	return Object.entries(apis)
		.filter(([, count]) => count > 0)
		.map(([name, count]) => `${name} ${count}`)
		.join(" · ");
}

function cellsLine(cells: ReportCells): string {
	return `${plural(cells.count, "cell")}${cells.count > 0 ? `, ${cells.failed} failed` : ""}`;
}

function tokensText(bucket: UsageBucket): string {
	if (bucket.responses === 0 && bucket.unmeasured > 0) return NOT_RECORDED;
	return `${formatTokens(bucket.totalTokens)}${bucket.unmeasured > 0 ? " +?" : ""}`;
}

function table(rows: string[][], rightAligned: ReadonlySet<number>): string[] {
	const widths: number[] = [];
	for (const row of rows)
		for (const [index, cell] of row.entries()) widths[index] = Math.max(widths[index] ?? 0, cell.length);
	return rows.map((row) =>
		row
			.map((cell, index) => (rightAligned.has(index) ? cell.padStart(widths[index]!) : cell.padEnd(widths[index]!)))
			.join("  ")
			.trimEnd(),
	);
}

/** The full report of one session, as lines. */
export function renderSessionReport(report: SessionReport, options: ReportTextOptions = {}): string[] {
	const style = options.style ?? PLAIN_REPORT_STYLE;
	const utc = options.utc === true;
	const home = options.home ?? homedir();
	const lines: string[] = [];
	const label = (text: string) => style.dim(text.padEnd(11));
	const row = (name: string, text: string) => lines.push(`${label(name)}${text}`);
	const more = (text: string) => lines.push(`${" ".repeat(11)}${text}`);
	const missing = (path: string) => `${NOT_RECORDED}${report.unrecorded[path] ? ` (${report.unrecorded[path]})` : ""}`;

	const { session, depth } = report;
	lines.push(
		`${style.bold(`Session ${session.id}`)}${session.name ? `  ${session.name}` : ""}  ${style.dim(`[${report.mode}]`)}`,
	);
	lines.push(
		style.dim(
			`${shortPath(session.cwd, home)}  ·  ${formatDate(session.createdAt, utc)} to ${formatDate(session.modifiedAt, utc)}${utc ? " UTC" : ""}  ·  ${formatBytes(session.bytes)}`,
		),
	);
	lines.push("");
	row("Depth", style.bold(depth.verdict));
	// Counters that began after an `ultron claude` session did hold only part of its turns and cells.
	const partial =
		report.mode === "ultron claude" && report.guardrails.countersSince !== null
			? style.dim(` (counted since ${formatDate(report.guardrails.countersSince, utc)})`)
			: "";
	if (report.turns === null) row("Turns", missing("turns"));
	else {
		const { turns } = report;
		const ended = nonZero([
			["aborted", turns.aborted],
			["failed", turns.failed],
			["running", turns.running],
		]);
		row(
			"Turns",
			`${plural(turns.count, "turn")}, ${formatElapsed(turns.wallMs)} wall${ended ? ` (${ended})` : ""}${partial}`,
		);
	}
	row(
		"Root",
		report.root.models.length > 0
			? report.root.models.map((model) => `${model.model} (${plural(model.responses, "response")})`).join(", ")
			: report.unrecorded["root.models"]
				? missing("root.models")
				: "no model response yet",
	);
	if (report.cells === null) row("Cells", missing("cells"));
	else {
		const { cells } = report;
		row(
			"Cells",
			`${cellsLine(cells.total)}${cells.source === "counters" ? style.dim("  (runtime count)") : ""}${partial}`,
		);
		const apis = apisLine(cells.total.apis);
		if (apis) more(apis);
		if (cells.subagents.count > 0 || cells.other.count > 0)
			more(
				style.dim(
					`root ${cells.root.count} · sub-agents ${cells.subagents.count}${cells.other.count > 0 ? ` · other lanes ${cells.other.count}` : ""}`,
				),
			);
		const tools = Object.entries(cells.otherTools);
		if (tools.length > 0)
			more(
				`other tools: ${tools.map(([name, tool]) => `${name} ${tool.calls}${tool.failed > 0 ? ` (${tool.failed} failed)` : ""}`).join(" · ")}`,
			);
	}

	// Depth.
	const { frames, subagents } = depth;
	if (frames.count === 0) row("Frames", "0");
	else {
		const states = nonZero([
			["complete", frames.complete],
			["incomplete", frames.incomplete],
			["failed", frames.failed],
			["cancelled", frames.cancelled],
			["running", frames.running],
		]);
		row(
			"Frames",
			`${frames.count}: ${states}${frames.nested > 0 ? ` · ${frames.nested} nested` : ""} · ${tokensText(report.usage.lanes.frames)} tokens`,
		);
		if (frames.calls !== null)
			more(
				style.dim(
					`from ${plural(frames.calls.map, "rlm.map call")} and ${plural(frames.calls.infer, "rlm.infer call")}`,
				),
			);
		const reasons = Object.entries(frames.incompleteReasons);
		if (reasons.length > 0) more(`incomplete: ${reasons.map(([reason, count]) => `${reason} ${count}`).join(" · ")}`);
		for (const model of frames.byModel)
			more(`${model.model}  ${plural(model.count, "frame")}  ${formatTokens(model.tokens)} tokens`);
	}
	if (subagents.count === 0) row("Sub-agents", "0");
	else {
		const states = nonZero([
			["completed", subagents.completed],
			["failed", subagents.failed],
			["cancelled", subagents.cancelled],
			["interrupted", subagents.interrupted],
			["running", subagents.running],
		]);
		row(
			"Sub-agents",
			`${subagents.count}: ${states} · max depth ${subagents.maxDepth}${subagents.nested > 0 ? ` (${subagents.nested} nested)` : ""}`,
		);
		const { verdicts } = subagents;
		more(
			`verdicts: ${verdicts.verified} verified · ${verdicts.contradicted} contradicted · ${verdicts.unverified} unverified${
				verdicts.unverified > 0
					? style.dim(
							` (${nonZero([
								["without a verdict", verdicts.none],
								["unchecked", verdicts.unchecked],
								["invalid", verdicts.invalid],
							])})`,
						)
					: ""
			}`,
		);
		for (const model of subagents.byModel)
			more(
				`${model.model}  ${plural(model.count, "sub-agent")}  ${
					model.unmeasured === model.count
						? `tokens ${NOT_RECORDED}`
						: `${formatTokens(model.tokens)} tokens${model.unmeasured > 0 ? ` (${model.unmeasured} ${NOT_RECORDED})` : ""}`
				}`,
			);
		for (const worktree of subagents.worktrees)
			more(
				`worktree ${worktree.branch}: ${plural(worktree.changedFiles, "file")}, merge ${worktree.merge ?? NOT_RECORDED}`,
			);
	}
	row("Workflows", depth.workflows === null ? missing("depth.workflows") : plural(depth.workflows.runs, "run"));
	const typedAgents = Object.entries(depth.typedAgents.byDefinition);
	row(
		"Other work",
		`${plural(depth.typedAgents.count, "typed-agent task")}${typedAgents.length > 0 ? ` (${typedAgents.map(([definition, count]) => `${definition} ${count}`).join(", ")})` : ""} · ${plural(depth.backgroundJobs.count, "background job")}`,
	);

	// Tokens and cost.
	lines.push("");
	lines.push(style.bold("Tokens and cost"));
	const usageRow = (name: string, bucket: UsageBucket): string[] => [
		`  ${name}`,
		String(bucket.responses),
		formatTokens(bucket.input),
		formatTokens(bucket.output),
		formatTokens(bucket.cacheRead),
		formatTokens(bucket.cacheWrite),
		tokensText(bucket),
		formatCost(bucket.cost, bucket.totalTokens),
	];
	const rootUsage = report.usage.lanes.root;
	const rows: string[][] = [["  lane", "responses", "input", "output", "cache read", "cache write", "total", "cost"]];
	if (report.mode === "ultron claude" && rootUsage.responses === 0)
		rows.push(["  root", "n/r", "n/r", "n/r", "n/r", "n/r", "n/r", "n/r"]);
	else rows.push(usageRow("root", rootUsage));
	rows.push(usageRow("frames", report.usage.lanes.frames));
	rows.push(usageRow("sub-agents", report.usage.lanes.subagents));
	if (report.usage.lanes.other.responses > 0 || report.usage.lanes.other.unmeasured > 0)
		rows.push(usageRow("other", report.usage.lanes.other));
	rows.push(usageRow("total", report.usage.total));
	const numeric = new Set([1, 2, 3, 4, 5, 6]);
	const rendered = table(rows, numeric);
	lines.push(style.dim(rendered[0]!), ...rendered.slice(1));
	if (report.usage.models.length > 0) {
		const modelRows = [["  model", "responses", "input", "output", "cache read", "cache write", "total", "cost"]];
		for (const model of report.usage.models) modelRows.push(usageRow(model.model, model));
		const renderedModels = table(modelRows, numeric);
		lines.push(style.dim(renderedModels[0]!), ...renderedModels.slice(1));
	}
	const notes: string[] = [];
	if (report.mode === "ultron claude")
		notes.push(
			"root n/r: Claude Code owns the root conversation; its tokens and cost are Claude Code's, not in this session.",
		);
	if (report.usage.total.cost.subscriptionUsd !== null)
		notes.push("(sub): made on a subscription login; the amount is the provider's notional figure, not a charge.");
	if (report.usage.total.cost.unpricedResponses > 0)
		notes.push(
			`unknown: ${plural(report.usage.total.cost.unpricedResponses, "response")} used tokens and reported no price.`,
		);
	if (report.usage.total.unmeasured > 0)
		notes.push(
			`+?: ${plural(report.usage.total.unmeasured, "sub-agent")} reported no usage; their tokens are not in the totals.`,
		);
	for (const note of notes) lines.push(style.dim(`  ${note}`));

	// Guardrails.
	lines.push("");
	lines.push(style.bold("Guardrails"));
	const { guardrails } = report;
	const since =
		guardrails.countersSince === null
			? ""
			: style.dim(` (counted since ${formatDate(guardrails.countersSince, utc)})`);
	if (guardrails.guards === null) row("  Loki", missing("guardrails.guards"));
	else if (Object.keys(guardrails.guards).length === 0) row("  Loki", `no write was checked${since}`);
	else
		for (const [name, guard] of Object.entries(guardrails.guards))
			row(
				`  ${name}`,
				`${plural(guard.checks, "check")}, ${guard.blocked} blocked, ${guard.unchecked} unchecked · after cells: ${plural(guard.afterChecks, "check")}, ${plural(guard.afterFindings, "finding")} · ${formatElapsed(guard.ms)}${since}`,
			);
	row(
		"  Secrets",
		guardrails.secretsMasked === null ? missing("guardrails.secretsMasked") : `${guardrails.secretsMasked} masked`,
	);
	const hints = Object.entries(guardrails.hints);
	row(
		"  Hints",
		`stuck-loop ${guardrails.hints["stuck-loop"] ?? 0}${hints
			.filter(([tag]) => tag !== "stuck-loop")
			.map(([tag, count]) => ` · ${tag} ${count}`)
			.join("")}`,
	);
	row(
		"  Nudges",
		guardrails.nudges === null
			? missing("guardrails.nudges")
			: `tool rounds ${guardrails.nudges.toolRounds} · wait ${guardrails.nudges.wait} · skill ${guardrails.nudges.skill}`,
	);
	row(
		"  Limits",
		guardrails.usageLimitBlocks === null
			? missing("guardrails.usageLimitBlocks")
			: `${plural(guardrails.usageLimitBlocks, "usage-limit block")}${since}`,
	);

	// Memory.
	lines.push("");
	lines.push(style.bold("Memory"));
	const { jev } = report.memory;
	if (jev === null) row("  Jev", missing("memory.jev"));
	else
		row(
			"  Jev",
			`${plural(jev.decisions, "decision")}${jev.capped ? " (the newest; older ones were dropped)" : ""}: recall ${jev.recall.total} (${jev.recall.retrieved} retrieved, ${jev.recall.skipped} skipped${jev.recall.failed > 0 ? `, ${jev.recall.failed} failed` : ""}) · retain ${jev.retain.total} (${jev.retain.kept} kept, ${jev.retain.skipped} skipped${jev.retain.sensitive > 0 ? `, ${jev.retain.sensitive} sensitive` : ""}${jev.retain.failed > 0 ? `, ${jev.retain.failed} failed` : ""})${jev.triage > 0 ? ` · triage ${jev.triage}` : ""}`,
		);
	if (report.memory.operations !== null)
		row(
			"  Store",
			Object.entries(report.memory.operations)
				.map(([operation, count]) => `${operation} ${count}`)
				.join(" · "),
		);
	return lines;
}

/** Root model for the table: the model that answered most, or the lane's kind when Claude Code owns the root. */
function tableModel(report: SessionReport): string {
	const model = report.root.models[0]?.model;
	if (model !== undefined) return report.root.models.length > 1 ? `${model} +${report.root.models.length - 1}` : model;
	return report.mode === "ultron claude" ? "Claude Code" : "-";
}

function clip(text: string, width: number): string {
	return text.length <= width ? text : `…${text.slice(text.length - width + 1)}`;
}

/** One line per session, in the order given: when it was last written, cwd, mode, model, turns, cells, frames, sub-agents, tokens, cost, depth. */
export function renderSessionTable(reports: readonly SessionReport[], options: ReportTextOptions = {}): string[] {
	const style = options.style ?? PLAIN_REPORT_STYLE;
	const utc = options.utc === true;
	const home = options.home ?? homedir();
	const rows: string[][] = [
		["LAST ACTIVE", "ID", "CWD", "MODE", "MODEL", "TURNS", "CELLS", "FRAMES", "SUBS", "TOKENS", "COST", "DEPTH"],
	];
	let unrecorded = false;
	for (const report of reports) {
		const count = (value: number | null | undefined): string => {
			if (value === null || value === undefined) {
				unrecorded = true;
				return "n/r";
			}
			return String(value);
		};
		// Claude Code owns the root's usage: with no frame or sub-agent usage either, nothing is known, not zero.
		const { total } = report.usage;
		const unknownUsage = report.mode === "ultron claude" && total.responses === 0 && total.unmeasured === 0;
		rows.push([
			formatDate(report.session.modifiedAt, utc),
			report.session.id.slice(0, 13),
			clip(shortPath(report.session.cwd, home), 32),
			report.mode,
			clip(tableModel(report), 34),
			count(report.turns?.count),
			count(report.cells?.total.count),
			String(report.depth.frames.count),
			String(report.depth.subagents.count),
			unknownUsage ? count(null) : tokensText(total),
			unknownUsage ? count(null) : formatCost(total.cost, total.totalTokens),
			report.depth.verdict,
		]);
	}
	const rendered = table(rows, new Set([5, 6, 7, 8, 9]));
	const lines = [style.dim(rendered[0]!), ...rendered.slice(1)];
	if (unrecorded)
		lines.push(
			style.dim(
				"n/r: not recorded by that session (never zero). Tokens and cost of an `ultron claude` session leave out the root, which is Claude Code's.",
			),
		);
	return lines;
}
