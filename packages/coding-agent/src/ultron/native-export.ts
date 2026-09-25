import { readFile, writeFile } from "node:fs/promises";
import { basename, dirname, resolve } from "node:path";
import { BACKGROUND_CONTEXT, type Entry, JsonlSessionRepo } from "@ultron/agent-core";
import { NodeExecutionEnv } from "@ultron/agent-core/node";
import { APP_NAME } from "../config.ts";
import { generateHtml, type SessionData } from "../core/export-html/index.ts";
import type { SessionEntry } from "../core/session-manager.ts";
import { normalizePath } from "../utils/paths.ts";

export interface NativeSessionExportInput {
	readonly id: string;
	readonly cwd: string;
	readonly createdAt: number;
	/** Active-branch entries, oldest first. */
	readonly entries: readonly Entry[];
	readonly tipId: string | null;
}

/** True when the file starts with a native Ultron session header. */
export async function isNativeSessionFile(path: string): Promise<boolean> {
	try {
		const header: unknown = JSON.parse((await readFile(path, "utf8")).split("\n", 1)[0] ?? "");
		return typeof header === "object" && header !== null && "kind" in header && header.kind === "header";
	} catch {
		return false;
	}
}

/** Render native Session entries with Pi's HTML exporter. */
export async function writeNativeSessionHtml(
	input: NativeSessionExportInput,
	outputPath: string | undefined,
): Promise<string> {
	const target = outputPath ? normalizePath(outputPath) : `${APP_NAME}-session-${input.id}.html`;
	await writeFile(target, generateHtml(toSessionData(input)), "utf8");
	return target;
}

/** Export a native Session file's main branch as HTML. */
export async function exportNativeSessionFile(inputPath: string, outputPath: string | undefined): Promise<string> {
	const path = resolve(inputPath);
	// Session files live in <sessionsRoot>/<cwd-directory>/<file>.jsonl.
	const sessionsRoot = dirname(dirname(path));
	const fileSystem = new NodeExecutionEnv({ cwd: process.cwd() });
	const repo = new JsonlSessionRepo({ fileSystem, sessionsRoot });
	let session: Awaited<ReturnType<JsonlSessionRepo["open"]>> | undefined;
	try {
		const metadata = (await repo.list(undefined, BACKGROUND_CONTEXT)).find((candidate) => candidate.path === path);
		if (metadata === undefined) throw new Error(`Session is not in its session directory: ${path}`);
		session = await repo.open(metadata, BACKGROUND_CONTEXT);
		const main = await session.branch("main", BACKGROUND_CONTEXT);
		const entries = main === undefined ? [] : await main.findEntries({ order: "oldestFirst" }, BACKGROUND_CONTEXT);
		return await writeNativeSessionHtml(
			{
				id: metadata.id,
				cwd: metadata.cwd,
				createdAt: metadata.createdAt,
				entries,
				tipId: entries.at(-1)?.id ?? null,
			},
			outputPath ?? `${APP_NAME}-session-${basename(path, ".jsonl")}.html`,
		);
	} finally {
		await session?.close(BACKGROUND_CONTEXT);
		await repo.close(BACKGROUND_CONTEXT);
		await fileSystem.cleanup(BACKGROUND_CONTEXT);
	}
}

function toSessionData(input: NativeSessionExportInput): SessionData {
	return {
		header: { type: "session", version: 3, id: input.id, timestamp: iso(input.createdAt), cwd: input.cwd },
		entries: input.entries.flatMap(toSessionEntry),
		leafId: input.tipId,
	};
}

function toSessionEntry(entry: Entry): SessionEntry[] {
	const base = { id: entry.id, parentId: entry.parentId, timestamp: iso(entry.timestamp) };
	switch (entry.type) {
		case "message":
			return [{ ...base, type: "message", message: entry.message }];
		case "compaction":
			// Native compaction keeps its retained messages inline; earlier history stays on the branch.
			return [
				{
					...base,
					type: "compaction",
					summary: entry.summary,
					firstKeptEntryId: entry.id,
					tokensBefore: entry.tokensBefore,
					...(entry.details === undefined ? {} : { details: entry.details }),
					...(entry.usage === undefined ? {} : { usage: entry.usage }),
					fromHook: entry.fromHook,
				},
			];
		case "branch_summary":
			return [
				{
					...base,
					type: "branch_summary",
					fromId: entry.fromId ?? entry.parentId ?? entry.id,
					summary: entry.summary,
					...(entry.details === undefined ? {} : { details: entry.details }),
					...(entry.usage === undefined ? {} : { usage: entry.usage }),
					fromHook: entry.fromHook,
				},
			];
		case "custom":
			return [
				{
					...base,
					type: "custom",
					customType: entry.customType,
					...(entry.data === undefined ? {} : { data: entry.data }),
				},
			];
	}
}

function iso(timestamp: number): string {
	return new Date(timestamp).toISOString();
}
