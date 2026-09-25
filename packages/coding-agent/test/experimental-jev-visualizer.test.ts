import { visibleWidth } from "@earendil-works/pi-tui";
import { describe, expect, test } from "vitest";
import {
	type JevSnapshot,
	parseJevDecisions,
	parseJevLedger,
	renderJevPanel,
	renderJevStatusLine,
} from "../src/experimental/jev-visualizer.ts";
import type { JevMemoryGate, JevMemoryPolicy, JevTriage } from "../src/ultron/jev.ts";
import { JevDecisionLog, recordingJevClient } from "../src/ultron/jev-decisions.ts";

const NOW = 10_000_000;

function fixture(overrides: Partial<JevSnapshot> = {}): JevSnapshot {
	return {
		now: NOW,
		available: { jev: true, hindsight: false },
		ledgerCalls: 4,
		inFlight: 1,
		decisions: [
			{
				at: NOW - 30_000,
				kind: "recall",
				status: "ok",
				retrieve: true,
				probability: 0.71,
				durationMs: 90,
				inputSha256: "bbbbbbbbbbbb",
			},
			{
				at: NOW - 45_000,
				kind: "triage",
				status: "ok",
				route: "powerful",
				routeConfidence: 0.82,
				category: "debugging",
				complexity: 1.4,
				urgency: "normal",
				durationMs: 120,
				inputSha256: "aaaaaaaaaaaa",
			},
			{ at: NOW - 12_000, kind: "retain", status: "ok", action: "keep", confidence: 0.9, durationMs: 80 },
			{ at: NOW - 5000, kind: "triage", status: "error", reason: "Jev UNAVAILABLE", durationMs: 5000 },
		],
		...overrides,
	};
}

describe("Jev visualizer", () => {
	test("renders availability and decisions oldest first", () => {
		const lines = renderJevPanel(fixture(), 120);
		expect(lines).toEqual([
			"Jev 2 triage · 1 recall (1 retrieved) · 1 retain · 1 failed · 4 ledger calls · 1 in flight",
			"jev ✓ configured · hindsight ✗ not configured (set ULTRON_HINDSIGHT_URL)",
			"  45s ago ◇ triage → powerful 82% · debugging · cx 1.4 · normal 120ms #aaaaaa",
			"  30s ago ✓ recall retrieve p=0.71 90ms #bbbbbb",
			"  12s ago + retain keep 90% 80ms",
			" 5.0s ago ✗ triage failed: Jev UNAVAILABLE 5.0s",
		]);
	});

	test("shows unconfigured Jev clearly instead of an empty panel", () => {
		const lines = renderJevPanel(fixture({ available: { jev: false, hindsight: false }, decisions: [] }), 120);
		expect(lines[0]).toContain("not configured · no decisions");
		expect(lines[1]).toContain("jev ✗ not configured (set TYPESAFE_API_KEY)");
		const unknown = renderJevPanel(fixture({ available: null, decisions: [] }), 120);
		expect(unknown[1]).toBe("decision log unavailable in this session");
		const unavailable = renderJevPanel(
			fixture({
				available: { jev: false, hindsight: false },
				decisions: [{ at: NOW - 1000, kind: "triage", status: "unavailable", reason: "Jev is not configured" }],
			}),
			120,
		);
		expect(unavailable.at(-1)).toBe(" 1.0s ago – triage unavailable: Jev is not configured");
	});

	test("bounds rows and width", () => {
		const decisions = Array.from({ length: 30 }, (_, index) => ({
			at: NOW - (30 - index) * 1000,
			kind: "recall",
			status: "ok",
			retrieve: index % 2 === 0,
			probability: 0.5,
		}));
		const lines = renderJevPanel(fixture({ decisions }), 100, { maxRows: 5 });
		expect(lines[2]).toBe("+25 earlier");
		expect(lines).toHaveLength(8);
		for (const width of [6, 24, 40]) {
			for (const line of renderJevPanel(fixture(), width)) expect(visibleWidth(line)).toBeLessThanOrEqual(width);
		}
	});

	test("collapsed line appears only for recent activity", () => {
		expect(renderJevStatusLine(fixture(), 200)).toBe(
			"Jev ▸ 2 triage · 1 recall (1 retrieved) · 1 retain · 1 failed · 4 ledger calls · 1 in flight · last triage failed: Jev UNAVAILABLE",
		);
		const stale = fixture({ inFlight: 0, decisions: [{ at: NOW - 3_600_000, kind: "triage", status: "ok" }] });
		expect(renderJevStatusLine(stale, 200)).toBeUndefined();
		expect(renderJevStatusLine(fixture({ decisions: [], inFlight: 0 }), 200)).toBeUndefined();
	});

	test("parses inspection payloads defensively", () => {
		expect(parseJevDecisions(null)).toEqual({ available: null, decisions: [] });
		expect(
			parseJevDecisions({
				available: { jev: true, hindsight: 1 },
				decisions: [{ at: 1, kind: "triage", route: "fast", prompt: "never shown" }, { kind: "bad" }],
			}),
		).toEqual({
			available: { jev: true, hindsight: false },
			decisions: [{ at: 1, kind: "triage", status: "ok", route: "fast" }],
		});
		expect(
			parseJevLedger({ usage: { usage: { jevCalls: 3 }, reservations: [{ kind: "jev" }, { kind: "task" }] } }),
		).toEqual({ ledgerCalls: 3, inFlight: 1 });
	});
});

