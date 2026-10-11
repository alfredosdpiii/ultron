// A scripted NativeHostApi for the progress and instances fixtures (mirrored by tests/fake_tasks/mod.rs): a fixed task
// tree, sequentially numbered spawned tasks whose outcome depends on their definition, idempotent spawn keys, a lane
// pool with two pins, a settable caller and clock, and a log of every task operation the module made.
// `test.clock {ms}` advances the clock, `test.caller {id}` sets the calling task (null: root), `test.finish {id, value}`
// completes a running task.

const done = (value) => ({ status: "succeeded", value, verification: "unverified" });
const task = (id, definition, state, extra = {}) => ({ id, key: `key-${id}`, fingerprint: "f".repeat(64), definition, state, ...extra });

export function fakeTasks(start) {
	const log = [];
	let clock = start;
	let caller = null;
	let spawned = 0;
	let pins = 0;
	const keys = new Map();
	const tasks = [
		task("task-a", "worker@1", "running"),
		task("task-b", "worker@1", "running", { parentId: "task-a" }),
		task("task-c", "worker@1", "running", { parentId: "task-b" }),
		task("task-idle", "worker@1", "admitted"),
		task("task-done", "rlm-child@1", "completed", { result: done({ answer: 42 }) }),
		task("task-failed", "rlm-child@1", "failed", { result: { status: "failed", error: "boom", verification: "unverified" } }),
		task("det-done", "det@1", "completed", { result: done(1) }),
		task("child-done", "rlm-child@1", "completed", { parentId: "task-a", result: done("ok") }),
		task("other-done", "rlm-child@1", "completed", { parentId: "task-a", result: done("ok") }),
	];
	const strategies = { "worker@1": "rlm", "rlm-child@1": "rlm", "det@1": "deterministic" };
	const api = {
		callerTaskId: () => caller,
		taskLane: (id) => {
			log.push(["taskLane", id]);
			return id === "task-done" ? "lane-task-done" : null;
		},
		strategy: (definition) => {
			if (strategies[definition]) return strategies[definition];
			if (definition.startsWith("verify-")) return "deterministic";
			throw new Error(`Unknown Ultron definition: ${definition}`);
		},
		tasks: async () => structuredClone(tasks),
		spawn: async (request, parent) => {
			log.push(["spawn", structuredClone(request), parent]);
			if (request.key !== undefined && keys.has(request.key)) return structuredClone(keys.get(request.key));
			if (request.definition === "verify-nostart@1") throw new Error("Admission refused: budget exhausted");
			spawned += 1;
			const id = `spawned-${spawned}`;
			const outcomes = {
				"verify-pass@1": ["completed", done({ passed: true })],
				"verify-fail@1": ["completed", done({ passed: false })],
				"verify-odd@1": ["completed", done("yes")],
				"verify-error@1": ["failed", { status: "failed", error: "verifier crashed", verification: "unverified" }],
				"verify-noresult@1": ["completed", undefined],
			};
			const [state, result] = outcomes[request.definition] ?? ["running", undefined];
			const created = task(id, request.definition, state, {
				...(parent === null ? {} : { parentId: parent }),
				...(result === undefined ? {} : { result }),
			});
			created.key = request.key ?? `auto-${spawned}`;
			tasks.push(created);
			if (request.key !== undefined) keys.set(request.key, created);
			return structuredClone(created);
		},
		result: async (id) => {
			log.push(["result", id]);
			const found = tasks.find((candidate) => candidate.id === id);
			if (!found?.result) throw new Error("result lost");
			return structuredClone(found.result);
		},
		cancel: async () => {
			throw new Error("cancel is not scripted");
		},
		steer: async () => false,
		usage: async () => null,
		pinLane: (lane, holder) => {
			log.push(["pinLane", lane, holder]);
			if (pins >= 2) return false;
			pins += 1;
			return true;
		},
		unpinLane: (lane, holder) => {
			log.push(["unpinLane", lane, holder]);
			pins -= 1;
		},
		now: () => clock,
	};
	const testModule = {
		prefixes: ["test."],
		handle: async (request) => {
			const { type, payload } = request;
			if (type === "test.clock") return (clock += payload.ms);
			if (type === "test.caller") return (caller = payload.id ?? null);
			if (type === "test.finish") {
				const found = tasks.find((candidate) => candidate.id === payload.id);
				found.state = "completed";
				found.result = done(payload.value ?? null);
				return true;
			}
			throw new Error(`Ultron RLM host request is not wired: ${type}`);
		},
	};
	/** The module as NativeRlmHost sees it, served the fake API instead of the host's. */
	const wrap = (module) => ({
		prefixes: module.prefixes,
		start: () => module.start?.(api),
		handle: (request) => module.handle(request, api),
		close: () => module.close?.(),
	});
	return { api, log, testModule, wrap };
}
