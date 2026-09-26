/**
 * The runtime line under Pi's footer: the RLM summary and Jev's presence side by side on one line. Both are
 * optional; when they do not fit together, Jev keeps a bounded share and the RLM summary is truncated first.
 */
import { truncateToWidth, visibleWidth } from "@ultron/tui";

/** Renders one part at a given width (undefined when it has nothing to show). */
export type StatusPart = (width: number) => string | undefined;

/** Wide enough to render a part at its natural length. */
const NATURAL = 400;

export function combineRuntimeStatus(
	parts: { readonly rlm?: StatusPart; readonly jev?: StatusPart },
	width: number,
	separator = " │ ",
): string | undefined {
	const bound = Math.max(1, width);
	const rlm = parts.rlm?.(NATURAL);
	const jev = parts.jev?.(NATURAL);
	if (rlm === undefined && jev === undefined) return undefined;
	if (rlm === undefined) return parts.jev!(bound);
	if (jev === undefined) return parts.rlm!(bound);
	const gap = visibleWidth(separator);
	const rlmWidth = visibleWidth(rlm);
	const jevWidth = visibleWidth(jev);
	if (rlmWidth + gap + jevWidth <= bound) return `${rlm}${separator}${jev}`;
	// Jev keeps up to 40% (at least 16 columns); the RLM summary gets the rest.
	const jevRoom = Math.min(jevWidth, Math.max(16, Math.floor(bound * 0.4)));
	const rlmRoom = bound - jevRoom - gap;
	if (rlmRoom < 12) return parts.rlm!(bound);
	const left = parts.rlm!(rlmRoom) ?? "";
	const right = parts.jev!(jevRoom) ?? "";
	return truncateToWidth(`${left}${separator}${right}`, bound, "…");
}