describe("Jev decision log", () => {
	const triage: JevTriage = {
		route: "architecture",
		routeConfidence: 0.6,
		complexity: 2,
		complexityConfidence: 0.5,
		urgency: "low",
		category: "architecture",
	};

	function client() {
		return {
			triage: async () => triage,
			memoryGate: async (): Promise<JevMemoryGate> => ({ retrieve: false, probability: 0.2 }),
			memoryRecall: async (): Promise<JevMemoryGate> => ({ retrieve: true, probability: 0.9 }),
			memoryPolicy: async (): Promise<JevMemoryPolicy> => {
				throw new Error("Jev ABORTED");
			},
		};
	}

	test("records decisions without inputs, persists them, and keeps the newest", async () => {
		let stored: unknown;
		let clock = 100;
		const store = {
			read: async () => stored as never,
			write: async (document: unknown) => {
				stored = document;
			},
		};
		const log = new JevDecisionLog(store, { capacity: 3, now: () => clock++ });
		const jev = recordingJevClient(client(), log);
		await expect(jev.triage("secret prompt sk-abcdefghijklmnop")).resolves.toEqual(triage);
		await jev.memoryGate("recall me");
		await jev.memoryRecall("recall me again");
		await expect(jev.memoryPolicy("keep this", "")).rejects.toThrow("Jev ABORTED");
		const decisions = await log.list();
		expect(decisions.map((decision) => [decision.kind, decision.status])).toEqual([
			["recall", "ok"],
			["recall", "ok"],
			["retain", "error"],
		]);
		expect(decisions[2]!.reason).toBe("Jev ABORTED");
		expect(JSON.stringify(stored)).not.toContain("secret prompt");
		expect(JSON.stringify(stored)).not.toContain("recall me");
		expect(decisions[0]!.inputSha256).toMatch(/^[a-f0-9]{12}$/);
		// A new log over the same store reloads the durable ring.
		const reopened = new JevDecisionLog(store, { capacity: 3 });
		expect((await reopened.list()).map((decision) => decision.id)).toEqual(decisions.map((decision) => decision.id));
	});

	test("a failing store never fails the Jev call", async () => {
		const log = new JevDecisionLog({
			read: async () => undefined,
			write: async () => {
				throw new Error("disk full");
			},
		});
		const jev = recordingJevClient(client(), log);
		await expect(jev.triage("x")).resolves.toEqual(triage);
		expect(await log.list()).toHaveLength(1);
	});
});
