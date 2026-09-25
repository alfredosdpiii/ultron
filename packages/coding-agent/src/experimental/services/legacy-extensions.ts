import { type Context, defineService, type JsonValue } from "@earendil-works/chord";

export interface LegacyExtensionCommandInfo {
	readonly name: string;
	readonly description?: string;
}

export interface LegacyExtensionCommandResult {
	readonly notifications: readonly string[];
}

export interface LegacyExtensionCommands {
	list(context: Context): Promise<readonly LegacyExtensionCommandInfo[]>;
	run(name: string, args: string, context: Context): Promise<LegacyExtensionCommandResult>;
}

export const LegacyExtensionCommands = defineService<LegacyExtensionCommands>("pi.legacy-extension-commands");

export function legacyExtensionCommandJson(value: LegacyExtensionCommandResult): JsonValue {
	return { notifications: [...value.notifications] };
}
