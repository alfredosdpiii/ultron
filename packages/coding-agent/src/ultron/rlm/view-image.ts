/**
 * `view_image` for the RLM REPL: the kernel skill names an image file (a path, or a temporary file it wrote for
 * bytes, a PIL image or a matplotlib figure) in an `rlm.view_image` host request. The host reads and normalizes it
 * the way Pi's `read` tool does (magic-byte type detection, conversion, auto-resize to the model's limits) and keeps
 * it with the running cell; the rlm tool result then carries it as an image content block next to the text. The
 * image never travels as printed base64, so the output truncation cannot mangle it and it costs no text tokens.
 */

import { readFile, stat } from "node:fs/promises";
import type { Api, ImageContent, Model, ModelImageResizeOptions } from "@ultron/ai";
import { getImageDimensions } from "@ultron/tui";
import { processImage } from "../../utils/image-process.ts";
import { detectSupportedImageMimeType } from "../../utils/mime.ts";

/** The host request type the kernel's `view_image` sends. */
export const VIEW_IMAGE_REQUEST = "rlm.view_image";
/** Images one cell may attach to its tool result. */
export const MAX_IMAGES_PER_CELL = 8;
/** Largest source file view_image reads; bigger images are refused before decoding. */
export const MAX_VIEW_IMAGE_SOURCE_BYTES = 64 * 1024 * 1024;
/** `detail="low"` caps the long side at this many pixels. */
const LOW_DETAIL_MAX_SIDE = 512;

/** How a cell's images are normalized: Pi's `images.autoResize` setting and the current model. */
export interface ViewImageOptions {
	readonly autoResizeImages?: boolean;
	/** The model the tool result goes to; its resize profile and `input` decide resizing and the non-vision note. */
	readonly model?: Model<Api>;
}

/** The images one cell attached, in call order. */
export class CellImages {
	readonly images: ImageContent[] = [];
	readonly options: ViewImageOptions;

	constructor(options: ViewImageOptions = {}) {
		this.options = options;
	}

	/** Load, normalize and keep one image; returns the kernel's reply (a short description and, if any, a note). */
	async attach(payload: Record<string, unknown>): Promise<{ description: string; note?: string }> {
		if (this.images.length >= MAX_IMAGES_PER_CELL) {
			throw new Error(`view_image: at most ${MAX_IMAGES_PER_CELL} images per cell; view the rest in a later cell`);
		}
		const path = payload.path;
		if (typeof path !== "string" || path.length === 0) throw new Error("view_image: path must be a non-empty string");
		const size = (await stat(path)).size;
		if (size > MAX_VIEW_IMAGE_SOURCE_BYTES) {
			throw new Error(
				`view_image: ${formatBytes(size)} is over the ${formatBytes(MAX_VIEW_IMAGE_SOURCE_BYTES)} limit`,
			);
		}
		const bytes = await readFile(path);
		const mimeType = detectSupportedImageMimeType(bytes);
		if (!mimeType) throw new Error("view_image: not a supported image (png, jpeg, gif or webp, detected by content)");
		const original = getImageDimensions(bytes.toString("base64"), mimeType);
		const detail = typeof payload.detail === "string" ? payload.detail : undefined;
		const processed = await processImage(bytes, mimeType, {
			autoResizeImages: this.options.autoResizeImages ?? true,
			resizeOptions: resizeOptionsFor(this.options.model, detail),
		});
		if (!processed.ok) throw new Error(`view_image: ${processed.message.replace(/^\[|\]$/g, "")}`);
		this.images.push({ type: "image", data: processed.data, mimeType: processed.mimeType });
		const shown = getImageDimensions(processed.data, processed.mimeType);
		const shownBytes = Buffer.from(processed.data, "base64").byteLength;
		let description = `image ${this.images.length}: ${shown ? `${shown.widthPx}x${shown.heightPx} ` : ""}${formatName(processed.mimeType)}, ${formatBytes(shownBytes)}`;
		if (original && shown && (original.widthPx !== shown.widthPx || original.heightPx !== shown.heightPx)) {
			description += ` (downscaled from ${original.widthPx}x${original.heightPx})`;
		} else if (processed.mimeType !== mimeType) {
			description += ` (converted from ${formatName(mimeType)})`;
		}
		const note = nonVisionNote(this.options.model);
		return note === undefined ? { description } : { description, note };
	}

	/** The tool result content for this cell: the text, then one image block per attached image. */
	content(text: string): Array<{ type: "text"; text: string } | ImageContent> {
		if (this.images.length === 0) return [{ type: "text", text }];
		const note = nonVisionNote(this.options.model);
		const withNote = note === undefined ? text : `${text}\n${note}`;
		return [{ type: "text", text: withNote }, ...this.images];
	}
}

/** Pi's read tool note, said once per cell when the current model takes no image input. */
export function nonVisionNote(model: Model<Api> | undefined): string | undefined {
	if (!model || model.input.includes("image")) return undefined;
	return `[view_image: the current model (${model.provider}/${model.id}) does not accept image input, so attached images are omitted from its request. Inspect images with Python instead (for example PIL for size and pixel colors).]`;
}

function resizeOptionsFor(
	model: Model<Api> | undefined,
	detail: string | undefined,
): ModelImageResizeOptions | undefined {
	const base = model?.inputLimits?.images?.resize;
	if (detail !== "low") return base;
	return {
		...base,
		maxWidth: Math.min(base?.maxWidth ?? LOW_DETAIL_MAX_SIDE, LOW_DETAIL_MAX_SIDE),
		maxHeight: Math.min(base?.maxHeight ?? LOW_DETAIL_MAX_SIDE, LOW_DETAIL_MAX_SIDE),
	};
}

function formatName(mimeType: string): string {
	return (mimeType.split("/")[1] ?? mimeType).toUpperCase();
}

function formatBytes(bytes: number): string {
	if (bytes < 1024) return `${bytes} B`;
	if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
	return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}
