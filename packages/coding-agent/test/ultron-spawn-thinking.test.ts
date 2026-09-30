/**
 * `rlm.spawn(brief, name=..., thinking=...)`: the host accepts the thinking level Python's rlm.spawn allows and sets
 * it on the subagent's lane; an unknown level is refused.
 */
import { describe, expect, test } from "vitest";
import { hostFixture, waitFor } from "./ultron-host-fixtures.ts";

describe("rlm.spawn thinking", () => {
	test("sets the child lane's thinking level", async () => {
		const fixture = hostFixture({ script: () => "done" });
		await fixture.call("rlm.spawn", { prompt: "brief", kwargs: { name: "c", thinking: "low" } });
		await waitFor(() => fixture.thinkingLevels.length === 1);
		expect(fixture.thinkingLevels[0]?.level).toBe("low");
	});

	test("leaves the lane's level alone without thinking=", async () => {
		const fixture = hostFixture({ script: () => "done" });
		await fixture.call("rlm.spawn", { prompt: "brief", kwargs: { name: "c" } });
		await waitFor(() => fixture.prompts.length === 1);
		expect(fixture.thinkingLevels).toEqual([]);
	});

	test("refuses an unknown level", async () => {
		const fixture = hostFixture({ script: () => "done" });
		await expect(
			fixture.call("rlm.spawn", { prompt: "brief", kwargs: { name: "c", thinking: "extreme" } }),
		).rejects.toThrow(/thinking must be one of off, minimal, low, medium, high, xhigh, max/);
	});
});
