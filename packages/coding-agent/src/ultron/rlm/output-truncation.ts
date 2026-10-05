/**
 * Middle truncation of RLM tool results before they enter the conversation (nano-rlm's
 * `truncate_tool_output`): an oversized result keeps its head and tail, so the first error and the
 * final summary both survive, and a marker says how much was cut. The small budget pushes the model
 * to process data in Python and print only what it needs. With a spill directory the whole text is saved to a private
 * file named in the marker, so what was cut can be read back instead of recomputed.
 */
import { chmodSync, closeSync, mkdirSync, openSync, readdirSync, statSync, unlinkSync, writeSync } from "node:fs";
import { join } from "node:path";

/** nano-rlm's TOOL_OUTPUT_MAX_BYTES. */
export const DEFAULT_RLM_OUTPUT_BYTES = 20_000;
/**
 * Upper bound for ULTRON_RLM_OUTPUT_BYTES. The kernel reports each captured stream in one JSON
 * protocol frame (at most 1 MiB), and escaping can grow a byte up to six.
 */
export const MAX_RLM_OUTPUT_BYTES = 128 * 1024;

/** The byte budget for one RLM tool result: ULTRON_RLM_OUTPUT_BYTES, else 20 KB, within 1 KB..128 KiB. */
export function rlmOutputBudget(env: NodeJS.ProcessEnv = process.env): number {
	const raw = env.ULTRON_RLM_OUTPUT_BYTES?.trim();
	const value = raw ? Number(raw) : Number.NaN;
	if (!Number.isInteger(value) || value <= 0) return DEFAULT_RLM_OUTPUT_BYTES;
	return Math.min(Math.max(value, 1024), MAX_RLM_OUTPUT_BYTES);
}

/** Spill files kept per directory (the kernel's own stream spills share it and the same bound). */
export const SPILL_KEEP = 20;

/**
 * Save `text` to a new private file (0600 in a 0700 directory) in `dir`, dropping the oldest beyond SPILL_KEEP; the
 * path, or undefined when it cannot be written.
 */
export function spillOutput(text: string, dir: string, name: string): string | undefined {
	try {
		mkdirSync(dir, { recursive: true, mode: 0o700 });
		chmodSync(dir, 0o700);
		const files = readdirSync(dir)
			.filter((file) => file.endsWith(".txt"))
			.map((file) => ({ path: join(dir, file), mtime: statSync(join(dir, file)).mtimeMs }))
			.sort((a, b) => a.mtime - b.mtime);
		for (const file of files.slice(0, Math.max(0, files.length - (SPILL_KEEP - 1)))) unlinkSync(file.path);
		const path = join(dir, `${process.hrtime.bigint()}-${process.pid}-${name}.txt`);
		const fd = openSync(path, "wx", 0o600);
		try {
			writeSync(fd, text);
		} finally {
			closeSync(fd);
		}
		return path;
	} catch {
		return undefined;
	}
}

/**
 * Keep the head and tail of `text` within `maxBytes` (UTF-8) and say what was elided; with `spillDir`, the whole text is
 * saved there first and the marker names the file.
 */
export function truncateToolOutput(text: string, maxBytes: number = rlmOutputBudget(), spillDir?: string): string {
	const data = Buffer.from(text, "utf8");
	if (data.length <= maxBytes) return text;
	const saved = spillDir === undefined ? undefined : spillOutput(text, spillDir, "cell");
	const keep = Math.floor(maxBytes / 2);
	// A cut inside a multi-byte character drops that character rather than adding a replacement glyph.
	const head = new TextDecoder("utf-8").decode(data.subarray(0, keep)).replace(/�$/, "");
	const tail = new TextDecoder("utf-8").decode(data.subarray(data.length - keep)).replace(/^�/, "");
	const lines = text.split("\n").length;
	const elided = data.length - 2 * keep;
	return [
		`Warning: truncated output (${data.length} bytes, ${lines} lines; the middle ${elided} bytes were cut). Process large data in Python and print only what you need.`,
		"",
		head,
		`[... ${elided} bytes truncated${saved === undefined ? "" : `; full output in ${saved}`} ...]`,
		tail,
	].join("\n");
}
