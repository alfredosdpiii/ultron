/**
 * The built-in MCP extension next to other extensions that do the same job.
 *
 * pi-mcp-adapter registered an `mcp` gateway tool and `/mcp`, like the built-in extension. With both loaded the
 * tool would be registered twice, so the adapter is left out: MCP is native now, and its `mcp.json` keeps working.
 * Any other extension that registers an `mcp` tool takes over from the built-in one instead (the user chose it).
 */

import type { LoadExtensionsResult } from "../../core/extensions/types.ts";

/** Path of the built-in MCP extension, as the resource loader names inline extensions. */
export const BUILTIN_MCP_EXTENSION_PATH = "<inline:mcp>";

const ADAPTER_PACKAGE = /(^|[\\/])pi-mcp-adapter([\\/]|$)/;

export const ADAPTER_NOTICE =
	"pi-mcp-adapter is installed but no longer needed: Ultron connects MCP servers itself and reads the same mcp.json. The adapter was not loaded; remove it with `ultron remove npm:pi-mcp-adapter`.";

/**
 * Leave out pi-mcp-adapter when the built-in MCP extension is loaded, and the built-in one when another extension
 * registers the `mcp` tool. `adapterSkipped` is true when an adapter was left out.
 */
export function preferNativeMcp(base: LoadExtensionsResult): { result: LoadExtensionsResult; adapterSkipped: boolean } {
	const builtin = base.extensions.find((extension) => extension.path === BUILTIN_MCP_EXTENSION_PATH);
	if (!builtin) return { result: base, adapterSkipped: false };
	const isAdapter = (path: string, resolvedPath: string) =>
		ADAPTER_PACKAGE.test(path) || ADAPTER_PACKAGE.test(resolvedPath);
	const adapters = base.extensions.filter((extension) => isAdapter(extension.path, extension.resolvedPath));
	const replaced = base.extensions.some(
		(extension) => extension !== builtin && !adapters.includes(extension) && extension.tools.has("mcp"),
	);
	const dropped = new Set(replaced ? [builtin] : adapters);
	if (dropped.size === 0) return { result: base, adapterSkipped: false };
	return {
		result: { ...base, extensions: base.extensions.filter((extension) => !dropped.has(extension)) },
		adapterSkipped: !replaced && adapters.length > 0,
	};
}
