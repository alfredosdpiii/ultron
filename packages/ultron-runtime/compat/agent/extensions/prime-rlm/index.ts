import { createHash } from "node:crypto";
import { existsSync, mkdirSync, copyFileSync, readdirSync } from "node:fs";
import { homedir } from "node:os";
import { basename, dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { Type } from "typebox";
import type { ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent";
import { RlmKernel, type KernelExecutionResult, type KernelHostHandler } from "./kernel.ts";
import { RlmChildRegistry, type ChildEntry, type ChildLifecycleEvent } from "./registry.ts";
import {
  configuredChildModelForRoute,
  configuredChildThinkingForRoute,
  configuredModelForRoute,
  configuredThinkingForRoute,
  recallMemory,
  triage,
} from "../jev/index.ts";
import { HindsightClient } from "../jev/hindsight.ts";
import { approvalBypassEnabled, createClient, guardToolCall } from "../jev/guard.ts";
import { traceEvent } from "../jev/telemetry.ts";
import { createPiService, parseResult } from "../../../host/pi-service.mjs";
import { MemoryService } from "../../../host/memory.mjs";
import { createBackgroundRunner } from "../../../host/background.mjs";

const extensionDir = dirname(fileURLToPath(import.meta.url));
const runtimePath = join(extensionDir, "runtime.py");
const RLM_SESSION_ROOT = process.env.PI_RLM_SESSION_ROOT ?? join(homedir(), ".pi", "rlm-sessions");

function sessionKey(ctx: ExtensionContext): string {
  const source = ctx.sessionManager.getSessionFile() ?? "ephemeral";
  return createHash("sha256").update(source).digest("hex").slice(0, 20);
}
function branchAnchor(ctx: ExtensionContext): string {
  const branch = ctx.sessionManager.getBranch();
  const leaf = branch.at(-1);
  return leaf && typeof leaf === "object" && "id" in leaf && typeof leaf.id === "string" ? leaf.id : "root";
}

function appendRlmJobEntry(pi: ExtensionAPI, event: ChildLifecycleEvent, entry: ChildEntry): void {
  pi.appendEntry("rlm-job", {
    version: 1,
    event,
    childId: entry.rlm_child_id,
    name: entry.session_name,
    status: entry.status,
    model: entry.model,
    timeoutMs: entry.timeout_ms,
    parentSessionKey: entry.parent_session_key,
    parentBranchAnchor: entry.parent_branch_anchor,
    startedAt: entry.started_at,
    endedAt: entry.ended_at,
    error: entry.error,
  });
}


function renderExecution(result: KernelExecutionResult): string {
  const sections: string[] = [];
  if (result.stdout) sections.push(`stdout:\n${result.stdout.trimEnd()}`);
  if (result.stderr) sections.push(`stderr:\n${result.stderr.trimEnd()}`);
  if (result.result) sections.push(`result:\n${result.result}`);
  if (result.error) sections.push(`error: ${result.error.ename}: ${result.error.evalue}\n${result.error.traceback.join("\n")}`);
  return sections.join("\n\n") || (result.status === "ok" ? "(no output)" : "(RLM execution failed)");
}

function testsWriterPrompt(request: string): string {
  return `You are Pi's tests-writer agent. Work in the current repository and write focused, durable tests for this request:

${request}

Rules:
- Inspect the changed implementation, public API, existing test conventions, package scripts, and relevant fixtures before editing.
- Derive the observable contract and boundary/error/state partitions first. Prefer tests that fail on realistic regressions.
- Test through public behavior. Do not assert private state, helper names, incidental serialization/order, or mock call order unless that interaction is explicitly contractual.
- Never derive the expected value by calling the implementation under test or duplicating its algorithm. Use an independent expected value, reference example, invariant, property, or fixed-vs-buggy case.
- Preserve existing fixtures and conventions. Change only test files unless the request explicitly requires production changes.
- Run the narrow baseline test command before and after the edit. Record the exact command and result.
- If the repository has a configured mutation-testing command, run a bounded mutation slice after the baseline. Do not chase 100%; classify killed, survived, no-coverage, timeout, and runner-error results separately.
- Use the jev_test_quality tool when available for generated tests, and jev_mutation_judge for selected mutant evidence. Jev is review evidence, not the executable oracle.
- Do not claim a test is useful without an observed baseline pass and a concrete contract-level assertion.

Finish with:
1. changed test paths;
2. exact verification commands and results;
3. a concise contract-to-assertion map;
4. any surviving or unclassified mutants;
5. a TEST_JUDGMENT_INPUT block containing the contract, test source or diff, and baseline result for the parent to review.`;
}

type RlmState = {
  key: string;
  sessionDir: string;
  snapshotPath: string;
  hostHandler: KernelHostHandler;
  kernel: RlmKernel;
  children: RlmChildRegistry;
  agents: ReturnType<typeof createPiService>;
  signal?: AbortSignal;
  ctx?: ExtensionContext;
};

export default function primeRlmExtension(pi: ExtensionAPI) {
  const jevClient = createClient();
  const hindsight = new HindsightClient();
  let state: RlmState | undefined;

  function ensureState(ctx: ExtensionContext): RlmState {
    const key = sessionKey(ctx);
    const anchor = branchAnchor(ctx);
    if (state?.key === key) {
      state.children.setParentContext(key, anchor);
      state.ctx = ctx;
      return state;
    }
    const sessionDir = join(RLM_SESSION_ROOT, key);
    const snapshotPath = join(sessionDir, "kernel-state.dill");
    const children = new RlmChildRegistry({
      sessionId: key,
      cwd: process.cwd(),
      maxDepth: Number(process.env.PI_RLM_MAX_DEPTH ?? 3),
      depth: Number(process.env.PI_RLM_DEPTH ?? 0),
      extensionPath: join(dirname(extensionDir), "prime-rlm", "index.ts"),
      approvalBypass: () => approvalBypassEnabled(pi),
      parentBranchAnchor: anchor,
      onChange: (event, entry) => appendRlmJobEntry(pi, event, entry),
    });
    const memory = MemoryService({
      directory: join(sessionDir, "memory"),
      backend: {
        namespace: `${hindsight.baseUrl}/v1/default/banks/${encodeURIComponent(hindsight.bankId)}`,
        scopeTags: {
          session: [`pi:session:${key}`],
          branch: [`pi:branch:${anchor}`],
          project: [`pi:project:${createHash("sha256").update(ctx.cwd).digest("hex").slice(0, 16)}`],
        },
        recall: (request, signal) => hindsight.recallFiltered(request, signal),
        retain: (request, signal) => hindsight.retainDocument(request, signal),
        get: (id, signal) => hindsight.document(id, signal),
        delete: (id, signal) => hindsight.deleteDocument(id, signal),
        operation: (id, signal) => hindsight.operation(id, signal),
      },
      gate: async (request, signal) => {
        if (!jevClient) return request.action === "recall" ? { retrieve: false, probability: 0 } : { action: "skip", confidence: 0 };
        if (request.action === "recall") return decideMemoryGate(jevClient, request.query, signal);
        return decideMemoryPolicy(jevClient, request.text, "", signal);
      },
    });
    const agents = createPiService({
      memory,
      directory: sessionDir,
      children,
      onEvent: (type: string, data: Record<string, unknown>) => traceEvent(type, data),
      route: async (prompt: string, signal?: AbortSignal) => {
        if (!jevClient) return { model: configuredChildModelForRoute("powerful") };
        try {
          const decision = await triage(jevClient, prompt, signal);
          const route = decision.route === "fast" && (decision.routeConfidence < 0.55 || decision.complexity >= 2) ? "powerful" : decision.route;
          return { model: configuredChildModelForRoute(route), thinking: configuredChildThinkingForRoute(route) };
        } catch (error) {
          signal?.throwIfAborted();
          traceEvent("typed_route_unavailable", { error: String(error) });
          return { model: configuredChildModelForRoute("powerful") };
        }
      },
      predict: async ({ request, definition, signal }: any) => {
        const ctx = state?.ctx;
        if (!ctx?.model) throw new Error("No active Pi model");
        const selected = request.model;
        const slash = typeof selected === "string" ? selected.indexOf("/") : -1;
        if (selected && slash < 1) throw new Error("Explicit model must be provider/model");
        const model = selected ? ctx.modelRegistry.find(selected.slice(0, slash), selected.slice(slash + 1)) : ctx.model;
        if (!model) throw new Error("Explicit model is unavailable");
        const response = await ctx.modelRegistry.complete(model, { messages: [{ role: "user", timestamp: Date.now(), content: [{ type: "text", text: `${definition.instructions}\nReturn ONLY JSON matching ${JSON.stringify(definition.outputSchema)}\nInput data: ${JSON.stringify(request.input)}` }] }] }, { signal, maxTokens: 2048 });
        if (response.stopReason !== "stop") throw new Error(`Prediction did not complete: ${response.stopReason}`);
        const text = response.content.filter((part: any) => part.type === "text").map((part: any) => part.text).join("\n");
        return { value: parseResult(text), cost: response.usage?.cost?.total ?? null };
      },
    });
    const hostHandler: KernelHostHandler = async (type, payload) => {
      const current = state;
      if (["agents.", "workflows.", "instances.", "artifacts.", "experiments.", "refinements.", "memory."].some(prefix => type.startsWith(prefix))) {
        return agents.dispatch(type, payload, { signal: current?.signal, branch: current?.ctx ? branchAnchor(current.ctx) : anchor });
      }
      if (type === "jev.triage") {
        const prompt = typeof payload.prompt === "string" ? payload.prompt : "";
        if (!jevClient) return { available: false, reason: "Jev API key is not configured" };
        const decision = await triage(jevClient, prompt, current?.signal);
        traceEvent("rlm_jev_triage", {
          promptHash: createHash("sha256").update(prompt).digest("hex").slice(0, 16),
          route: decision.route,
          category: decision.category,
          complexity: decision.complexity,
        });
        return { available: true, ...decision };
      }
      if (type === "jev.recall") {
        const prompt = typeof payload.prompt === "string" ? payload.prompt : "";
        if (!jevClient) return { available: false, gate: { retrieve: false, probability: 0 }, results: [] };
        const memory = await recallMemory(jevClient, hindsight, prompt, current?.signal);
        traceEvent("rlm_jev_recall", {
          promptHash: createHash("sha256").update(prompt).digest("hex").slice(0, 16),
          retrieve: memory.gate.retrieve,
          resultCount: memory.results.length,
        });
        return { available: true, ...memory };
      }
      if (type === "rlm.spawn") {
        const prompt = typeof payload.prompt === "string" ? payload.prompt : "";
        const kwargs = payload.kwargs && typeof payload.kwargs === "object" ? payload.kwargs as Record<string, unknown> : {};
        const routedKwargs = { ...kwargs };
        if (!routedKwargs.model && jevClient) {
          const decision = await triage(jevClient, prompt, current?.signal);
          const route = decision.route === "fast" &&
            (decision.routeConfidence < 0.55 || decision.complexity >= 2)
            ? "powerful"
            : decision.route;
          const model = configuredChildModelForRoute(route);
          if (model.includes("/")) routedKwargs.model = model;
          if (!routedKwargs.thinking) routedKwargs.thinking = configuredChildThinkingForRoute(route);
          traceEvent("rlm_spawn_route", {
            promptHash: createHash("sha256").update(prompt).digest("hex").slice(0, 16),
            route,
            model,
            thinking: routedKwargs.thinking,
          });
        }
        const child = await children.spawn(prompt, routedKwargs, current?.signal);
        traceEvent("rlm_spawn", {
          childId: child.rlm_child_id,
          name: child.session_name,
          model: child.model,
          timeoutMs: child.timeout_ms,
          parentBranchAnchor: child.parent_branch_anchor,
          depth: Number(process.env.PI_RLM_DEPTH ?? 0),
        });
        return {
          rlm_child_id: child.rlm_child_id,
          name: child.session_name,
          session_dir: child.session_dir,
          model: child.model,
          timeout_ms: child.timeout_ms,
          parent_branch_anchor: child.parent_branch_anchor,
        };
      }
      if (type === "rlm.list_subagents") return { subagents: children.list() };
      if (type === "rlm.collect") {
        const selectors = Array.isArray(payload.selectors) ? payload.selectors.filter((item): item is string => typeof item === "string") : [];
        const timeoutMs = typeof payload.timeout_ms === "number" ? payload.timeout_ms : 0;
        return { results: await children.collect(selectors, timeoutMs) };
      }
      if (type === "rlm.delete_subagent") {
        const selector = typeof payload.selector === "string" ? payload.selector : "";
        return { subagent: await children.delete(selector) };
      }
      if (type === "rlm.message") {
        const selector = typeof payload.selector === "string" ? payload.selector : "";
        const message = typeof payload.message === "string" ? payload.message : "";
        return children.message(selector, message);
      }
      if (type === "agent_message.send") {
        const role = payload.receiver_role === "parent" ? "parent" : "child";
        const message = typeof payload.message === "string" ? payload.message : "";
        if (role === "parent") return children.sendToParent(message);
        const selector = typeof payload.receiver_name === "string" ? payload.receiver_name : "";
        return children.message(selector, message);
      }
      if (type === "rlm.find_models") {
        const configured = (process.env.PI_RLM_MODELS ?? "").split(",").map((item) => item.trim()).filter(Boolean);
        const query = typeof payload.query === "string" ? payload.query.toLowerCase() : "";
        return { models: configured.filter((model) => !query || model.toLowerCase().includes(query)).slice(0, Number(payload.limit) || 8).map((selector) => ({ selector, provider: selector.split("/", 1)[0], id: selector.split("/").slice(1).join("/"), name: selector })) };
      }
      if (type === "bash") {
        const command = typeof payload.command === "string" ? payload.command : "";
        if (!command.trim()) throw new Error("bash command must be non-empty");
        const current = state;
        if (!current?.ctx) throw new Error("RLM session context is unavailable");
        const decision = await guardToolCall(jevClient, pi, current.ctx, "bash", { command }, current.signal);
        if (decision?.block) throw new Error(decision.reason);
        const result = await pi.exec("bash", ["-lc", command], { signal: state?.signal });
        return { stdout: result.stdout, stderr: result.stderr, code: result.code, killed: result.killed };
      }
      throw new Error(`Unknown RLM host request: ${type}`);
    };
    const kernel = new RlmKernel({ cwd: ctx.cwd, runtimePath, snapshotPath }, hostHandler);
    state = { key, sessionDir, snapshotPath, hostHandler, kernel, children, agents, ctx };
    return state;
  }

  pi.registerTool({
    name: "ipython",
    label: "Python REPL",
    description: "Execute Python in a persistent RLM control environment. Use rlm.spawn(...) for recursive child agents, rlm.list_subagents() to inspect them, and await bash(command) for host-executed commands.",
    promptSnippet: "Persistent Python REPL with Jev-governed recursive subagents",
    promptGuidelines: [
      "Use ipython as the primary interface for persistent computation and programmatic delegation; direct tools remain available for simple work.",
      "Use await agents.list(), agents.invoke('definition@version', input), or agents.spawn(...) for schema-validated specialists; handles confirm admission only. Optional workflows.run(nodes) invokes the same specialists.",
      "Permission prompts and optional typed-agent controls are off by default. Schema validation and truthful failure reporting remain active.",
      "Use memory.prepare(query, scope='session', task_id='...') only when memory is useful; memory.why(task_id) explains a recorded decision without a fresh search. Memory writes require evidence references.",
      "Use refinements.propose/list/activate/rollback for versioned procedure changes; activation is local and default-off approval does not mean content is verified.",
      "Use await rlm.spawn(prompt, name='worker', timeout_ms=...) to admit independent child agents; child jobs have bounded lifetimes and inherit parent cancellation.",
      "Use await rlm.list_subagents() to inspect child status and read child artifacts from their session_dir.",
      "Use await jev.triage(prompt) for a typed route/complexity/urgency decision, or await jev.recall(prompt) for Jev-gated Hindsight context.",
      "Jev routes child tasks and gates Hindsight recall/retention; host bash commands use the same approval policy as normal tools.",
      "Treat Python execution as trusted local code with the current user's permissions; do not run untrusted code without an external sandbox.",
    ],
    parameters: Type.Object({ code: Type.String({ description: "Python code to execute in the persistent RLM REPL" }) }),
    async execute(_toolCallId, params, signal, _onUpdate, ctx) {
      const current = ensureState(ctx);
      current.signal = signal;
      current.ctx = ctx;
      const result = await current.kernel.execute(params.code, signal);
      return {
        content: [{ type: "text", text: renderExecution(result) }],
        details: result,
      };
    },
  });

  pi.registerCommand("memory", {
    description: "Inspect recorded memory decisions without forcing a new retrieval",
    handler: async (args, ctx) => {
      const current = ensureState(ctx);
      const taskId = args.trim() || `session:${current.key}`;
      const result = await current.agents.dispatch("memory.why", { taskId }, { branch: branchAnchor(ctx) });
      ctx.ui.notify(JSON.stringify(result, null, 2), "info");
    },
  });

  pi.registerCommand("refine-proposals", {
    description: "List durable refinement proposals and activations",
    handler: async (_args, ctx) => {
      const current = ensureState(ctx);
      ctx.ui.notify(JSON.stringify(await current.agents.dispatch("refinements.list", {}, { branch: branchAnchor(ctx) }), null, 2), "info");
    },
  });

  pi.registerCommand("experiments", {
    description: "List recorded Twin experiment runs",
    handler: async (_args, ctx) => {
      const current = ensureState(ctx);
      ctx.ui.notify(JSON.stringify(await current.agents.dispatch("experiments.list", {}, { branch: branchAnchor(ctx) }), null, 2), "info");
    },
  });

  pi.registerCommand("background", {
    description: "Start or inspect an explicit detached Pi job",
    handler: async (args, ctx) => {
      const words = args.trim().split(/\s+/).filter(Boolean);
      const runner = createBackgroundRunner();
      const command = words.shift() ?? "list";
      try {
        const result = command === "start" ? await runner.start(words.join(" "))
          : command === "inspect" ? runner.inspect(words[0])
          : command === "stop" ? await runner.stop(words[0])
          : runner.list();
        ctx.ui.notify(JSON.stringify(result, null, 2), "info");
      } catch (error) { ctx.ui.notify(String(error), "error"); }
    },
  });

  pi.registerCommand("agents", {
    description: "Show typed agent definitions, task status, and default-off controls",
    handler: async (_args, ctx) => {
      const current = ensureState(ctx);
      ctx.ui.notify(JSON.stringify({ definitions: current.agents.tasks.listDefinitions().map((d: any) => d.key), tasks: current.agents.tasks.list().map((t: any) => ({ id: t.id, state: t.state })), controls: current.agents.tasks.controls, durability: "session task journal; branch-filtered tasks and checkpoint restoration" }, null, 2), "info");
    },
  });

  pi.registerCommand("rlm", {
    description: "Show persistent RLM kernel and child-agent status",
    handler: async (_args, ctx) => {
      const current = ensureState(ctx);
      ctx.ui.notify(JSON.stringify({ kernel: current.kernel.isRunning ? "running" : "stopped", children: current.children.list() }, null, 2), "info");
    },
  });

  pi.registerCommand("rlm-children", {
    description: "List recursive RLM child agents",
    handler: async (_args, ctx) => {
      const current = ensureState(ctx);
      const children = current.children.list();
      ctx.ui.notify(children.length ? children.map((child) => `${child.rlm_child_id} ${child.session_name} ${child.status}`).join("\n") : "No RLM child agents.", "info");
    },
  });

  pi.registerCommand("test-writer", {
    description: "Start a Jev-routed child agent that writes focused tests",
    handler: async (args, ctx) => {
      const request = args.trim();
      if (!request) {
        ctx.ui.notify("Usage: /test-writer <behavior or change to cover>", "warning");
        return;
      }
      const current = ensureState(ctx);
      current.signal = ctx.signal;
      current.ctx = ctx;
      let route: "fast" | "powerful" | "architecture" | "designer" = "powerful";
      if (jevClient) {
        try {
          const decision = await triage(jevClient, request, ctx.signal);
          route = decision.route === "fast" ? "powerful" : decision.route;
          traceEvent("test_writer_route", {
            route,
            category: decision.category,
            complexity: decision.complexity,
          });
        } catch (error) {
          traceEvent("test_writer_route_error", { error: String(error) });
        }
      }
      const child = await current.children.spawn(testsWriterPrompt(request), {
        name: `tests-writer-${Date.now()}`,
        model: configuredModelForRoute(route),
        thinking: configuredThinkingForRoute(route),
      });
      traceEvent("test_writer_spawn", {
        childId: child.rlm_child_id,
        name: child.session_name,
        route,
        model: child.model,
      });
      ctx.ui.notify(`Tests writer started: ${child.session_name} (${route}). Use /rlm-children to monitor it.`, "info");
    },
  });

  pi.on("agent_end", async (_event, ctx) => {
    if (!state || state.key !== sessionKey(ctx) || !state.kernel.isRunning) return;
    const directory = join(state.sessionDir, "checkpoints");
    mkdirSync(directory, { recursive: true, mode: 0o700 });
    const result = await state.kernel.snapshot(join(directory, `${branchAnchor(ctx)}.dill`));
    if (result.status !== "ok") traceEvent("kernel_checkpoint_failed", { session: state.key });
  });

  pi.on("session_tree", async (_event, ctx) => {
    if (!state || state.key !== sessionKey(ctx)) return;
    const current = state;
    await current.kernel.shutdown();
    const branch = ctx.sessionManager.getBranch();
    current.agents.selectBranch(["root", ...branch.map((entry: any) => entry.id)]);
    const checkpoint = [...branch].reverse().map((entry: any) => join(current.sessionDir, "checkpoints", `${entry.id}.dill`)).find(path => existsSync(path));
    // A branch change must never silently restore the later branch's default snapshot.
    current.ctx = ctx;
    current.kernel = new RlmKernel({ cwd: ctx.cwd, runtimePath, ...(checkpoint ? { snapshotPath: checkpoint } : {}) }, current.hostHandler);
    ctx.ui.notify(checkpoint ? "Python restored from the nearest checkpoint on this branch." : "No Python checkpoint on this branch; starting with empty working state.", "info");
  });

  pi.on("session_shutdown", async (_event, ctx) => {
    if (!state || state.key !== sessionKey(ctx)) return;
    await state.agents.shutdown();
    await state.kernel.snapshot(state.snapshotPath).catch(() => undefined);
    await state.kernel.shutdown();
    await state.children.shutdown();
    state = undefined;
  });
}
