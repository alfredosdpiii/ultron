/**
 * A root agent that lives outside Ultron: Claude Code calling the `rlm` tool over MCP (`ultron mcp`). Ultron's
 * runtime is the same as for its own root lane (kernel, hints, file hooks, secret masking, host, children, frames,
 * usage ledger), but nothing here ever runs the root lane: Claude Code owns the conversation and the model loop.
 *
 * What changes is how the root's side of the runtime is driven:
 * - a cell is run directly (`runCell`), not as a tool call of a harness run;
 * - a root turn (the usage window for budgets and the stuck-loop count) is a Claude Code user turn, opened by the
 *   UserPromptSubmit hook (`beginTurn`) or by the first cell of a turn, and closed by the Stop hook (`endTurn`);
 * - completion events for root-owned work cannot start a run: they wait in an inbox and lead the next cell result,
 *   or go out with the next user prompt's hook context;
 * - the research-loop brake and the wait nudge, which steer a native run, are appended to the next cell result.
 */
import { randomUUID } from "node:crypto";
import type { AgentHarnessToolInvocation } from "@ultron/agent-core";
import type { Context } from "@ultron/chord";
import { BACKGROUND_CONTEXT, withAbortSignal } from "@ultron/chord/context";
import { type RuntimeEvent, runtimeEventText } from "../async-events.ts";
import { SkillExtractionNudger, ToolRoundNudger } from "../tool-round-nudge.ts";

/** Operation ids of cells run for an external root start with this; the rlm tool maps them to the root lane. */
export const EXTERNAL_ROOT_OPERATION = "external-root:";

/** At most this many events are kept waiting; older ones are dropped with a count. */
const MAX_INBOX = 64;

export type ExternalCellContent = { type: "text"; text: string } | { type: "image"; data: string; mimeType: string };

export type ExternalCellResult = { content: ExternalCellContent[]; isError: boolean };

/** What the runtime gives the controller (all of it built by the session worker for its own root lane too). */
export interface ExternalRootDeps {
	/** The rlm tool's execute for one cell of the root lane. */
	execute(code: string, invocation: AgentHarnessToolInvocation, context: Context): Promise<{ content: unknown[] }>;
	host: {
		beginRootTurn(runId: string): void;
		endRootTurn(runId: string): void;
		rootIdOfRun(runId: string): string;
		pendingRootNotifications(): number;
	};
	usage: { turnBudgetExhausted(rootId: string | undefined): Promise<string | undefined> };
	hints: { runEnded(lane: string): void };
	fileHooks: { beginTurn(): void };
	/** Loki's one-time setup note, and its short policy note. */
	lokiNotice: Promise<string | undefined>;
	lokiContext?: string;
	/** ULTRON_TOOL_ROUNDS_NUDGE and ULTRON_SKILL_NUDGE thresholds (0 disables). */
	toolRoundsNudge: number;
	skillNudge: number;
	asyncEvents: boolean;
	/** Told each steer appended to a cell result, and each cell refused because the turn's budget was spent. */
	onNudge?: (message: string) => void;
	onUsageLimit?: () => void;
	now?: () => number;
}

export class ExternalRootController {
	readonly #deps: ExternalRootDeps;
	readonly #inbox: RuntimeEvent[] = [];
	#dropped = 0;
	/** Notes for the root from the nudgers, appended to the next cell result. */
	readonly #notes: string[] = [];
	readonly #nudger: ToolRoundNudger;
	readonly #skillNudger: SkillExtractionNudger;
	readonly #prefix = `cc-${Date.now().toString(36)}-${randomUUID().slice(0, 8)}`;
	#turns = 0;
	#turn: string | undefined;
	#cells = 0;
	#lokiShown = false;

	constructor(deps: ExternalRootDeps) {
		this.#deps = deps;
		const note = async (message: string) => {
			this.#notes.push(message);
			deps.onNudge?.(message);
		};
		this.#nudger = new ToolRoundNudger(deps.toolRoundsNudge, note, { asyncEvents: deps.asyncEvents, nextCall: true });
		this.#skillNudger = new SkillExtractionNudger(deps.skillNudge, note);
	}

	/** The async dispatcher's sink for root-lane events. */
	readonly sink = (events: RuntimeEvent[]): void => {
		this.#inbox.push(...events);
		while (this.#inbox.length > MAX_INBOX) {
			this.#inbox.shift();
			this.#dropped += 1;
		}
	};

	/** Root events waiting for delivery. */
	get pendingEvents(): number {
		return this.#inbox.length;
	}

	/** The current root turn id, if one is open. */
	get turn(): string | undefined {
		return this.#turn;
	}

	get cells(): number {
		return this.#cells;
	}

