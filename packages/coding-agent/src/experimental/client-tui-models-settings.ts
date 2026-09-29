/**
 * `/settings → Models` in the native TUI: the rows (effective value and source of each model choice) and the
 * pickers' entries, built from the worker's settings read and the models the Session's providers make available.
 * The session row switches the Session's model the way `/model` does (and saves it as the default); every other
 * row writes its setting through `SessionControl.setSetting`, and the worker reads it for the next frame, review
 * or sub-agent. Claude Code mode rows are saved for the next `ultron claude`.
 */

import type { JsonValue } from "@ultron/chord";
import { BACKGROUND_CONTEXT } from "@ultron/chord/context";
import type { Component } from "@ultron/tui";
import {
	type ModelChoice,
	type ModelSettingRow,
	ModelsSettingsSubmenu,
} from "../modes/interactive/components/models-settings.ts";
import { DEFAULT_CLAUDE_MODEL } from "../ultron/claude/claude-cli.ts";
import type { PiCommandHost } from "./client-tui-pi-commands.ts";
import type { ModelRef, ModelSummary } from "./services/models.ts";
import type { WorkerSettingResult, WorkerSettingsRead } from "./services/session-control.ts";

/** Persist a setting in the worker (client-tui-settings' `changeSetting`); undefined when it failed. */
export type ChangeSetting = (key: string, value: JsonValue) => Promise<WorkerSettingResult["applied"] | undefined>;

/** What a live change reaches next, for the status line. */
const NEXT: Record<string, string> = {
	"rlm.frameModel": "the next frame",
	"rlm.frameThinking": "the next frame",
	"review.model": "the next /review",
	"rlm.childModel": "the next sub-agent",
};

const CLAUDE_CODE = "claude-code";
const FRAME_THINKING_CHOICES = ["off", "low", "medium", "high"] as const;

export interface ModelsSectionInput {
	readonly values: Readonly<Record<string, JsonValue>>;
	readonly envOverrides: Readonly<Record<string, { name: string; value: string }>>;
	readonly available: readonly ModelSummary[];
	readonly session: ModelRef | undefined;
}

const ref = (model: ModelRef): string => `${model.provider}/${model.modelId}`;
/** The Claude Code CLI provider has no tool calling: it serves frames, never a session or a sub-agent. */
const toolCapable = (model: ModelSummary): boolean =>
	model.provider !== CLAUDE_CODE && model.model?.api !== "claude-code-cli";

function modelChoices(models: readonly ModelSummary[]): ModelChoice[] {
	return models.map((model) => ({
		value: ref(model),
		label: model.modelId,
		...(model.name && model.name !== model.modelId ? { description: model.name } : {}),
		group: model.provider,
	}));
}

