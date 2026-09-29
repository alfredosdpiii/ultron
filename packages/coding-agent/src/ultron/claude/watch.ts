/**
 * `ultron watch`: the RLM view of a running `ultron mcp` session (Claude Code's TUI cannot draw it). It polls the
 * server's control socket for the same read-only inspections the native TUI polls (`agents.status {graph: true}`,
 * `instances.list`, `rlm.pool`, `rlm.frames`, `ctx.state`) plus the root cells Claude Code ran, and renders them with
 * the native components: the docked RLM panel (graph, gauges, kernels, guard stats) above the RLM pane (waves of
 * subagents and workflow nodes, frames, jobs, verdicts).
 *
 *   ultron watch                      newest session in this directory
 *   ultron watch --socket <path>      a given session (`ultron claude` prints this command)
 *   ultron watch --list               running sessions
 *   ultron watch --once [--plain]     print one frame and exit
 */

import { type Component, ProcessTerminal, TuiAltScreen, truncateToWidth } from "@ultron/tui";
import { getAgentDir } from "../../config.ts";
import { KeybindingsManager } from "../../core/keybindings.ts";
import { SettingsManager } from "../../core/settings-manager.ts";
import { renderRlmDock } from "../../experimental/rlm-graph.ts";
import { RlmPane } from "../../experimental/rlm-pane.ts";
import {
	PLAIN_STYLE,
	parseAgentsStatus,
	parseContextState,
	parseFrames,
	parsePool,
	parseRetained,
	RlmClock,
	type RlmRootCell,
	type RlmSnapshot,
	type RlmStyle,
} from "../../experimental/rlm-visualizer.ts";
import { initTheme, theme } from "../../modes/interactive/theme/theme.ts";
import { type ControlClient, connectControl, findServer, listServers } from "./control-socket.ts";
import type { RootCellRecord } from "./mcp-server.ts";

const POLL_MS = 500;
const ANSI = /\x1b\[[0-9;?]*[A-Za-z]|\x1b\][^\x07]*\x07/g;
const RLM_FRAME_LIMIT = 20;

export interface WatchArgs {
	socket?: string;
	list: boolean;
	once: boolean;
	plain: boolean;
	width?: number;
}

export function parseWatchArgs(args: readonly string[]): WatchArgs {
	const parsed: WatchArgs = { list: false, once: false, plain: false };
	for (let index = 0; index < args.length; index += 1) {
		const arg = args[index]!;
		if (arg === "--socket") {
			const value = args[++index];
			if (value === undefined) throw new Error("--socket needs a path");
			parsed.socket = value;
		} else if (arg === "--list") parsed.list = true;
		else if (arg === "--once") parsed.once = true;
		else if (arg === "--plain") parsed.plain = true;
		else if (arg === "--width") {
			const value = Number(args[++index]);
			if (!Number.isSafeInteger(value) || value < 20) throw new Error("--width must be an integer >= 20");
			parsed.width = value;
		} else throw new Error(`unknown option for ultron watch: ${arg}`);
	}
	return parsed;
}

/** The server's status line fields. */
type ServerStatus = {
	name?: string;
	cwd?: string;
	ready?: boolean;
	error?: string;
	claudeSessionId?: string;
	frameModel?: string;
	children?: string;
	turn?: string | null;
	cells?: number;
	pendingEvents?: number;
};

/** One poll of a session: everything a frame is drawn from. */
export async function pollSnapshot(
	client: ControlClient,
	clock: RlmClock,
): Promise<{ snapshot: RlmSnapshot; status: ServerStatus }> {
	const inspect = (request: string, payload: Record<string, unknown> = {}) =>
		client.request("inspect", { request, payload }, 10_000);
	const [statusValue, agents, instances, pool, context, frames, cellsValue] = await Promise.allSettled([
		client.request("status", {}, 10_000),
		inspect("agents.status", { graph: true }),
		inspect("instances.list"),
		inspect("rlm.pool"),
		inspect("ctx.state"),
		inspect("rlm.frames", { limit: RLM_FRAME_LIMIT }),
		client.request("cells", {}, 10_000),
	]);
	const now = Date.now();
	const status = (statusValue.status === "fulfilled" ? statusValue.value : {}) as ServerStatus;
	const parsed =
		agents.status === "fulfilled"
			? parseAgentsStatus(agents.value)
			: { tasks: [], usage: null, limits: null, jobs: [], toolCalls: [], workflows: [] };
	const records =
		cellsValue.status === "fulfilled" ? ((cellsValue.value as { cells?: RootCellRecord[] }).cells ?? []) : [];
	const cells: RlmRootCell[] = records.map((cell) => ({
		toolCallId: `cell-${cell.n}`,
		code: cell.code,
		status: cell.status,
		startedAt: cell.startedAt,
		...(cell.endedAt === undefined ? {} : { endedAt: cell.endedAt }),
		...(cell.output === undefined ? {} : { output: cell.output }),
	}));
	const error =
		agents.status === "rejected"
			? agents.reason instanceof Error
				? agents.reason.message
				: String(agents.reason)
			: status.error;
	const snapshot: RlmSnapshot = {
		now,
		tasks: parsed.tasks,
		usage: parsed.usage,
		limits: parsed.limits,
		pool: pool.status === "fulfilled" ? parsePool(pool.value) : null,
		rootCell: cells.at(-1) ?? null,
		retained: instances.status === "fulfilled" ? parseRetained(instances.value) : new Set(),
		timing: clock.timings(parsed.tasks, parsed.usage, now),
		frames: frames.status === "fulfilled" ? parseFrames(frames.value) : [],
		jobs: parsed.jobs,
		toolCalls: parsed.toolCalls,
		workflows: parsed.workflows,
		context: context.status === "fulfilled" ? parseContextState(context.value) : null,
		cells: cells.slice(-12),
		turn: cells.length === 0 ? null : { startedAt: cells[0]!.startedAt },
		...(error === undefined ? {} : { error }),
	};
	return { snapshot, status };
}

