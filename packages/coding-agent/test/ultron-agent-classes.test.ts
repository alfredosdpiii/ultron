import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { afterEach, describe, expect, test } from "vitest";
import { RlmKernel } from "../src/ultron/rlm/kernel.ts";
import { hostFixture, journal, memoryDefinitionStore } from "./ultron-host-fixtures.ts";

/**
 * Agents as Python classes (Phase 6, A55): a real Python kernel whose host requests go to a real
 * NativeRlmHost (definition registry, journal, validation) with scripted model lanes.
 */

const runtimePath = fileURLToPath(new URL("../src/ultron/rlm/runtime.py", import.meta.url));
const snapshotKey = Buffer.alloc(32, 5);
const cleanups: Array<() => Promise<void> | void> = [];

afterEach(async () => {
	for (const cleanup of cleanups.splice(0).reverse()) await cleanup();
});

type Reply = (method: string, input: { args: Record<string, unknown>; state?: Record<string, unknown> }) => string;

/** Scripted lane: answers by the method named in the derived instructions. */
function laneScript(reply: Reply) {
	return (_lane: string, prompt: string): string => {
		const method = /You implement the method `[A-Za-z_]+\.([A-Za-z_]+)\(/.exec(prompt)?.[1];
		const input = /\nInput data:\n(.*)\n\nOutput contract:\n/.exec(prompt)?.[1];
		if (!method || !input) throw new Error(`Unscripted prompt: ${prompt.slice(0, 120)}`);
		return reply(method, JSON.parse(input));
	};
}

function setup(reply: Reply, definitionStore = memoryDefinitionStore()) {
	const fixture = hostFixture({ script: laneScript(reply), definitionStore });
	const spillDir = mkdtempSync(join(tmpdir(), "ultron-agent-spill-"));
	const snapshotDir = mkdtempSync(join(tmpdir(), "ultron-agent-snap-"));
	cleanups.push(() => fixture.host.close());
	cleanups.push(() => rmSync(spillDir, { recursive: true, force: true }));
	cleanups.push(() => rmSync(snapshotDir, { recursive: true, force: true }));
	const previousSpill = process.env.ULTRON_RLM_SPILL_DIR;
	process.env.ULTRON_RLM_SPILL_DIR = spillDir;
	cleanups.push(() => {
		if (previousSpill === undefined) delete process.env.ULTRON_RLM_SPILL_DIR;
		else process.env.ULTRON_RLM_SPILL_DIR = previousSpill;
	});
	const kernel = () => {
		const instance = new RlmKernel({ cwd: process.cwd(), runtimePath, snapshotKey }, (type, payload, signal) =>
			fixture.host.handle(type, payload, { abortSignal: signal } as never),
		);
		cleanups.push(() => instance.shutdown());
		return instance;
	};
	return { fixture, kernel, spillDir, snapshotPath: join(snapshotDir, "main.snapshot") };
}

async function run(kernel: RlmKernel, code: string): Promise<string> {
	const result = await kernel.execute(code);
	if (result.status !== "ok") throw new Error(`cell failed: ${JSON.stringify(result.error)}\n${code}`);
	return result.stdout;
}

const TRIAGE = `
from dataclasses import dataclass
from typing import Literal

@dataclass
class Label:
    kind: Literal["bug", "feature"]
    confidence: float

@agent
class Triage:
    """TRIAGE ROLE: you label incoming reports."""
    seen: int = 0
    labels: list[str] = []

    async def classify(self, report: str) -> Label:
        """Label the report as a bug or a feature."""
        ...

    def record(self, label: Label) -> int:
        self.seen += 1
        self.labels.append(label.kind)
        return self.seen
`;

const labelReply: Reply = (method, input) => {
	if (method !== "classify") throw new Error(method);
	return JSON.stringify({ kind: String(input.args.report).includes("crash") ? "bug" : "feature", confidence: 0.9 });
};

describe("agent classes: schema derivation", () => {
	test("annotations become JSON schemas the host accepts", async () => {
		const { kernel } = setup(labelReply);
		const k = kernel();
		const out = await run(
			k,
			`import json, enum
from dataclasses import dataclass, field
from typing import Literal, Optional, TypedDict, NotRequired, Annotated, Any

class Color(enum.Enum):
    RED = "red"
    BLUE = "blue"

@dataclass
class Point:
    x: int
    y: float = 0.0
    tags: list[str] = field(default_factory=list)

class Movie(TypedDict):
    title: str
    year: NotRequired[int]

class FakeModel:  # duck-typed pydantic: schema with $defs, $ref and defaults
    @classmethod
    def model_json_schema(cls):
        return {"$defs": {"Inner": {"type": "object", "properties": {"n": {"type": "integer", "default": 1}}}},
                "type": "object", "properties": {"inner": {"$ref": "#/$defs/Inner"}}, "required": ["inner"], "title": "FakeModel"}
    @classmethod
    def model_validate(cls, value):
        return ("validated", value)

cases = {
    "str": str, "int": int, "float": float, "bool": bool, "none": None, "any": Any,
    "list": list[int], "dict": dict[str, bool], "tuple": tuple[int, str], "tuple_var": tuple[int, ...], "set": set[str],
    "optional": Optional[int], "union": int | str, "literal": Literal["a", "b"], "enum": Color,
    "annotated": Annotated[str, "a short title"], "dataclass": Point, "typeddict": Movie, "pydantic": FakeModel,
}
print(json.dumps({name: agent.schema(tp) for name, tp in cases.items()}))`,
		);
		const schemas = JSON.parse(out);
		expect(schemas).toMatchObject({
			str: { type: "string" },
			int: { type: "integer" },
			float: { type: "number" },
			bool: { type: "boolean" },
			none: { type: "null" },
			any: {},
			list: { type: "array", items: { type: "integer" } },
			dict: { type: "object", additionalProperties: { type: "boolean" } },
			tuple: {
				type: "array",
				prefixItems: [{ type: "integer" }, { type: "string" }],
				minItems: 2,
				maxItems: 2,
				items: false,
			},
			tuple_var: { type: "array", items: { type: "integer" } },
			set: { type: "array", items: { type: "string" }, uniqueItems: true },
			optional: { anyOf: [{ type: "integer" }, { type: "null" }] },
			union: { anyOf: [{ type: "integer" }, { type: "string" }] },
			literal: { enum: ["a", "b"] },
			enum: { enum: ["red", "blue"] },
			annotated: { type: "string", description: "a short title" },
			dataclass: {
				type: "object",
				title: "Point",
				properties: { x: { type: "integer" }, y: { type: "number" }, tags: { type: "array" } },
				required: ["x"],
				additionalProperties: false,
			},
			typeddict: { type: "object", properties: { title: { type: "string" } }, required: ["title"] },
			pydantic: {
				type: "object",
				properties: { inner: { type: "object", properties: { n: { type: "integer" } } } },
				required: ["inner"],
			},
		});
		// Keywords the registry refuses are dropped, and references are inlined.
		expect(JSON.stringify(schemas.pydantic)).not.toMatch(/\$ref|\$defs|default/);
		// Every derived schema registers: a definition using all of them is accepted by the real registry.
		expect(
			await run(
				k,
				`@agent
class Everything:
    async def take(self, a: list[int], b: dict[str, bool], c: tuple[int, str], d: Optional[int], e: Literal["a", "b"], f: Color, g: Point, h: Movie, i: FakeModel, j: Annotated[str, "t"], k: set[str]) -> Point:
        ...
print(await Everything.register())`,
			),
		).toBe("1\n");
	});

	test("unsupported contracts fail at class definition with the reason", async () => {
		const { kernel } = setup(labelReply);
		const k = kernel();
		const failure = async (code: string) => (await k.execute(code)).error?.evalue ?? "";
		expect(await failure("@agent\nclass A:\n    async def m(self, x: bytes) -> str:\n        ...")).toMatch(
			/A\.m\(x\): unsupported annotation 'bytes'/,
		);
		expect(await failure("@agent\nclass B:\n    def m(self) -> str:\n        ...")).toMatch(
			/B\.m: a model-driven method must be `async def`/,
		);
		expect(await failure("@agent\nclass C:\n    async def m(self, *items: int) -> str:\n        ...")).toMatch(
			/\*args and \*\*kwargs cannot form a typed contract/,
		);
		expect(await failure("@agent\nclass D:\n    key: str = 'x'")).toMatch(/D\.key: field name is reserved/);
		expect(await failure("@agent\nclass E:\n    n: int = 'three'")).toMatch(/E\.n default must be integer, got str/);
		expect(await failure("@agent\nclass F:\n    async def m(self) -> dict[int, str]:\n        ...")).toMatch(
			/dict keys must be str/,
		);
	});
});

describe("agent classes: invocation", () => {
	test("a model-driven method is one typed agents.invoke: docstrings are the prompt, arguments and fields are data", async () => {
		const { fixture, kernel } = setup(labelReply);
		const k = kernel();
		const out = await run(
			k,
			`${TRIAGE}
t = Triage("main")
label = await t.classify("the app crashes on start")
print(type(label).__name__, label.kind, label.confidence, t.record(label), Triage.describe()["generation"])`,
		);
		expect(out).toBe("Label bug 0.9 1 1\n");
		expect(fixture.prompts).toHaveLength(1);
		const prompt = fixture.prompts[0]!.prompt;
		expect(prompt).toContain("TRIAGE ROLE: you label incoming reports.");
		expect(prompt).toContain("You implement the method `Triage.classify(report: str) -> Label`.");
		expect(prompt).toContain("Label the report as a bug or a feature.");
		expect(prompt).toContain(
			'Input data:\n{"args":{"report":"the app crashes on start"},"state":{"seen":0,"labels":[]}}',
		);
		expect(prompt).toContain('One JSON value of type {kind: "bug" | "feature", confidence: number}');
		// The instructions carry no argument values: arguments stay in the data channel.
		expect(prompt.split("Input data:")[0]).not.toContain("crashes");
		const tasks = await journal(fixture);
		expect(tasks).toHaveLength(1);
		expect(tasks[0]).toMatchObject({
			definition: "triage--classify@1",
			state: "completed",
			result: { status: "succeeded", value: { kind: "bug", confidence: 0.9 }, verification: "unverified" },
		});
		const listed = (await fixture.call("agents.list")) as Array<Record<string, unknown>>;
		expect(listed.find((item) => item.id === "triage--classify")).toMatchObject({
			version: "1",
			strategy: "rlm",
			inputDescription: expect.stringContaining("args: {report: string"),
		});
	});

	test("large arguments reach the model as bounded previews with a file holding the full value", async () => {
		const { fixture, kernel, spillDir } = setup((_method, input) => {
			const doc = input.args.doc as { $preview: { preview: string }; path: string; bytes: number };
			return JSON.stringify(`${doc.bytes}:${readFileSync(doc.path, "utf8").length}`);
		});
		const k = kernel();
		const out = await run(
			k,
			`@agent
class Reader:
    """Summarize documents."""
    async def summarize(self, doc: str) -> str:
        """Summarize the document."""
        ...
big = "x" * 200_000
print(await Reader().summarize(big))`,
		);
		expect(out).toBe(`200002:200002\n`);
		const prompt = fixture.prompts[0]!.prompt;
		expect(prompt.length).toBeLessThan(8_000);
		expect(prompt).toContain('"$preview":{"type":"builtins.str","length":200000');
		expect(prompt).toContain(spillDir);
	});

	test("a bad return is rejected with the schema error and never recorded as success (A14)", async () => {
		let reply = JSON.stringify({ kind: "urgent", confidence: 0.5 });
		const { fixture, kernel } = setup(() => reply);
		const k = kernel();
		await run(k, TRIAGE);
		const bad = await k.execute(`t = Triage("main")\nawait t.classify("crash")`);
		expect(bad.status).toBe("error");
		expect(bad.error?.ename).toBe("AgentCallError");
		expect(bad.error?.evalue).toBe(
			'Triage.classify (triage--classify@1) failed: triage--classify@1 output does not match its schema: $.kind must be equal to one of the allowed values ("bug", "feature")',
		);
		reply = JSON.stringify({ kind: "bug" });
		expect((await k.execute(`await Triage("main").classify("crash")`)).error?.evalue).toContain(
			"$ must have required properties confidence",
		);
		reply = "Looks like a bug to me.";
		expect((await k.execute(`await Triage("main").classify("crash")`)).error?.evalue).toMatch(
			/Triage\.classify \(triage--classify@1\) failed: .*JSON/,
		);
		// The error keeps the host's result for code that wants to inspect it; state is untouched.
		expect(
			await run(
				k,
				`try:
    await Triage("main").classify("crash")
except AgentCallError as error:
    print(error.result["status"], error.result["verification"], error.definition, Triage("main").seen)`,
			),
		).toBe("failed unverified triage--classify@1 0\n");
		const tasks = await journal(fixture);
		expect(tasks.length).toBe(4);
		for (const task of tasks) {
			expect(task.state).toBe("failed");
			expect(task.result).toMatchObject({ status: "failed", verification: "unverified" });
			expect(task.result?.value).toBeUndefined();
		}
		// Input contracts are checked in the kernel before any task is admitted.
		const wrongInput = await k.execute(`await Triage("main").classify(42)`);
		expect(wrongInput.error).toMatchObject({
			ename: "TypeError",
			evalue: "Triage.classify(report) must be string, got int",
		});
		expect((await journal(fixture)).length).toBe(4);
	});

	test("real bodies run in the kernel; subclasses rebuild inherited model-driven methods with their own role", async () => {
		const { fixture, kernel } = setup((method) => JSON.stringify(method === "respond" ? ["concern"] : 0));
		const k = kernel();
		const out = await run(
			k,
			`import os
@agent
class Role:
    """Generic reviewer."""
    async def respond(self, change: str) -> list[str]:
        """Review the change."""
        ...
    def pid(self) -> int:
        return os.getpid()

class Security(Role):
    """SECURITY ROLE: review authentication and data exposure."""

s = Security("s1")
print(await s.respond("add login"), s.pid() == os.getpid(), Security.describe()["methods"]["respond"]["definition"])`,
		);
		expect(out).toBe("['concern'] True security--respond\n");
		expect(fixture.prompts[0]!.prompt).toContain("SECURITY ROLE");
		expect(fixture.prompts[0]!.prompt).not.toContain("Generic reviewer");
	});
});

describe("agent classes: durable state", () => {
	test("fields persist across cells and reset_scratch; assignments are type-checked", async () => {
		const { kernel } = setup(labelReply);
		const k = kernel();
		await run(k, `${TRIAGE}\nt = Triage("main")\nt.record(await t.classify("crash"))\nt.labels.append("manual")`);
		expect(await run(k, `print(t.seen, t.labels, Triage.instances())`)).toBe("1 ['bug', 'manual'] ['main']\n");
		expect((await k.execute(`t.seen = "many"`)).error?.evalue).toBe("Triage.seen must be integer, got str");
		await k.resetScratch();
		// Scratch is fresh (the instance variable is gone), but the class is defined again and the state reattaches.
		expect(
			await run(
				k,
				`print('t' in globals(), 'Label' in globals())\nt = Triage("main")\nprint(t.seen, t.labels, Triage("other").seen)`,
			),
		).toBe("False True\n1 ['bug', 'manual'] 0\n");
		expect(await run(k, `t.record(await t.classify("feature request"))\nprint(t.fields())`)).toBe(
			"{'seen': 2, 'labels': ['bug', 'manual', 'feature']}\n",
		);
		expect((await k.execute(`Triage.load("missing")`)).error?.ename).toBe("KeyError");
		expect((await k.execute(`@agent\nclass Needs:\n    goal: str\nNeeds("x")`)).error?.evalue).toBe(
			"Needs('x') is missing required field(s): goal",
		);
		expect(await run(k, `n = Needs("x", goal="ship")\nprint(Needs.load("x").goal)`)).toBe("ship\n");
	});

	test("the class, its generation, and instance fields survive a snapshot and restore into a new kernel", async () => {
		const { fixture, kernel, snapshotPath } = setup(labelReply);
		const first = kernel();
		await run(first, `${TRIAGE}\nt = Triage("main")\nt.record(await t.classify("crash"))\nt.labels.append("kept")`);
		const saved = await first.snapshot(snapshotPath);
		expect(saved.status).toBe("ok");
		expect(saved.snapshot?.saved).toContain("state");
		expect(saved.snapshot?.skipped).toEqual(expect.arrayContaining(["Label", "Triage", "t"]));
		await first.shutdown();

		const second = kernel();
		expect((await second.restore(snapshotPath)).status).toBe("ok");
		expect(
			await run(second, `t = Triage("main")\nprint(t.seen, t.labels, agent.classes()["triage"]["generation"])`),
		).toBe("1 ['bug', 'kept'] 1\n");
		expect(
			await run(second, `print(t.record(await t.classify("new feature")), Triage.describe()["generation"])`),
		).toBe("2 1\n");
		// The restored class reused its registered generation instead of minting a new one.
		const listed = (await fixture.call("agents.list")) as Array<{ id: string; version: string }>;
		expect(listed.filter((item) => item.id === "triage--classify").map((item) => item.version)).toEqual(["1"]);
		expect(fixture.prompts.at(-1)!.prompt).toContain('"state":{"seen":1,"labels":["bug","kept"]}');
	});

	test("redefinition makes a new generation; older generations stay invokable and roll back", async () => {
		const { fixture, kernel } = setup(labelReply);
		const k = kernel();
		await run(k, `${TRIAGE}\nawait Triage("main").classify("crash")`);
		// The identical cell again: same descriptors, same generation.
		await run(k, `${TRIAGE}\nawait Triage("main").classify("crash")`);
		const v2 = TRIAGE.replace("Label the report as a bug or a feature.", "GEN2: label strictly.");
		await run(k, `${v2}\nawait Triage("main").classify("crash")`);
		expect(await run(k, `print(Triage.describe()["generation"], Triage.generations())`)).toBe(
			"2 {1: {'current': False, 'restorable': True}, 2: {'current': True, 'restorable': True}}\n",
		);
		expect(
			await run(
				k,
				`Old = Triage.at(1)\nprint(await Old("main").classify("crash"), Triage.describe()["generation"])`,
			),
		).toBe("Label(kind='bug', confidence=0.9) 2\n");
		expect(fixture.prompts.at(-1)!.prompt).toContain("Label the report as a bug or a feature.");
		expect(fixture.prompts.at(-1)!.prompt).not.toContain("GEN2");
		expect(
			await run(k, `await Triage.rollback(1)\nawait Triage("main").classify("crash")\nprint(Triage.generations())`),
		).toBe("{1: {'current': True, 'restorable': True}, 2: {'current': False, 'restorable': True}}\n");
		expect(fixture.prompts.at(-1)!.prompt).not.toContain("GEN2");
		const definitions = (await journal(fixture)).map((task) => task.definition);
		expect(definitions).toEqual([
			"triage--classify@1",
			"triage--classify@1",
			"triage--classify@2",
			"triage--classify@1",
			"triage--classify@1",
		]);
		// A generation that was never registered is refused by the host, not invented.
		expect((await k.execute(`await Triage.at(9)("main").classify("crash")`)).error?.evalue).toMatch(
			/Unknown Ultron agent definition: triage--classify@9/,
		);
	});
});