	/** Waiting events as `<runtime_event>` lines (and clears them), or undefined when none wait. */
	takeEvents(): string | undefined {
		if (this.#inbox.length === 0 && this.#dropped === 0) return undefined;
		const events = this.#inbox.splice(0);
		const dropped = this.#dropped;
		this.#dropped = 0;
		const lines = events.length === 0 ? "" : runtimeEventText(events);
		return dropped === 0
			? lines
			: `${lines}${lines ? "\n" : ""}[${dropped} older runtime event(s) dropped; \`await agents.status()\` lists all work]`;
	}

	/** Loki's policy note and, once, its setup note: the context a session starts with. */
	async sessionContext(): Promise<string | undefined> {
		const parts: string[] = [];
		if (this.#deps.lokiContext) parts.push(this.#deps.lokiContext);
		const notice = await this.#lokiNotice();
		if (notice) parts.push(notice);
		return parts.length === 0 ? undefined : parts.join("\n\n");
	}

	async #lokiNotice(): Promise<string | undefined> {
		if (this.#lokiShown) return undefined;
		// A commit still running (a slow hook) is waited for briefly; otherwise the note comes with a later turn.
		const notice = await Promise.race([
			this.#deps.lokiNotice.catch(() => undefined),
			new Promise<undefined>((done) => setTimeout(() => done(undefined), 5_000).unref?.()),
		]);
		if (notice === undefined) return undefined;
		this.#lokiShown = true;
		return notice;
	}

	/**
	 * A user turn starts (UserPromptSubmit): open a root turn and return the context to add to the prompt, namely
	 * Loki's notice and events that arrived since the last cell. Never throws.
	 */
	async beginTurn(): Promise<string | undefined> {
		if (this.#turn !== undefined) await this.endTurn();
		this.#openTurn();
		const parts: string[] = [];
		const notice = await this.#lokiNotice().catch(() => undefined);
		if (notice) parts.push(notice);
		const events = this.takeEvents();
		if (events) parts.push(`Runtime events since your last rlm call:\n${events}`);
		return parts.length === 0 ? undefined : parts.join("\n\n");
	}

	/** The turn ended (Stop): budget window, stuck-loop count. Never throws. */
	async endTurn(): Promise<void> {
		const turn = this.#turn;
		if (turn === undefined) return;
		this.#turn = undefined;
		this.#deps.host.endRootTurn(turn);
		this.#deps.hints.runEnded("main");
		this.#nudger.runEnded(turn);
		this.#skillNudger.runEnded(turn);
	}

	#openTurn(): string {
		this.#turns += 1;
		const turn = `${this.#prefix}-${this.#turns}`;
		this.#turn = turn;
		this.#deps.host.beginRootTurn(turn);
		this.#deps.fileHooks.beginTurn();
		return turn;
	}

	/** Run one cell in the root kernel: the rlm tool's own result, with waiting events first and notes last. */
	async runCell(code: string, signal?: AbortSignal): Promise<ExternalCellResult> {
		const turn = this.#turn ?? this.#openTurn();
		this.#cells += 1;
		const events = this.takeEvents();
		const lead = events ? `${events}\n` : "";
		const exhausted = await this.#deps.usage
			.turnBudgetExhausted(this.#deps.host.rootIdOfRun(turn))
			.catch(() => undefined);
		if (exhausted !== undefined) {
			this.#deps.onUsageLimit?.();
			return {
				content: [{ type: "text", text: `${lead}[Ultron] ${exhausted}; answer with what you have.` }],
				isError: true,
			};
		}
		const invocation: AgentHarnessToolInvocation = {
			invocationId: `${turn}:${this.#cells}`,
			operationId: `${EXTERNAL_ROOT_OPERATION}${turn}`,
			turnId: `${turn}:${this.#cells}`,
			getMemo: async () => undefined,
			setMemo: async () => {},
		};
		const context = signal === undefined ? BACKGROUND_CONTEXT : withAbortSignal(signal, BACKGROUND_CONTEXT);
		let content: ExternalCellContent[];
		let isError = false;
		try {
			const result = await this.#deps.execute(code, invocation, context);
			content = result.content.filter(isCellContent);
		} catch (error) {
			isError = true;
			content = [{ type: "text", text: error instanceof Error ? error.message : String(error) }];
		}
		const running = this.#deps.host.pendingRootNotifications();
		this.#nudger.turnEnded(turn, 1, running);
		this.#skillNudger.turnEnded(turn, 1, isError ? 1 : 0, running);
		const notes = this.#notes.splice(0);
		const first = content.find((part) => part.type === "text");
		const text = `${lead}${first?.type === "text" ? first.text : "(no result)"}${notes.length ? `\n${notes.join("\n")}` : ""}`;
		const images = content.filter((part) => part.type === "image");
		return { content: [{ type: "text", text }, ...images], isError };
	}
}

function isCellContent(part: unknown): part is ExternalCellContent {
	if (typeof part !== "object" || part === null) return false;
	const value = part as Record<string, unknown>;
	return (
		(value.type === "text" && typeof value.text === "string") ||
		(value.type === "image" && typeof value.data === "string" && typeof value.mimeType === "string")
	);
}
