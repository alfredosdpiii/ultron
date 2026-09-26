/**
 * Pi's editor-level actions for the native TUI, apart from the TUI class: model cycling in `enabledModels`
 * scope (Ctrl+P), the external editor (Ctrl+G), suspend (Ctrl+Z) and clipboard paste (Ctrl+V).
 */

import { writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { TUI } from "@ultron/tui";
import { APP_TITLE } from "../config.ts";
import { resolveModelScopeFromModels } from "../core/model-resolver.ts";
import { editInExternalEditor } from "../modes/interactive/external-editor.ts";
import { readClipboardText } from "../utils/clipboard.ts";
import { extensionForImageMimeType, readClipboardImage } from "../utils/clipboard-image.ts";
import type { ModelRef, ModelSummary } from "./services/models.ts";

/** The model Ctrl+P (1) or Shift+Ctrl+P (-1) moves to, as Pi's `cycleModel`, or why there is none. */
export function nextModel(
	available: readonly ModelSummary[],
	current: ModelRef | null | undefined,
	direction: 1 | -1,
	enabledPatterns: readonly string[] | undefined,
): { readonly model: ModelSummary } | { readonly error: string } {
	let candidates = available;
	let scoped = false;
	if (enabledPatterns !== undefined && enabledPatterns.length > 0) {
		const records = candidates.flatMap((candidate) => (candidate.model === undefined ? [] : [candidate.model]));
		const inScope = resolveModelScopeFromModels([...enabledPatterns], records).scopedModels;
		if (inScope.length > 0) {
			scoped = true;
			candidates = candidates.filter((candidate) =>
				inScope.some(
					(entry) => entry.model.provider === candidate.provider && entry.model.id === candidate.modelId,
				),
			);
		}
	}
	if (candidates.length <= 1) return { error: scoped ? "Only one model in scope" : "Only one model available" };
	const index = candidates.findIndex(
		(candidate) => candidate.provider === current?.provider && candidate.modelId === current?.modelId,
	);
	return { model: candidates[(index + direction + candidates.length) % candidates.length]! };
}

/** Pi's Ctrl+G: the TUI stops while `command` edits `content`; resolves the edited text, or undefined. */
export async function editExternally(ui: TUI, command: string, content: string): Promise<string | undefined> {
	ui.stop();
	try {
		const result = await editInExternalEditor({ command, content });
		return result.status === "complete" ? result.content : undefined;
	} finally {
		ui.start();
		ui.requestRender(true);
	}
}

/** Pi's Ctrl+Z: stop the TUI and suspend the process group; SIGCONT restores it. */
export function suspendTui(ui: TUI): void {
	if (process.platform === "win32") throw new Error("Suspend to background is not supported on Windows");
	// Keep the event loop alive and ignore SIGINT while suspended, as Pi does.
	const keepAlive = setInterval(() => {}, 2 ** 30);
	const ignoreSigint = (): void => {};
	process.on("SIGINT", ignoreSigint);
	const resume = (): void => {
		clearInterval(keepAlive);
		process.removeListener("SIGINT", ignoreSigint);
		ui.start();
		ui.requestRender(true);
	};
	process.once("SIGCONT", resume);
	try {
		ui.stop();
		process.kill(0, "SIGTSTP");
	} catch (error) {
		process.removeListener("SIGCONT", resume);
		resume();
		throw error;
	}
}

/**
 * Pi's Ctrl+V: a clipboard image is saved to a temp file and its path returned; otherwise the clipboard text.
 * Clipboard access can be unavailable (no display server, no permission), which yields undefined.
 */
export async function clipboardPasteText(): Promise<string | undefined> {
	try {
		const image = await readClipboardImage();
		if (image) {
			const extension = extensionForImageMimeType(image.mimeType) ?? "png";
			const filePath = join(tmpdir(), `${APP_TITLE}-clipboard-${crypto.randomUUID()}.${extension}`);
			writeFileSync(filePath, Buffer.from(image.bytes));
			return filePath;
		}
		return (await readClipboardText()) || undefined;
	} catch {
		return undefined;
	}
}
