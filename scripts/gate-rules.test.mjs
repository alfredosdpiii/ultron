import assert from "node:assert/strict";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { allRowsPassed, rowRange } from "./acceptance-report.mjs";
import { freePath } from "./eval-quality.mjs";

test("the gate requires every manifest row, not a fixed count", () => {
	assert.equal(allRowsPassed({ total: 55, passed: 55 }), true);
	// 46 passing rows once counted as complete; with 55 rows that must fail.
	assert.equal(allRowsPassed({ total: 55, passed: 46 }), false);
	assert.equal(allRowsPassed({ total: 0, passed: 0 }), false);
});

test("the report labels its row range from the rows it has", () => {
	assert.equal(rowRange([{ id: "A01" }, { id: "A55" }]), "A01-A55");
	assert.equal(rowRange([]), "no rows");
});

test("an eval result never overwrites an earlier one from the same day", () => {
	const dir = mkdtempSync(join(tmpdir(), "ultron-free-path-"));
	try {
		const first = join(dir, "2026-09-26-hard-model.json");
		assert.equal(freePath(first), first);
		writeFileSync(first, "{}");
		assert.equal(freePath(first), join(dir, "2026-09-26-hard-model-2.json"));
		writeFileSync(join(dir, "2026-09-26-hard-model-2.json"), "{}");
		assert.equal(freePath(first), join(dir, "2026-09-26-hard-model-3.json"));
	} finally {
		rmSync(dir, { recursive: true, force: true });
	}
});
