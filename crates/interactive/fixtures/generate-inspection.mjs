// Generates crates/interactive/fixtures/inspection.json from packages/coding-agent's inspection commands
// (experimental/services/inspection-commands.ts; TypeScript, this checkout): every command (`/agents`, `/memory`,
// `/skills`, `/experiments`, `/goals`, `/progress`) run with several argument texts against a recording session
// control, which answers each request with the case's value; the record is the request it made (or the usage
// error it threw) and the status text it showed (`formatInspection`). Plus `formatInspection` over values that reach
// every branch: scalars, JS number forms, empty containers, nesting, array-index key order, and the 40-line bound.
// crates/interactive/tests/inspection.rs replays every case.
// Usage: node crates/interactive/fixtures/generate-inspection.mjs   (deterministic; output is written next to this file)
import { writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const { inspectionCommands, formatInspection } = await import(
	join(here, "../../../packages/coding-agent/src/experimental/services/inspection-commands.ts")
);

const answer = {
	tasks: [{ id: "task-1", definition: "rlm-frame@1", state: "completed" }],
	usage: { modelCalls: 2, cost: 0.0125 },
};

const commands = [];
const requests = [];
const control = {
	async inspect(type, payload) {
		requests.push({ type, payload });
		return answer;
	},
};
let shown = "";
const ui = {
	showStatus(text) {
		shown = text;
	},
};
for (const command of inspectionCommands(control, ui)) {
	for (const args of ["", " ", "list", "why", "why task-a", " why  task-a ", "list extra", "task-9", "a b"]) {
		requests.length = 0;
		shown = "";
		let error = null;
		try {
			await command.run(args, {});
		} catch (thrown) {
			error = thrown.message;
		}
		commands.push({ name: command.name, args, requests: [...requests], shown, error });
	}
}

const many = Object.fromEntries(Array.from({ length: 45 }, (_, index) => [`key${index}`, index]));
const values = [
	null,
	true,
	0,
	-0,
	1.5,
	1e21,
	123456789012,
	0.000001,
	1e-7,
	-2.25,
	"plain",
	"line one\nline two",
	"é—ü",
	[],
	{},
	[1, "two", null, false],
	[[], {}, [1, [2, []]], { a: [] }],
	{ b: 1, a: { c: [], d: {} }, 10: "ten", 2: "two", x: [{ y: 1, z: [3, 4] }, "s"] },
	[{ id: "m1", kind: "forget", state: "forgotten", memoryId: "doc-1", textHash: "abc" }],
	many,
	Array.from({ length: 50 }, (_, index) => index),
	{ nested: { deeper: { deepest: { list: [1, { k: "v" }] } } } },
	answer,
];
const formats = values.map((value, index) => ({ title: `/case ${index}`, value, text: formatInspection(`/case ${index}`, value) }));

const out = { commands, formats };
writeFileSync(join(here, "inspection.json"), `${JSON.stringify(out, null, "\t")}\n`);
