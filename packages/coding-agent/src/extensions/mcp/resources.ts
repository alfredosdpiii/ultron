/**
 * MCP resources. Ported from Pi 1.0.0's MCP extension, without the model-tool definitions: the REPL lists and
 * reads resources through the `mcp` gateway (see index.ts).
 */

import type {
	ListResourcesResult,
	ListResourceTemplatesResult,
	McpRequestOptions,
	ReadResourceResult,
	Resource,
	ResourceTemplate,
} from "@ultron/mcp";

/** A connected server that offers resources. */
export interface McpResourceServer {
	name: string;
	timeoutMs: number;
	resourcesPage(cursor: string | undefined, options: McpRequestOptions): Promise<ListResourcesResult>;
	resourceTemplatesPage(cursor: string | undefined, options: McpRequestOptions): Promise<ListResourceTemplatesResult>;
	allResources(options: McpRequestOptions): Promise<Resource[]>;
	allResourceTemplates(options: McpRequestOptions): Promise<ResourceTemplate[]>;
	readResource(uri: string, options: McpRequestOptions): Promise<ReadResourceResult>;
}

/** MCP App user interfaces, which only hosts that render them can use. */
export function isMcpAppResource(item: { uri?: string; uriTemplate?: string; mimeType?: string }): boolean {
	const uri = item.uri ?? item.uriTemplate ?? "";
	return uri.startsWith("ui://") || /;\s*profile\s*=\s*"?mcp-app"?/i.test(item.mimeType ?? "");
}

/** A listed resource or template without `_meta` and icons, tagged with its server. */
export function listed<T extends { _meta?: unknown }>(server: string, item: T): Record<string, unknown> {
	const { _meta, icons: _icons, ...rest } = item as T & { icons?: unknown };
	return { server, ...rest };
}
