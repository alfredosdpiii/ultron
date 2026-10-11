// Generates crates/extensions/fixtures/runner.json: runs the TypeScript ExtensionRunner
// (packages/coding-agent/src/core/extensions/runner.ts) over scripted extension sets and records, per scenario, every
// handler call in order (which extension saw which event), the dispatch outcome (result or thrown error), and every
// error the runner reported through onError. The Rust tests drive crates/extensions' router with in-process scripted
// endpoints that answer the same way and require the same log, outcome and errors.
//
// A scripted extension registers at most one handler per event: over the protocol an extension process receives
// each event once and runs its own handlers. Handler actions:
//   {}                    return undefined
//   { return: v }         return v
//   { throw: "m" }        throw new Error("m")
//   { mutateInput: p }    Object.assign(event.input, p)   (protocol: result { input })
//   { setHeaders: p }     Object.assign(event.headers, p) (protocol: result { headers })
// Usage: node crates/extensions/fixtures/generate.mjs   (from the repository root; deterministic)
import { writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { createEventBus } from "../../../packages/coding-agent/src/core/event-bus.ts";
import {
	createExtensionRuntime,
	loadExtensionFromFactory,
} from "../../../packages/coding-agent/src/core/extensions/loader.ts";
import { ExtensionRunner, emitProjectTrustEvent } from "../../../packages/coding-agent/src/core/extensions/runner.ts";

const here = dirname(fileURLToPath(import.meta.url));
const clone = (v) => (v === undefined ? null : JSON.parse(JSON.stringify(v)));

const user = (text) => ({ role: "user", content: [{ type: "text", text }], timestamp: 1 });
const assistant = (text) => ({ role: "assistant", content: [{ type: "text", text }], timestamp: 2 });
const system = (text, timestamp) => ({ role: "system", content: text, timestamp });
const turnEnd = {
	type: "turn_end",
	turnIndex: 0,
	message: assistant("done"),
	toolResults: [],
	messageEntryId: "m1",
	toolResultEntryIds: [],
	outcome: "completed",
};
const custom = (customType) => ({ type: "custom", customType, data: { n: customType } });

const scenarios = [
	{
		name: "session_before_switch: first cancel wins, errors continue",
		extensions: [
			{ path: "a", on: { session_before_switch: { return: { cancel: false } } } },
			{ path: "b", on: { session_before_switch: { throw: "b failed" } } },
			{ path: "c", on: { session_before_switch: { return: { cancel: true } } } },
			{ path: "d", on: { session_before_switch: { return: { cancel: false } } } },
		],
		call: { kind: "emit", event: { type: "session_before_switch", reason: "new" } },
	},
	{
		name: "session_before_fork: last result without cancel is returned",
		extensions: [
			{ path: "a", on: { session_before_fork: { return: { skipConversationRestore: true } } } },
			{ path: "b", on: { session_before_fork: {} } },
			{ path: "c", on: { session_before_fork: { return: { cancel: false } } } },
		],
		call: { kind: "emit", event: { type: "session_before_fork", entryId: "e1", position: "at" } },
	},
	{
		name: "session_before_compact: custom compaction then no cancel",
		extensions: [
			{
				path: "a",
				on: {
					session_before_compact: {
						return: { compaction: { summary: "s", firstKeptEntryId: "e2", tokensBefore: 10 } },
					},
				},
			},
			{ path: "b", on: { session_before_compact: {} } },
		],
		call: {
			kind: "emit",
			event: {
				type: "session_before_compact",
				preparation: { firstKeptEntryId: "e2" },
				branchEntries: [],
				reason: "manual",
				willRetry: false,
			},
		},
	},
	{
		name: "session_before_tree: cancel stops later extensions",
		extensions: [
			{ path: "a", on: { session_before_tree: { return: { cancel: true, label: "x" } } } },
			{ path: "b", on: { session_before_tree: { return: { cancel: false } } } },
		],
		call: {
			kind: "emit",
			event: {
				type: "session_before_tree",
				preparation: { targetId: "t", oldLeafId: null, commonAncestorId: null, entriesToSummarize: [], userWantsSummary: false },
			},
		},
	},
	{
		name: "observe-only event: results ignored, every extension called, errors reported",
		extensions: [
			{ path: "a", on: { agent_start: { return: { cancel: true } } } },
			{ path: "b", on: { agent_start: { throw: "b broke" } } },
			{ path: "c", on: {} },
			{ path: "d", on: { agent_start: {} } },
		],
		call: { kind: "emit", event: { type: "agent_start" } },
	},
	{
		name: "session_shutdown is awaited by every subscriber",
		extensions: [
			{ path: "a", on: { session_shutdown: {} } },
			{ path: "b", on: { session_shutdown: {} } },
		],
		call: { kind: "emit", event: { type: "session_shutdown", reason: "quit" } },
	},
	{
		name: "project_trust: undecided falls through, first decision wins",
		extensions: [
			{ path: "a", on: { project_trust: { return: { trusted: "undecided" } } } },
			{ path: "b", on: { project_trust: { throw: "b cannot decide" } } },
			{ path: "c", on: { project_trust: { return: { trusted: "no", remember: true } } } },
			{ path: "d", on: { project_trust: { return: { trusted: "yes" } } } },
		],
		call: { kind: "projectTrust", event: { type: "project_trust", cwd: "/w" } },
	},
	{
		name: "project_trust: all undecided gives no result",
		extensions: [{ path: "a", on: { project_trust: { return: { trusted: "undecided" } } } }],
		call: { kind: "projectTrust", event: { type: "project_trust", cwd: "/w" } },
	},
	{
		name: "tool_call: input patches chain, first block wins",
		extensions: [
			{ path: "a", on: { tool_call: { mutateInput: { command: "ls -la" } } } },
			{ path: "b", on: { tool_call: { return: { block: false, reason: "fine" } } } },
			{ path: "c", on: { tool_call: { return: { block: true, reason: "no", terminate: true } } } },
			{ path: "d", on: { tool_call: { return: { block: true, reason: "never" } } } },
		],
		call: {
			kind: "toolCall",
			event: { type: "tool_call", toolCallId: "t1", toolName: "bash", input: { command: "ls" } },
		},
	},
	{
		name: "tool_call: no block returns the last result",
		extensions: [
			{ path: "a", on: { tool_call: { return: { block: false, reason: "one" } } } },
			{ path: "b", on: { tool_call: {} } },
		],
		call: { kind: "toolCall", event: { type: "tool_call", toolCallId: "t1", toolName: "rlm", input: { code: "1" } } },
	},
	{
		name: "tool_call: a handler error propagates",
		extensions: [
			{ path: "a", on: { tool_call: { throw: "gate down" } } },
			{ path: "b", on: { tool_call: { return: { block: true } } } },
		],
		call: { kind: "toolCall", event: { type: "tool_call", toolCallId: "t1", toolName: "rlm", input: {} } },
	},
	{
		name: "before_file_write: messages join, first block wins with default reason",
		extensions: [
			{ path: "a", on: { before_file_write: { return: { message: "note a" } } } },
			{ path: "b", on: { before_file_write: {} } },
			{ path: "c", on: { before_file_write: { return: { message: "note c" } } } },
			{ path: "d", on: { before_file_write: { return: { block: true } } } },
			{ path: "e", on: { before_file_write: { return: { message: "never" } } } },
		],
		call: {
			kind: "beforeFileWrite",
			event: { type: "before_file_write", path: "/w/a.txt", content: "x", cwd: "/w", lane: "root" },
		},
	},
	{
		name: "before_file_write: messages only",
		extensions: [
			{ path: "a", on: { before_file_write: { return: { message: "one" } } } },
			{ path: "b", on: { before_file_write: { return: { message: "two", block: false } } } },
		],
		call: {
			kind: "beforeFileWrite",
			event: { type: "before_file_write", path: "/w/a.txt", content: "x", cwd: "/w", lane: "root" },
		},
	},
	{
		name: "before_file_write: a handler error propagates",
		extensions: [
			{ path: "a", on: { before_file_write: { return: { message: "one" } } } },
			{ path: "b", on: { before_file_write: { throw: "checker crashed" } } },
		],
		call: {
			kind: "beforeFileWrite",
			event: { type: "before_file_write", path: "/w/a.txt", content: "x", cwd: "/w", lane: "root" },
		},
	},
	{
		name: "after_cell_changes: every handler runs, messages joined, errors reported",
		extensions: [
			{ path: "a", on: { after_cell_changes: { return: { message: "saw a" } } } },
			{ path: "b", on: { after_cell_changes: { throw: "b failed" } } },
			{ path: "c", on: { after_cell_changes: { return: { message: "saw c" } } } },
		],
		call: {
			kind: "afterCellChanges",
			event: { type: "after_cell_changes", files: ["/w/x"], checked: [], deleted: [], complete: true, cwd: "/w", lane: "root" },
		},
	},
	{
		name: "after_cell_changes: no messages",
		extensions: [{ path: "a", on: { after_cell_changes: {} } }],
		call: {
			kind: "afterCellChanges",
			event: { type: "after_cell_changes", files: [], checked: [], deleted: [], complete: false, cwd: "/w", lane: "root" },
		},
	},
	{
		name: "user_bash: first result wins",
		extensions: [
			{ path: "a", on: { user_bash: {} } },
			{ path: "b", on: { user_bash: { return: { result: { output: "hi", exitCode: 0, cancelled: false, truncated: false } } } } },
			{ path: "c", on: { user_bash: { return: { result: { output: "never", exitCode: 1, cancelled: false, truncated: false } } } } },
		],
		call: { kind: "userBash", event: { type: "user_bash", command: "echo hi", excludeFromContext: false, cwd: "/w" } },
	},
	{
		name: "user_bash: an invalid result is reported and thrown",
		extensions: [
			{ path: "a", on: { user_bash: { return: { result: { output: 1 } } } } },
			{ path: "b", on: { user_bash: { return: { result: { output: "never", exitCode: 0, cancelled: false, truncated: false } } } } },
		],
		call: { kind: "userBash", event: { type: "user_bash", command: "x", excludeFromContext: true, cwd: "/w" } },
	},
	{
		name: "user_bash: a handler error is reported and thrown",
		extensions: [{ path: "a", on: { user_bash: { throw: "no shell" } } }],
		call: { kind: "userBash", event: { type: "user_bash", command: "x", excludeFromContext: false, cwd: "/w" } },
	},
	{
		name: "user_bash: nobody handles it",
		extensions: [{ path: "a", on: { user_bash: {} } }],
		call: { kind: "userBash", event: { type: "user_bash", command: "x", excludeFromContext: false, cwd: "/w" } },
	},
	{
		name: "tool_result: patches chain into later handlers",
		extensions: [
			{ path: "a", on: { tool_result: { return: { content: [{ type: "text", text: "patched" }] } } } },
			{ path: "b", on: { tool_result: { throw: "b failed" } } },
			{ path: "c", on: { tool_result: { return: { isError: true, details: { why: "c" } } } } },
		],
		call: {
			kind: "toolResult",
			event: {
				type: "tool_result",
				toolCallId: "t1",
				toolName: "rlm",
				input: { code: "1" },
				content: [{ type: "text", text: "orig" }],
				isError: false,
				details: null,
			},
		},
	},
	{
		name: "tool_result: unmodified gives no result",
		extensions: [{ path: "a", on: { tool_result: { return: {} } } }],
		call: {
			kind: "toolResult",
			event: { type: "tool_result", toolCallId: "t1", toolName: "rlm", input: {}, content: [], isError: false, details: null },
		},
	},
	{
		name: "message_end: role change rejected, replacement chains",
		extensions: [
			{ path: "a", on: { message_end: { return: { message: user("swapped") } } } },
			{ path: "b", on: { message_end: { return: { message: assistant("rewritten") } } } },
			{ path: "c", on: { message_end: {} } },
		],
		call: { kind: "messageEnd", event: { type: "message_end", message: assistant("orig") } },
	},
	{
		name: "message_end: unmodified",
		extensions: [{ path: "a", on: { message_end: { throw: "a failed" } } }],
		call: { kind: "messageEnd", event: { type: "message_end", message: assistant("orig") } },
	},
	{
		name: "cache_warming_decision: last override wins",
		extensions: [
			{ path: "a", on: { cache_warming_decision: { return: { action: "stop" } } } },
			{ path: "b", on: { cache_warming_decision: { throw: "b failed" } } },
			{ path: "c", on: { cache_warming_decision: { return: { action: "warm" } } } },
			{ path: "d", on: { cache_warming_decision: { return: {} } } },
		],
		call: {
			kind: "cacheWarming",
			event: { type: "cache_warming_decision", warmCost: 1, missCost: 2, continuationProbability: 0.5, action: "stop" },
		},
	},
	{
		name: "input: transforms chain, handled short-circuits",
		extensions: [
			{ path: "a", on: { input: { return: { action: "transform", text: "one" } } } },
			{ path: "b", on: { input: { return: { action: "continue" } } } },
			{ path: "c", on: { input: { return: { action: "handled" } } } },
			{ path: "d", on: { input: { return: { action: "transform", text: "never" } } } },
		],
		call: { kind: "input", text: "hi", source: "interactive" },
	},
	{
		name: "input: transform result",
		extensions: [
			{ path: "a", on: { input: { return: { action: "transform", text: "one" } } } },
			{ path: "b", on: { input: { throw: "b failed" } } },
			{ path: "c", on: { input: { return: { action: "transform", text: "two", images: [{ type: "image", data: "AA", mimeType: "image/png" }] } } } },
		],
		call: { kind: "input", text: "hi", source: "rpc", streamingBehavior: "steer" },
	},
	{
		name: "input: same text with new images is a transform",
		extensions: [
			{ path: "a", on: { input: { return: { action: "transform", text: "hi", images: [{ type: "image", data: "BB", mimeType: "image/png" }] } } } },
			{ path: "b", on: { input: { return: { action: "transform", text: "hi", images: null } } } },
		],
		call: { kind: "input", text: "hi", source: "interactive" },
	},
	{
		name: "input: unchanged continues",
		extensions: [{ path: "a", on: { input: { return: { action: "transform", text: "hi" } } } }],
		call: { kind: "input", text: "hi", source: "extension" },
	},
	{
		name: "before_provider_request: payload chains",
		extensions: [
			{ path: "a", on: { before_provider_request: { return: { model: "m", n: 1 } } } },
			{ path: "b", on: { before_provider_request: {} } },
			{ path: "c", on: { before_provider_request: { throw: "c failed" } } },
			{ path: "d", on: { before_provider_request: { return: { model: "m", n: 2 } } } },
		],
		call: { kind: "providerRequest", payload: { model: "m" } },
	},
	{
		name: "before_provider_headers: mutations apply in order",
		extensions: [
			{ path: "a", on: { before_provider_headers: { setHeaders: { "x-a": "1", authorization: null } } } },
			{ path: "b", on: { before_provider_headers: { throw: "b failed" } } },
			{ path: "c", on: { before_provider_headers: { setHeaders: { "x-a": "2", "x-c": "3" } } } },
		],
		call: { kind: "providerHeaders", headers: { authorization: "Bearer k", "x-keep": "y" } },
	},
	{
		name: "before_agent_start: messages collect, prompt override is seen by later handlers",
		extensions: [
			{ path: "a", on: { before_agent_start: { return: { message: { customType: "note", content: "a", display: true } } } } },
			{ path: "b", on: { before_agent_start: { return: { systemPrompt: "forced by b" } } } },
			{ path: "c", on: { before_agent_start: { throw: "c failed" } } },
			{ path: "d", on: { before_agent_start: { return: { message: { customType: "note", content: "d", display: false, details: { k: 1 } } } } } },
		],
		call: { kind: "agentStart", prompt: "do it" },
	},
	{
		name: "resources_discover: paths collect per extension",
		extensions: [
			{ path: "a", on: { resources_discover: { return: { skillPaths: ["/s1"], themePaths: [] } } } },
			{ path: "b", on: { resources_discover: { throw: "b failed" } } },
			{ path: "c", on: { resources_discover: { return: { skillPaths: ["/s2", "/s3"], promptPaths: ["/p"], themePaths: ["/t"] } } } },
		],
		call: { kind: "resources", cwd: "/w", reason: "startup" },
	},
	{
		name: "context: handlers see the conversation only; a change keeps the replayed system message first",
		extensions: [
			{ path: "a", on: { context: { return: { messages: [user("u1")] } } } },
			{ path: "b", on: { context: { return: { messages: [user("u1")] } } } },
			{ path: "c", on: { context: { throw: "c failed" } } },
		],
		call: {
			kind: "context",
			messages: [system("prompt", 5), user("u1"), assistant("a1"), system("more", 6), user("u2")],
		},
	},
	{
		name: "context_with_system: output used as returned; dropping the system message is reported",
		extensions: [
			{ path: "a", on: { context_with_system: { return: { messages: [user("only")] } } } },
			{ path: "b", on: { context_with_system: {} } },
		],
		call: { kind: "context", messages: [system("prompt", 5), user("u1")] },
	},
	{
		name: "turn_end boundary: entries and continue chain; context rebuilt after each handler",
		extensions: [
			{ path: "a", on: { turn_end: { return: { entries: [custom("one")] } } } },
			{ path: "b", on: { turn_end: { return: { continue: true } } } },
			{ path: "c", on: { turn_end: { throw: "c failed" } } },
			{ path: "d", on: { turn_end: { return: { entries: [custom("one"), custom("two")], continue: false } } } },
		],
		call: { kind: "boundary", event: turnEnd },
	},
	{
		name: "turn_end boundary: invalid entries reset the result",
		extensions: [
			{ path: "a", on: { turn_end: { return: { entries: [custom("bad")], continue: true } } } },
			{ path: "b", on: { turn_end: {} } },
		],
		call: { kind: "boundary", event: turnEnd },
	},
	{
		name: "agent_before_settle boundary: last valid entries win",
		extensions: [
			{ path: "a", on: { agent_before_settle: { return: { entries: [custom("bad")] } } } },
			{ path: "b", on: { agent_before_settle: { return: { entries: [custom("ok")], continue: true } } } },
		],
		call: { kind: "boundary", event: { type: "agent_before_settle", outcome: "aborted" } },
	},
	{
		name: "commands: duplicate names get invocation suffixes in load order",
		extensions: [
			{ path: "a", commands: ["review", "deploy"] },
			{ path: "b", commands: ["review"] },
			{ path: "c", commands: ["review:2", "review", "status"] },
		],
		call: { kind: "commands" },
	},
];

async function runScenario(scenario) {
	const log = [];
	const errors = [];
	const runtime = createExtensionRuntime();
	const eventBus = createEventBus();
	const extensions = [];
	for (const spec of scenario.extensions) {
		const factory = (pi) => {
			for (const [eventType, action] of Object.entries(spec.on ?? {})) {
				pi.on(eventType, async (event) => {
					log.push({ ext: spec.path, event: snapshot(event) });
					if (action.throw !== undefined) throw new Error(action.throw);
					if (action.mutateInput !== undefined) Object.assign(event.input, action.mutateInput);
					if (action.setHeaders !== undefined) Object.assign(event.headers, action.setHeaders);
					return action.return;
				});
			}
			for (const name of spec.commands ?? []) {
				pi.registerCommand(name, { description: `${spec.path} ${name}`, handler: async () => {} });
			}
		};
		extensions.push(await loadExtensionFromFactory(factory, "/w", eventBus, runtime, spec.path));
	}
	const runner = new ExtensionRunner(extensions, runtime, "/w", {}, {});
	runner.onError((e) => errors.push({ extensionPath: e.extensionPath, event: e.event, error: e.error }));
	const call = scenario.call;
	let outcome;
	try {
		outcome = { ok: clone(await dispatch(runner, extensions, call, errors)) };
	} catch (err) {
		outcome = { throws: err instanceof Error ? err.message : String(err) };
	}
	return { name: scenario.name, extensions: scenario.extensions, call, log, outcome, errors };
}

function snapshot(event) {
	if (event.type === "before_agent_start") {
		const forced = event.systemPromptOptions.forceSystemPrompt;
		return clone({
			type: event.type,
			prompt: event.prompt,
			images: event.images,
			systemPrompt: forced === undefined ? "<default>" : event.systemPrompt,
		});
	}
	return clone(event);
}

async function dispatch(runner, extensions, call, errors) {
	switch (call.kind) {
		case "emit":
			return runner.emit(structuredClone(call.event));
		case "projectTrust": {
			const out = await emitProjectTrustEvent({ extensions }, call.event, { cwd: "/w", mode: "print", hasUI: false });
			errors.push(...out.errors.map((e) => ({ extensionPath: e.extensionPath, event: e.event, error: e.error })));
			return out.result;
		}
		case "toolCall": {
			const event = structuredClone(call.event);
			const result = await runner.emitToolCall(event);
			return { result, input: event.input };
		}
		case "beforeFileWrite":
			return runner.emitBeforeFileWrite(structuredClone(call.event));
		case "afterCellChanges":
			return runner.emitAfterCellChanges(structuredClone(call.event));
		case "userBash":
			return runner.emitUserBash(structuredClone(call.event));
		case "toolResult":
			return runner.emitToolResult(structuredClone(call.event));
		case "messageEnd":
			return runner.emitMessageEnd(structuredClone(call.event));
		case "cacheWarming":
			return runner.emitCacheWarmingDecision(structuredClone(call.event));
		case "input":
			return runner.emitInput(call.text, call.images, call.source, call.streamingBehavior);
		case "providerRequest":
			return runner.emitBeforeProviderRequest(structuredClone(call.payload));
		case "providerHeaders":
			return runner.emitBeforeProviderHeaders(structuredClone(call.headers));
		case "agentStart": {
			const out = await runner.emitBeforeAgentStart(call.prompt, call.images, { cwd: "/w" });
			return { messages: out.messages, systemPrompt: out.systemPromptOptions.forceSystemPrompt };
		}
		case "resources":
			return runner.emitResourcesDiscover(call.cwd, call.reason);
		case "context":
			return runner.emitContext(structuredClone(call.messages));
		case "boundary":
			return runner.emitBoundary(structuredClone(call.event), (entries) => {
				if (entries.some((e) => e.customType === "bad")) throw new Error("bad entry");
				return { size: entries.length };
			});
		case "commands":
			return runner
				.getRegisteredCommands()
				.map((c) => ({ name: c.name, invocationName: c.invocationName, description: c.description }));
		default:
			throw new Error(`unknown call ${call.kind}`);
	}
}

const results = [];
for (const scenario of scenarios) results.push(await runScenario(scenario));
writeFileSync(join(here, "runner.json"), `${JSON.stringify({ scenarios: results }, null, "\t")}\n`);
console.log(`wrote ${results.length} scenarios`);
