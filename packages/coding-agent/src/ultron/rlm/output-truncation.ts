/**
 * Middle truncation of RLM tool results before they enter the conversation (nano-rlm's
 * `truncate_tool_output`): an oversized result keeps its head and tail, so the first error and the
 * final summary both survive, and a marker says how much was cut. The small budget pushes the model
 * to process data in Python and print only what it needs.
 */

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

/** Keep the head and tail of `text` within `maxBytes` (UTF-8) and say what was elided. */
export function truncateToolOutput(text: string, maxBytes: number = rlmOutputBudget()): string {
	const data = Buffer.from(text, "utf8");
	if (data.length <= maxBytes) return text;
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
		`[... ${elided} bytes truncated ...]`,
		tail,
	].join("\n");
}