/** The rows of the Models section. */
export function modelSettingRows(input: ModelsSectionInput): ModelSettingRow[] {
	const text = (key: string): string | undefined => {
		const value = input.values[key];
		return typeof value === "string" && value !== "" ? value : undefined;
	};
	const all = modelChoices(input.available);
	const tools = modelChoices(input.available.filter(toolCapable));
	const claudeModels = input.available.filter((model) => model.provider === CLAUDE_CODE);
	const claudeChoices = modelChoices(claudeModels);
	const session = input.session === undefined ? "none" : ref(input.session);
	const locked = (key: string): { locked?: string; env?: { name: string; value: string } } => {
		const env = input.envOverrides[key];
		return env === undefined
			? {}
			: { env, locked: `Set by ${env.name}; unset it to choose here (the variable wins over the setting).` };
	};
	const row = (
		id: string,
		label: string,
		description: string,
		display: string,
		choices: readonly ModelChoice[] | undefined,
		current: string | undefined,
	): ModelSettingRow => {
		const lock = locked(id);
		return {
			id,
			label,
			description,
			display: lock.env === undefined ? display : `${lock.env.value} (set by ${lock.env.name})`,
			...(lock.locked === undefined ? {} : { locked: lock.locked }),
			...(choices === undefined ? {} : { choices: () => choices }),
			current: current ?? "",
		};
	};
	const frameModel = text("rlm.frameModel");
	const frameEnv = input.envOverrides["rlm.frameModel"];
	const effectiveFrame = frameEnv?.value ?? frameModel;
	const reviewModel = text("review.model");
	const childModel = text("rlm.childModel");
	const frameThinking = text("rlm.frameThinking");
	const ccRoot = text("claudeCode.model");
	const ccFrame = text("claudeCode.frameModel");
	const ccChild = text("claudeCode.childModel");
	const ccUltronChild = text("claudeCode.ultronChildModel");
	const defaultProvider = text("defaultProvider");
	const defaultModel = text("defaultModel");
	const savedDefault = defaultProvider && defaultModel ? `${defaultProvider}/${defaultModel}` : undefined;
	const alias = (value: string | undefined) => (value === undefined ? undefined : `${CLAUDE_CODE}/${value}`);
	return [
		row(
			"session",
			"Session model",
			`The Session's own model (tool-capable models only). Choosing one switches the running Session now and saves it as the default, as /model does. Saved default: ${savedDefault ?? "none"}.`,
			`${session} (session)`,
			tools,
			input.session === undefined ? undefined : ref(input.session),
		),
		row(
			"rlm.frameModel",
			"Frame model",
			"Code-free rlm.infer / rlm.map frames that name no model= (and /review frames without a review model). Order: model= > ULTRON_RLM_FRAME_MODEL > this setting > the session model.",
			frameModel === undefined ? `same as session (${session})` : `${frameModel} (setting)`,
			[{ value: "", label: "Same as session model" }, ...all],
			frameModel,
		),
		row(
			"review.model",
			"Review model",
			"/review frames. Order: --model > ULTRON_REVIEW_MODEL > this setting > the frame model.",
			reviewModel !== undefined
				? `${reviewModel} (setting)`
				: effectiveFrame !== undefined
					? `${effectiveFrame} (frame model)`
					: `same as session (${session})`,
			[{ value: "", label: "Same as frame model" }, ...all],
			reviewModel,
		),
		row(
			"rlm.childModel",
			"Sub-agent model",
			"rlm.spawn sub-agents that name no model=. Sub-agents call tools, so claude-code models are not offered.",
			childModel === undefined ? `same as session (${session})` : `${childModel} (setting)`,
			[{ value: "", label: "Same as session model" }, ...tools],
			childModel,
		),
		row(
			"rlm.frameThinking",
			"Frame thinking",
			"Thinking level of inference frames (claude-code frames pass it as --effort).",
			frameThinking === undefined ? "same as session" : `${frameThinking} (setting)`,
			[
				{ value: "", label: "Same as session" },
				...FRAME_THINKING_CHOICES.map((level) => ({ value: level, label: level })),
			],
			frameThinking,
		),
		{
			id: "claude-code-heading",
			label: "Claude Code mode",
			description: "`ultron claude`: Claude Code as the root agent. These apply when it next starts.",
			display: "",
			heading: true,
		},
		row(
			"claudeCode.model",
			"Claude Code root model",
			"The model Claude Code itself runs (claude --model); ULTRON_CLAUDE_MODEL overrides it.",
			ccRoot === undefined ? `${DEFAULT_CLAUDE_MODEL} (default)` : `${ccRoot} (setting)`,
			[{ value: "", label: `Default (${DEFAULT_CLAUDE_MODEL})` }, ...claudeChoices],
			alias(ccRoot),
		),
		row(
			"claudeCode.frameModel",
			"Claude Code frame model",
			"rlm.map / rlm.infer frames under `ultron claude`; ULTRON_CLAUDE_FRAME_MODEL overrides it.",
			ccFrame === undefined ? `${CLAUDE_CODE}/${DEFAULT_CLAUDE_MODEL} (default)` : `${ccFrame} (setting)`,
			[{ value: "", label: `Default (${CLAUDE_CODE}/${DEFAULT_CLAUDE_MODEL})` }, ...all],
			ccFrame,
		),
		row(
			"claudeCode.childModel",
			"Claude Code sub-agent model",
			"Model of Claude Code sub-agents (children=claude); ULTRON_CLAUDE_CHILD_MODEL overrides it.",
			ccChild === undefined ? `${DEFAULT_CLAUDE_MODEL} (default)` : `${ccChild} (setting)`,
			[{ value: "", label: `Default (${DEFAULT_CLAUDE_MODEL})` }, ...claudeChoices],
			alias(ccChild),
		),
		row(
			"claudeCode.ultronChildModel",
			"Ultron sub-agent model",
			"Model of Ultron sub-agents under `ultron claude` (children=ultron); tool-capable models only.",
			ccUltronChild === undefined ? `default model (${savedDefault ?? "none"})` : `${ccUltronChild} (setting)`,
			[{ value: "", label: "Default model" }, ...tools],
			ccUltronChild,
		),
	];
}

