// Records crates/ai/fixtures/claude-usage.json: Claude Code's `rate_limit_event` read as subscription usage
// (`rateLimitsFromEvent`) and the `claude_code_usage` diagnostic built from it (`usageDiagnostic`, timestamp
// fixed at 0), from packages/coding-agent/src/ultron/claude/root-runner.ts, for unified windows, the event's own
// window as a fallback, invalid numbers and missing info. crates/ai/tests/claude_subscription_usage.rs replays it.
// Usage: node crates/ai/fixtures/generate-claude-usage.mjs
import "../../cli/fixtures/offline-guard.mjs";
import { writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const { rateLimitsFromEvent, usageDiagnostic } = await import(
	join(here, "../../../packages/coding-agent/src/ultron/claude/root-runner.ts")
);
Date.now = () => 0;

const events = [
	{ type: "rate_limit_event", rate_limit_info: { status: "allowed", unifiedWindows: { five_hour: { utilization: 0.09, resetsAt: 1790700600 }, seven_day: { utilization: 0.62 } } } },
	{ type: "rate_limit_event", rate_limit_info: { status: "rejected", rateLimitType: "five_hour", utilization: 1, resetsAt: 1790700600 } },
	{ type: "rate_limit_event", rate_limit_info: { status: "allowed_warning", rateLimitType: "seven_day_opus", utilization: 0.8, unifiedWindows: { five_hour: { utilization: 0.5 } } } },
	{ type: "rate_limit_event", rate_limit_info: { rateLimitType: "five_hour", utilization: 0.4, unifiedWindows: { five_hour: { utilization: 0.1 } } } },
	{ type: "rate_limit_event", rate_limit_info: { status: 3, unifiedWindows: { a: { utilization: -1, resetsAt: "x" }, b: null, c: [1], d: { utilization: 2 } } } },
	{ type: "rate_limit_event", rate_limit_info: { unifiedWindows: [{ utilization: 0.3 }] } },
	{ type: "rate_limit_event", rate_limit_info: {} },
	{ type: "rate_limit_event", rate_limit_info: [] },
	{ type: "rate_limit_event", rate_limit_info: null },
	{ type: "rate_limit_event" },
];
const cases = events.map((event) => {
	const limits = rateLimitsFromEvent(event);
	return { event, limits: limits ?? null, diagnostic: limits ? usageDiagnostic(limits) : null };
});
writeFileSync(join(here, "claude-usage.json"), `${JSON.stringify({ cases }, null, 1)}\n`);
console.log(`wrote ${cases.length} cases`);
