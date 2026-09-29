/**
 * `/settings → Models` (Ultron's native TUI): one row per model choice (session, frames, /review, sub-agents,
 * Claude Code mode), each showing its effective value and where it comes from. Selecting a row opens a model
 * picker over the models the user's providers make available, grouped by provider with the current value marked;
 * there is no free-text entry. A row an environment variable overrides is locked and says which variable.
 */
import { Container, type SelectItem, SettingsList, Spacer, Text } from "@ultron/tui";
import { getSettingsListTheme, theme } from "../theme/theme.ts";
import { SelectSubmenu } from "./settings-submenu.ts";

const PICKER_LAYOUT = { minPrimaryColumnWidth: 12, maxPrimaryColumnWidth: 46 };
/** Picker rows that only label a provider group; choosing one does nothing. */
const GROUP_PREFIX = "\u0000group:";

/** One entry of a model picker. */
export interface ModelChoice {
	/** What the row saves: `provider/model`, a thinking level, or a row-specific token such as "" for the default. */
	readonly value: string;
	readonly label: string;
	readonly description?: string;
	/** The provider heading the entry is listed under; entries without one come first (e.g. "Same as session"). */
	readonly group?: string;
}

export interface ModelSettingRow {
	/** The setting key (or a row id such as "session"). */
	readonly id: string;
	readonly label: string;
	readonly description: string;
	/** The effective value and its source, e.g. `claude-code/haiku (setting)`. */
	readonly display: string;
	/** Why the row cannot be changed here (an environment variable overrides it); the row then opens nothing. */
	readonly locked?: string;
	/** A heading row with no value (e.g. "Claude Code mode"). */
	readonly heading?: boolean;
	/** The picker's entries (built when the row opens); the value of `current` is marked. */
	readonly choices?: () => readonly ModelChoice[];
	/** The value the picker marks and preselects. */
	readonly current?: string;
}

/** The picker's list: entries without a group first, then one heading per provider, entries sorted by label. */
export function modelPickerItems(choices: readonly ModelChoice[], current: string | undefined): SelectItem[] {
	const mark = (choice: ModelChoice): SelectItem => ({
		value: choice.value,
		label: `${choice.value === current ? "✓ " : "  "}${choice.label}`,
		...(choice.description === undefined ? {} : { description: choice.description }),
	});
	const items = choices.filter((choice) => choice.group === undefined).map(mark);
	const groups = new Map<string, ModelChoice[]>();
	for (const choice of choices) {
		if (choice.group === undefined) continue;
		const list = groups.get(choice.group) ?? [];
		list.push(choice);
		groups.set(choice.group, list);
	}
	for (const group of [...groups.keys()].sort((a, b) => a.localeCompare(b))) {
		items.push({ value: `${GROUP_PREFIX}${group}`, label: theme.fg("muted", `── ${group} ──`) });
		items.push(
			...groups
				.get(group)!
				.sort((a, b) => a.label.localeCompare(b.label))
				.map(mark),
		);
	}
	return items;
}

/**
 * The Models submenu. `onPick` saves a choice and returns the row's new display (or undefined to keep it);
 * `setDisplay` corrects a row afterwards (a save that failed).
 */
export class ModelsSettingsSubmenu extends Container {
	private readonly list: SettingsList;
	private readonly current: Map<string, string | undefined>;

	constructor(
		rows: readonly ModelSettingRow[],
		onPick: (row: ModelSettingRow, value: string) => string | undefined,
		onClose: () => void,
	) {
		super();
		this.addChild(new Text(theme.bold(theme.fg("accent", "Models")), 0, 0));
		this.addChild(
			new Text(
				theme.fg(
					"muted",
					"Changes apply to the running session: the next frame, review or sub-agent uses them. Claude Code mode rows apply when `ultron claude` next starts.",
				),
				0,
				0,
			),
		);
		this.addChild(new Spacer(1));
		const current = new Map(rows.map((row) => [row.id, row.current]));
		this.current = current;
		const items = rows.map((row) => ({
			id: row.id,
			label: row.heading ? theme.bold(row.label) : row.label,
			description: row.locked === undefined ? row.description : `${row.description} ${row.locked}`,
			currentValue: row.locked === undefined ? row.display : `${row.display} · locked`,
			...(row.heading || row.locked !== undefined || row.choices === undefined
				? {}
				: {
						submenu: (_value: string, done: (selected?: string) => void) =>
							new SelectSubmenu(
								row.label,
								row.description,
								modelPickerItems(row.choices!(), current.get(row.id)),
								current.get(row.id) ?? "",
								(value) => {
									if (value.startsWith(GROUP_PREFIX)) return;
									current.set(row.id, value);
									done(onPick(row, value));
								},
								() => done(),
								undefined,
								{ searchable: true, layout: PICKER_LAYOUT },
							),
					}),
		}));
		this.list = new SettingsList(items, Math.min(items.length, 14), getSettingsListTheme(), () => {}, onClose);
		this.addChild(this.list);
	}

	/** Show `display` (and mark `current`) for row `id`, e.g. after a save that failed. */
	setDisplay(id: string, display: string, current?: string): void {
		this.list.updateValue(id, display);
		this.current.set(id, current);
	}

	getSettingsList(): SettingsList {
		return this.list;
	}

	handleInput(data: string): void {
		this.list.handleInput(data);
	}
}
