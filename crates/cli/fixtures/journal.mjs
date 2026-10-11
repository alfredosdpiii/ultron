// The format-4 journal normalization the CLI fixtures share (print_mode.mjs, rpc_mode.mjs): the Rust tests apply
// the same function (tests/common `normalize_journal`).

const UUID = /[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}/g;
const TIME_KEYS = new Set(["timestamp", "createdAt", "startedAt", "endedAt", "lastAt", "since", "updatedAt", "requestedAt"]);

/** Every UUID-shaped string renamed `00000000-0000-7000-8000-<n>` in order of first appearance. */
export function renameIds(text, ids = new Map()) {
	return text.replace(UUID, (id) => {
		if (!ids.has(id)) ids.set(id, `00000000-0000-7000-8000-${String(ids.size + 1).padStart(12, "0")}`);
		return ids.get(id);
	});
}

export function zeroTimes(value) {
	if (Array.isArray(value)) return value.map(zeroTimes);
	if (value === null || typeof value !== "object") return value;
	return Object.fromEntries(Object.entries(value).map(([k, v]) => [k, TIME_KEYS.has(k) && typeof v === "number" ? 0 : zeroTimes(v)]));
}

/**
 * A format-4 journal as compared across implementations: transactions of records, without the records only one
 * side writes (Ultron module documents, the timing-dependent progress frames, the Rust build's automatic names and
 * their naming usage), sequences renumbered over what is left, ids renamed in order of appearance, times zeroed.
 * The Rust test applies the same function (tests/print_mode.rs `normalize_journal`).
 */
export function filteredJournal(text) {
	const lines = text.split("\n").filter(Boolean);
	const header = JSON.parse(lines[0]);
	let transactions = lines.slice(1).map((line) => [JSON.parse(line)].flat());
	transactions = transactions
		.map((records) => {
			const autoName = records.some((r) => r.namespace === "ultron.module" && r.key === "session-name");
			return records.filter(
				(r) =>
					r.namespace !== "ultron.module" &&
					r.namespace !== "pi.pending.assistant_frame" &&
					!(autoName && r.namespace === "pi.session.name") &&
					!(r.kind === "usage" && r.details?.source === "session-naming"),
			);
		})
		.filter((records) => records.length > 0);
	const seqs = transactions.flat().map((r) => r.seq);
	const renumber = new Map(seqs.map((seq, i) => [seq, i + 1]));
	for (const r of transactions.flat()) r.seq = renumber.get(r.seq);
	if (header.nextSeq !== undefined) header.nextSeq = seqs.filter((seq) => seq < header.nextSeq).length + 1;
	return { header, transactions };
}

export function normalizeJournal(text) {
	return zeroTimes(JSON.parse(renameIds(JSON.stringify(filteredJournal(text)))));
}

