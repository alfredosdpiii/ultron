/** How to start this same Ultron CLI again (for Claude Code's MCP config, its hooks and subagent servers). */
export interface SelfCommand {
	readonly command: string;
	readonly args: readonly string[];
}

/**
 * `ULTRON_SELF_COMMAND` (a JSON array: command and leading args) overrides it; otherwise the running Node binary
 * with its flags and this CLI's entry script.
 */
export function selfCommand(env: NodeJS.ProcessEnv = process.env): SelfCommand {
	const configured = env.ULTRON_SELF_COMMAND?.trim();
	if (configured) {
		const parsed: unknown = JSON.parse(configured);
		if (!Array.isArray(parsed) || parsed.length === 0 || !parsed.every((part) => typeof part === "string"))
			throw new Error("ULTRON_SELF_COMMAND must be a JSON array of strings");
		return { command: parsed[0] as string, args: parsed.slice(1) as string[] };
	}
	const script = process.argv[1];
	if (!script) throw new Error("cannot tell how this Ultron CLI was started");
	return { command: process.execPath, args: [...process.execArgv, script] };
}

/** A shell command line for `self` plus `args` (hook commands are run by a shell). */
export function shellCommand(self: SelfCommand, args: readonly string[]): string {
	return [self.command, ...self.args, ...args].map(shellQuote).join(" ");
}

export function shellQuote(value: string): string {
	return /^[A-Za-z0-9_@%+=:,./-]+$/.test(value) ? value : `'${value.replace(/'/g, `'\\''`)}'`;
}
