import type { JsonValue } from "@earendil-works/chord";
import type { PresentationUI } from "./presentation-ui.ts";
import type { SessionControl } from "./session-control.ts";
import type { SlashCommandContribution } from "./slash-commands.ts";

const MAX_LINES = 40;

/** One inspector command: a subcommand word maps to a read-only host request. */
interface InspectorSpec {
	readonly name: string;
	readonly description: string;
	readonly argumentHint: string;
	/** Maps the argument text to a host request, or throws a usage error. */
	request(args: string): { type: string; payload: Record<string, JsonValue> };
}

const INSPECTORS: readonly InspectorSpec[] = [
	{
		name: "agents",
		description: "Show Ultron tasks, definitions, and usage",
		argumentHint: "[task-id]",
		request: (args) => (args ? inspect("agents.inspect", { id: args }) : inspect("agents.status", {})),
	},
	{
		name: "memory",
		description: "Show recorded memory evidence without a new search",
		argumentHint: "why <task-id> | list",
		request(args) {
			const [verb, target] = words(args);
			if (verb === "why" && target) return inspect("memory.why", { taskId: target });
			if (verb === undefined || verb === "list") return inspect("memory.list", {});
			throw new Error("Usage: /memory why <task-id> | /memory list");
		},
	},
	{
		name: "skills",
		description: "Show skill versions or why a skill was selected",
		argumentHint: "why <decision-id> | list",
		request(args) {
			const [verb, target] = words(args);
			if (verb === "why" && target) return inspect("skills.why", { decision_id: target });
			if (verb === undefined || verb === "list") return inspect("skills.list", {});
			throw new Error("Usage: /skills why <decision-id> | /skills list");
		},
	},
	{
		name: "experiments",
		description: "Show recorded experiment attempts",
		argumentHint: "",
		request: () => inspect("experiments.list", {}),
	},
	{
		name: "goals",
		description: "Show goals and their verification state",
		argumentHint: "[goal-id]",
		request: (args) => (args ? inspect("goals.get", { id: args }) : inspect("goals.list", {})),
	},
	{
		name: "progress",
		description: "Show progress receipts and decisions for a task",
		argumentHint: "<task-id>",
		request(args) {
			if (!args) throw new Error("Usage: /progress <task-id>");
			return inspect("progress.history", { task_id: args });
		},
	},
];

export function inspectionCommands(control: SessionControl, ui: PresentationUI): SlashCommandContribution[] {
	return INSPECTORS.map((spec) => ({
		name: spec.name,
		description: spec.description,
		argumentHint: spec.argumentHint,
		async run(args, context) {
			const request = spec.request(args.trim());
			const value = await control.inspect(request.type, request.payload, context);
			ui.showStatus(formatInspection(`/${spec.name} ${args}`.trim(), value), context);
			return undefined;
		},
	}));
}

/** Render JSON as indented `key: value` lines, bounded so a large record cannot flood the screen. */
export function formatInspection(title: string, value: JsonValue): string {
	const lines = [title];
	const scalar = (item: JsonValue): string => (typeof item === "string" ? item : JSON.stringify(item));
	const render = (item: JsonValue, indent: string, label: string | undefined): void => {
		const head = label === undefined ? indent : `${indent}${label}: `;
		if (item === null || typeof item !== "object") {
			lines.push(`${head}${scalar(item)}`);
			return;
		}
		const empty = Array.isArray(item) ? item.length === 0 : Object.keys(item).length === 0;
		if (empty) {
			lines.push(`${head}(none)`);
			return;
		}
		if (label !== undefined) lines.push(`${indent}${label}:`);
		const inner = label === undefined ? indent : `${indent}  `;
		if (Array.isArray(item)) {
			for (const child of item) {
				if (child === null || typeof child !== "object") {
					lines.push(`${inner}- ${scalar(child)}`);
				} else {
					lines.push(`${inner}-`);
					render(child, `${inner}  `, undefined);
				}
			}
			return;
		}
		for (const [key, child] of Object.entries(item)) render(child, inner, key);
	};
	render(value, "", undefined);
	if (lines.length <= MAX_LINES) return lines.join("\n");
	return [...lines.slice(0, MAX_LINES), `… ${lines.length - MAX_LINES} more lines`].join("\n");
}

function inspect(
	type: string,
	payload: Record<string, JsonValue>,
): { type: string; payload: Record<string, JsonValue> } {
	return { type, payload };
}

function words(args: string): string[] {
	return args.split(/\s+/).filter(Boolean);
}
