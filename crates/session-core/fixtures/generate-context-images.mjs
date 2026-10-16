// Generates crates/session-core/fixtures/context_images.json: `contextImageBudget` (packages/coding-agent/src/ultron/
// image-budget.ts) for ULTRON_CONTEXT_IMAGES values, which it reads with JavaScript's `Number()` (hex, octal and
// binary literals included). The Rust test (crates/session-core/tests/context_images.rs) requires the same budgets.
// Usage: node crates/session-core/fixtures/generate-context-images.mjs   (from the repository root; deterministic)
import "../../cli/fixtures/offline-guard.mjs";
import { writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { contextImageBudget } from "../../../packages/coding-agent/src/ultron/image-budget.ts";

const here = dirname(fileURLToPath(import.meta.url));

const VALUES = [
	null,
	"",
	"   ",
	"8",
	"0",
	"3",
	" 12 ",
	"\t5\n",
	"0x10",
	"0X1f",
	"0x",
	"0x1g",
	"-0x10",
	"+0x10",
	"0o17",
	"0O7",
	"0o8",
	"0b101",
	"0B11",
	"0b2",
	"010",
	"1e1",
	"1E2",
	"1e",
	"1.0",
	"1.5",
	"5.",
	".0",
	"-1",
	"-0",
	"+3",
	"Infinity",
	"-Infinity",
	"inf",
	"NaN",
	"nan",
	"1_0",
	"1e400",
	"0x7fffffffffffffffffff",
	"abc",
	" 7 ",
	"﻿9",
];

const cases = VALUES.map((value) => ({
	value,
	budget: contextImageBudget(value === null ? {} : { ULTRON_CONTEXT_IMAGES: value }),
}));

writeFileSync(join(here, "context_images.json"), `${JSON.stringify({ cases }, null, "\t")}\n`);