function headerLine(status: ServerStatus, style: RlmStyle): string {
	const parts = [
		status.cwd ?? "",
		status.claudeSessionId ? `claude ${status.claudeSessionId.slice(0, 8)}` : undefined,
		status.ready === false ? "starting" : undefined,
		status.frameModel ? `frames ${status.frameModel}` : undefined,
		status.children ? `children ${status.children}` : undefined,
		status.cells === undefined ? undefined : `${status.cells} cells`,
		status.pendingEvents ? `${status.pendingEvents} events waiting` : undefined,
	].filter((part): part is string => Boolean(part));
	return `${style.bold(style.fg("accent", "Ultron"))} ${style.fg("muted", parts.join(" · "))}`;
}

/** One frame as text lines: header, the docked panel, the pane. */
export function renderWatchFrame(
	pane: RlmPane,
	snapshot: RlmSnapshot,
	status: ServerStatus,
	width: number,
	height: number,
	style: RlmStyle,
): string[] {
	const header = truncateToWidth(headerLine(status, style), Math.max(1, width), "…");
	const dock = renderRlmDock(snapshot, Math.max(1, width - 2), {
		style,
		spinnerFrame: Math.floor(Date.now() / 100),
		maxRows: Math.max(6, Math.floor(height * 0.4)),
	}).map((line) => ` ${line}`);
	const rest = Math.max(6, height - dock.length - 2);
	const paneLines = pane.render(width).slice(0, rest);
	return [header, ...dock, "", ...paneLines];
}

function resolveSocket(args: WatchArgs): string {
	if (args.socket !== undefined) return args.socket;
	const server =
		findServer({ cwd: process.cwd() }) ??
		(process.env.CLAUDE_CODE_SESSION_ID
			? findServer({ claudeSessionId: process.env.CLAUDE_CODE_SESSION_ID })
			: undefined) ??
		listServers().find((record) => !record.parent);
	if (server === undefined)
		throw new Error("no running `ultron mcp` session found; start one with `ultron claude`, or pass --socket <path>");
	return server.socket;
}

/** `ultron watch ...`. */
export async function runWatchCommand(argv: readonly string[]): Promise<void> {
	const args = parseWatchArgs(argv);
	if (args.list) {
		for (const server of listServers())
			process.stdout.write(
				`${server.parent ? "  child " : ""}${server.name}\t${server.cwd}\tpid ${server.pid}\t${server.socket}\n`,
			);
		return;
	}
	const socket = resolveSocket(args);
	const client = await connectControl(socket, 5_000);
	const clock = new RlmClock();
	const settings = SettingsManager.create(process.cwd(), getAgentDir(), { projectTrusted: false });
	const keybindings = KeybindingsManager.create();
	let latest = await pollSnapshot(client, clock);
	const plain = args.plain || !process.stdout.isTTY;
	if (!plain) initTheme(settings.getTheme(), false);
	const style: RlmStyle = plain
		? PLAIN_STYLE
		: { fg: (color, text) => theme.fg(color, text), bold: (text) => theme.bold(text) };
	if (args.once) {
		const pane = new RlmPane({
			snapshot: () => latest.snapshot,
			height: () => 30,
			keybindings,
			style,
			focused: () => false,
			onClose: () => {},
			requestRender: () => {},
		});
		const width = args.width ?? (process.stdout.columns || 100);
		const lines = renderWatchFrame(pane, latest.snapshot, latest.status, width, 40, style);
		// Plain output carries no escape codes (truncation adds resets even without colors).
		const text = lines.join("\n");
		process.stdout.write(`${plain ? text.replace(ANSI, "") : text}\n`);
		client.close();
		return;
	}
	const ui = new TuiAltScreen(new ProcessTerminal(), false, getAgentDir());
	let stopped = false;
	let finish: () => void = () => {};
	const stop = (): void => {
		if (stopped) return;
		stopped = true;
		ui.stop();
		client.close();
		finish();
	};
	const pane = new RlmPane({
		snapshot: () => latest.snapshot,
		height: () => Math.max(6, ui.terminal.rows - Math.floor(ui.terminal.rows * 0.4) - 3),
		keybindings,
		style,
		focused: () => true,
		onClose: stop,
		requestRender: () => ui.requestRender(),
	});
	const root: Component = {
		render: (width) => renderWatchFrame(pane, latest.snapshot, latest.status, width, ui.terminal.rows, style),
		handleInput: (data) => {
			if (data === "q" || data === "\u0003") stop();
			else pane.handleInput(data);
		},
		invalidate: () => {},
	};
	ui.addChild(root);
	ui.setLayoutRoot(root);
	ui.setFocus(root);
	ui.start();
	const timer = setInterval(() => {
		void pollSnapshot(client, clock).then(
			(next) => {
				latest = next;
				ui.requestRender();
			},
			() => stop(),
		);
	}, POLL_MS);
	await new Promise<void>((resolve) => {
		finish = resolve;
	});
	clearInterval(timer);
}