/** The display of a row after `value` was chosen ("" is the row's default). */
export function displayAfterPick(row: ModelSettingRow, value: string, session: string): string {
	if (row.id === "session") return `${value} (session)`;
	if (value !== "") {
		const shown =
			row.id === "claudeCode.model" || row.id === "claudeCode.childModel"
				? value.replace(/^claude-code\//, "")
				: value;
		return `${shown} (setting)`;
	}
	switch (row.id) {
		case "rlm.frameModel":
		case "rlm.childModel":
			return `same as session (${session})`;
		case "review.model":
			return "same as frame model";
		case "rlm.frameThinking":
			return "same as session";
		case "claudeCode.ultronChildModel":
			return "default model";
		default:
			return "default";
	}
}

/** A one-line summary for the /settings row. */
export function modelsSummary(rows: readonly ModelSettingRow[]): string {
	const display = (id: string) => rows.find((row) => row.id === id)?.display ?? "";
	return `frames: ${display("rlm.frameModel")}`;
}

/** The Models section of `/settings`, or undefined without a Session's models. */
export function modelsSection(
	host: PiCommandHost,
	read: Pick<WorkerSettingsRead, "values" | "envOverrides">,
	change: ChangeSetting,
): { summary: string; open(done: (summary?: string) => void): Component } | undefined {
	const models = host.models();
	if (models === undefined) return undefined;
	const input = (): ModelsSectionInput => {
		const state = models.state.value;
		return {
			values: read.values,
			envOverrides: read.envOverrides ?? {},
			available: state?.catalog.availableModels ?? [],
			session: state?.configuration.model ?? host.currentModel() ?? undefined,
		};
	};
	return {
		summary: modelsSummary(modelSettingRows(input())),
		open(done) {
			const values = read.values as Record<string, JsonValue>;
			const session = input().session;
			let submenu: ModelsSettingsSubmenu | undefined;
			/** Re-derive every row (a new frame model changes the review row's fallback, for one). */
			const refresh = (): Map<string, ModelSettingRow> => {
				const rows = new Map(modelSettingRows(input()).map((row) => [row.id, row]));
				for (const row of rows.values()) if (!row.heading) submenu?.setDisplay(row.id, row.display, row.current);
				return rows;
			};
			const stored = (id: string, value: string): JsonValue =>
				value === ""
					? null
					: id === "claudeCode.model" || id === "claudeCode.childModel"
						? value.replace(/^claude-code\//, "")
						: value;
			submenu = new ModelsSettingsSubmenu(
				modelSettingRows(input()),
				(row, value) => {
					const error = (cause: unknown) =>
						host.showStatus(`Error: ${cause instanceof Error ? cause.message : String(cause)}`);
					if (row.id === "session") {
						const slash = value.indexOf("/");
						void models
							.select({ provider: value.slice(0, slash), modelId: value.slice(slash + 1) }, BACKGROUND_CONTEXT)
							.then(() => {
								refresh();
								host.showStatus(`Session model: ${value} (saved as default)`);
							})
							.catch((cause: unknown) => {
								refresh();
								error(cause);
							});
						return displayAfterPick(row, value, session === undefined ? "none" : ref(session));
					}
					// Shown at once; put back if the worker refuses it.
					const previous = values[row.id] ?? null;
					values[row.id] = stored(row.id, value);
					void change(row.id, value === "" ? null : value).then((applied) => {
						if (applied === undefined) {
							values[row.id] = previous;
							refresh();
							return;
						}
						const when =
							applied === "live"
								? `applies to ${NEXT[row.id] ?? "the running session"}`
								: "applies when ultron claude next starts";
						host.showStatus(`${row.label}: ${value === "" ? "default" : value} (${when})`);
					});
					return refresh().get(row.id)?.display;
				},
				() => done(modelsSummary(modelSettingRows(input()))),
			);
			return submenu;
		},
	};
}
