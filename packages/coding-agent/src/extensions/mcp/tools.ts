/**
 * MCP tool results as tool content. Ported from Pi 1.0.0's MCP extension, without the model-tool definitions:
 * in Ultron MCP tools are not model tools, the REPL reaches them through the `mcp` gateway (see index.ts).
 */

import { randomBytes } from "node:crypto";
import { writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { ImageContent, TextContent } from "@ultron/ai";
import { type CallToolResult, type ContentBlock, type McpRequestOptions, toLlmContent } from "@ultron/mcp";
import { formatSize } from "../../core/tools/truncate.ts";

/** Saves a binary resource and returns the file path. `extension` includes the dot, for example `.bin`. */
export type McpOutputSaver = (data: string | Uint8Array, extension: string) => Promise<string>;

export async function saveToTempFile(data: string | Uint8Array, extension: string): Promise<string> {
	const path = join(tmpdir(), `ultron-mcp-${randomBytes(8).toString("hex")}${extension}`);
	// Results can carry private data, so only the user may read the file.
	await writeFile(path, data, { mode: 0o600 });
	return path;
}

export interface McpToolCaller {
	callTool(name: string, args: Record<string, unknown>, options: McpRequestOptions): Promise<CallToolResult>;
}

export function textOf(content: readonly (TextContent | ImageContent)[]): string {
	return content
		.filter((block): block is TextContent => block.type === "text")
		.map((block) => block.text)
		.join("\n");
}

export interface ConvertMcpResultOptions {
	/** Saves binary resources. Default: a temp file. */
	saveOutput?: McpOutputSaver;
	/** Whether the server's resources can be read with `mcp.read`, which resource links then name. */
	readableResources?: boolean;
}

/** File extension for a saved binary resource: the one its URI ends in, else `.bin`. */
function extensionOf(uri: string): string {
	const path = URL.canParse(uri) ? new URL(uri).pathname : uri;
	return /\.[A-Za-z0-9]{1,8}$/.exec(path)?.[0] ?? ".bin";
}

/** Blobs of these types are shown as text. */
function isTextMimeType(mimeType: string | undefined): boolean {
	if (!mimeType) return false;
	const type = mimeType.split(";", 1)[0].trim().toLowerCase();
	return type.startsWith("text/") || type === "application/json" || type.endsWith("+json") || type.endsWith("+xml");
}

/** Model-facing content of one block of `server`'s result. */
async function blockToContent(
	server: string,
	block: ContentBlock,
	options: ConvertMcpResultOptions,
): Promise<(TextContent | ImageContent)[]> {
	if (block.type === "resource_link") {
		const details = [block.mimeType, block.size === undefined ? undefined : formatSize(block.size)].filter(Boolean);
		const read = options.readableResources ? `. Read it with await mcp.read("${server}", uri)` : "";
		const description = block.description ? `: ${block.description}` : "";
		return [
			{
				type: "text",
				text: `[Resource ${block.uri} "${block.title ?? block.name}"${details.length > 0 ? ` (${details.join(", ")})` : ""}${description}${read}]`,
			},
		];
	}
	if (block.type === "resource" && "blob" in block.resource && !block.resource.mimeType?.startsWith("image/")) {
		const { uri, mimeType, blob } = block.resource;
		const data = Buffer.from(blob, "base64");
		if (isTextMimeType(mimeType)) return [{ type: "text", text: data.toString("utf8") }];
		const kind = `${mimeType ?? "unknown type"}, ${formatSize(data.length)}`;
		try {
			const path = await (options.saveOutput ?? saveToTempFile)(data, extensionOf(uri));
			return [{ type: "text", text: `[Binary resource ${uri} (${kind}) saved to ${path}]` }];
		} catch (error) {
			const reason = error instanceof Error ? error.message : String(error);
			return [{ type: "text", text: `[Binary resource ${uri} (${kind}) could not be saved: ${reason}]` }];
		}
	}
	return toLlmContent({ content: [block] });
}

/** Model-facing content of `server`'s content blocks, before the output limit. */
export async function toModelContent(
	server: string,
	blocks: readonly ContentBlock[],
	options: ConvertMcpResultOptions = {},
): Promise<(TextContent | ImageContent)[]> {
	return (await Promise.all(blocks.map((block) => blockToContent(server, block, options)))).flat();
}

/**
 * Content of an MCP result for the REPL. The text is not cut here: the kernel bounds what a cell prints, and the
 * value itself is data for the program. Without content blocks the structured content is returned as JSON.
 */
export async function convertMcpResult(
	server: string,
	tool: string,
	result: CallToolResult,
	options: ConvertMcpResultOptions = {},
): Promise<{ content: (TextContent | ImageContent)[]; isError: boolean }> {
	const content: (TextContent | ImageContent)[] =
		result.content.length > 0 ? await toModelContent(server, result.content, options) : toLlmContent(result);
	if (result.isError && textOf(content) === "") {
		content.push({ type: "text", text: `MCP tool ${server}/${tool} returned an error` });
	}
	return { content, isError: result.isError === true };
}
