/** Loads `ultron mcp <command>` and the MCP runtime only when the command runs (see cli.ts). */
export const loadMcpCommand = () => import("./cli.ts");

/** Subcommands of `ultron mcp` that manage MCP servers Ultron connects to. */
export const MCP_MANAGEMENT_COMMANDS: readonly string[] = ["add", "remove", "list", "login", "logout", "help"];

/** Whether `ultron mcp <first>` manages MCP servers, rather than starting Ultron's own MCP server. */
export function isMcpManagementCommand(first: string | undefined): boolean {
	return first !== undefined && (MCP_MANAGEMENT_COMMANDS.includes(first) || first === "--help" || first === "-h");
}
