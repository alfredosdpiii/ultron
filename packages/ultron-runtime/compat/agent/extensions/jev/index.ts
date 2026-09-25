import { Type } from "typebox";
import {
  choice,
  noul,
  score,
  TypeSafeClient,
} from "@typesafe-ai/sdk";
import {
  isToolCallEventType,
  type ExtensionAPI,
  type ExtensionContext,
} from "@earendil-works/pi-coding-agent";
import { HindsightClient, type HindsightRecall } from "./hindsight.ts";
import { decideMemoryGate, decideMemoryPolicy, formatRecall } from "./memory.ts";
import { runDeterministicEvals, runJudgeEvals } from "./evals.ts";
import { pendingApprovals } from "./approval.ts";
import { approvalBypassEnabled, createClient, guardToolCall, setApprovalBypass } from "./guard.ts";
import { hashText, traceEvent } from "./telemetry.ts";
import {
  refinementContext,
  refinementThreshold,
  generateRefinement,
  reviewRefinement,
  type RefinementRecord,
} from "./refinement.ts";
import { runWorkflowGraph } from "./workflow.ts";
import {
  judgeMutation,
  judgeTestQuality,
  type MutationJudgmentInput,
  type TestQualityInput,
} from "./test-quality.ts";

type Route = "fast" | "powerful" | "architecture" | "designer";
export type Triage = {
  route: Route;
  routeConfidence: number;
  complexity: number;
  complexityConfidence: number;
  urgency: "low" | "normal" | "high";
  category: "lookup" | "extraction" | "localized_change" | "debugging" | "architecture" | "design" | "other";
};

export type MemoryWorkflowResult = {
  gate: { retrieve: boolean; probability: number };
  results: HindsightRecall[];
};

type CompactionToolUse = {
  tool_use_id: string;
  tool: string;
  input: Record<string, unknown>;
};

type CompactionToolResult = {
  tool_use_id: string;
  text: string;
  isError?: boolean;
};

type CompactionMessage = {
  role: "user" | "assistant";
  text: string;
  toolUses: CompactionToolUse[];
  toolResults?: CompactionToolResult[];
};

type CompactionCall = {
  id: string;
  toolUseId: string;
  tool: CompactionToolUse;
  result: CompactionToolResult;
  callIndex: number;
  resultIndex: number;
  pinned: boolean;
};

type CompactionDecision = {
  id: string;
  tool: string;
  action: "keep" | "drop_result" | "drop_call";
  keepCall: number;
  keepResult: number;
};

type CompactionResult = {
  messages: CompactionMessage[];
  decisions: CompactionDecision[];
  stats: {
    calls: number;
    callsDropped: number;
    resultsDropped: number;
  };
};

const FAST_MODEL = process.env.PI_JEV_FAST_MODEL ?? "cliproxyapi/glm-5.3-flash";
const POWERFUL_MODEL = process.env.PI_JEV_POWERFUL_MODEL ?? "cliproxyapi/gpt-6-astra";
const ARCHITECTURE_MODEL = process.env.PI_JEV_ARCHITECTURE_MODEL ?? "cliproxyapi/gpt-6-astra";
const DESIGNER_MODEL = process.env.PI_JEV_DESIGNER_MODEL ?? "cliproxyapi/kimi-k3";
const MAIN_MODEL = "cliproxyapi/gpt-6-astra";
const JEV_MODEL = process.env.PI_JEV_MODEL ?? "jev-latest";
const ROUTE_THRESHOLD = 0.55;

function modelForRoute(route: Route): string {
  return route === "fast"
    ? FAST_MODEL
    : route === "architecture"
      ? ARCHITECTURE_MODEL
      : route === "designer"
        ? DESIGNER_MODEL
        : POWERFUL_MODEL;
}


function pickModel(ctx: ExtensionContext, route: Route) {
  const configured = modelForRoute(route);
  const slash = configured.indexOf("/");
  if (slash === -1) return undefined;
  return ctx.modelRegistry.find(configured.slice(0, slash), configured.slice(slash + 1));
}
function pickConfiguredModel(ctx: ExtensionContext, configured: string) {
  const slash = configured.indexOf("/");
  if (slash === -1) return undefined;
  return ctx.modelRegistry.find(configured.slice(0, slash), configured.slice(slash + 1));
}

export function configuredModelForRoute(route: Route): string {
  return modelForRoute(route);
}

export function configuredThinkingForRoute(route: Route): "high" | "xhigh" {
  return route === "fast" || route === "designer" ? "high" : "xhigh";
}
export function configuredChildModelForRoute(route: Route): string {
  return route === "fast"
    ? FAST_MODEL
    : route === "architecture"
      ? ARCHITECTURE_MODEL
      : route === "designer"
        ? DESIGNER_MODEL
        : POWERFUL_MODEL;
}

export function configuredChildThinkingForRoute(route: Route): "high" | "xhigh" {
  return route === "fast" ? "high" : "xhigh";
}

export async function triage(client: TypeSafeClient, prompt: string, signal?: AbortSignal): Promise<Triage> {
  const response = await client.systemOne(
    {
      state: {
        request: prompt,
        task: "Classify this coding-agent request before execution.",
      },
      questions: {
        route: choice("Which model class is sufficient for this request?", {
          fast: "Direct lookup, extraction, routine formatting, or a small localized change with clear instructions.",
          powerful: "Debugging, high-stakes decisions, or ambiguous requirements needing strong reasoning.",
          architecture: "Architecture, broad refactoring, system design, or multi-file structural change.",
          designer: "UI, UX, frontend, visual design, interaction design, copy polish, or other design-focused work.",
        }),
        complexity: score("How complex is this request?", [
          "One-step lookup, extraction, or localized edit.",
          "Several related steps or moderate reasoning.",
          "Architecture, difficult debugging, broad refactoring, or high-stakes work.",
        ]),
        urgency: choice("How urgent is this request?", {
          low: "No time pressure; ordinary work.",
          normal: "Should be handled in the current work session.",
          high: "Explicit outage, security incident, data loss, or immediate production impact.",
        }),
        category: choice("What is the primary request category?", {
          lookup: "Find or explain existing information.",
          extraction: "Extract or classify known information.",
          localized_change: "Small, clearly bounded code or configuration change.",
          debugging: "Diagnose or fix incorrect behavior, failure, or crash.",
          architecture: "Design or change system structure, interfaces, or boundaries.",
          design: "Create or improve visual, interaction, or product design.",
          other: "Does not fit the listed categories.",
        }),
      },
    },
    { signal },
  );

  const answers = response.answers;
  return {
    route: answers.route.choice,
    routeConfidence: answers.route.confidence,
    complexity: answers.complexity.score,
    complexityConfidence: answers.complexity.confidence,
    urgency: answers.urgency.choice,
    category: answers.category.choice,
  };
}

export async function recallMemory(
  client: TypeSafeClient,
  hindsight: HindsightClient,
  prompt: string,
  signal?: AbortSignal,
): Promise<MemoryWorkflowResult> {
  const workflow = await runWorkflowGraph(
    [
      {
        id: "memory-gate",
        run: async (input: string, _results, workflowSignal) => decideMemoryGate(client, input, workflowSignal),
      },
      {
        id: "memory-recall",
        dependsOn: ["memory-gate"],
        run: async (input: string, results, workflowSignal) => {
          const gate = results.get("memory-gate") as { retrieve: boolean; probability: number };
          if (!gate.retrieve) return { gate, results: [] as HindsightRecall[] };
          try {
            return { gate, results: await hindsight.recall(input, workflowSignal) };
          } catch (error) {
            traceEvent("memory_recall_error", { error: String(error) });
            return { gate, results: [] as HindsightRecall[] };
          }
        },
      },
    ],
    prompt,
    { signal, onEvent: (workflowEvent) => traceEvent("workflow", workflowEvent) },
  ).catch((error) => {
    traceEvent("memory_gate_error", { error: String(error) });
    return new Map<string, unknown>([
      ["memory-recall", { gate: { retrieve: false, probability: 0 }, results: [] as HindsightRecall[] }],
    ]);
  });
  return workflow.get("memory-recall") as MemoryWorkflowResult;
}


function isRecord(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

function contentText(content: unknown): string {
  if (typeof content === "string") return content;
  if (!Array.isArray(content)) return "";
  return content
    .map((block) => {
      if (!isRecord(block) || block.type !== "text" || typeof block.text !== "string") return "";
      return block.text;
    })
    .filter(Boolean)
    .join("\n");
}

function toJevMessages(messages: readonly unknown[]): CompactionMessage[] {
  return messages.map((message) => {
    if (!isRecord(message)) return { role: "user", text: "", toolUses: [] };
    const role = message.role;

    if (role === "assistant") {
      const content = Array.isArray(message.content) ? message.content : [];
      const toolUses = content
        .filter((block) => isRecord(block) && block.type === "toolCall")
        .map((block) => {
          const id = isRecord(block) ? block.id : undefined;
          const name = isRecord(block) ? block.name : undefined;
          const args = isRecord(block) ? block.arguments : undefined;
          return {
            tool_use_id: String(id ?? "unknown"),
            tool: String(name ?? "unknown"),
            input: isRecord(args) ? args : {},
          };
        });
      return { role: "assistant", text: contentText(message.content), toolUses };
    }

    if (role === "toolResult") {
      return {
        role: "user",
        text: "",
        toolUses: [],
        toolResults: [
          {
            tool_use_id: String(message.toolCallId ?? "unknown"),
            text: contentText(message.content),
            isError: Boolean(message.isError),
          },
        ],
      };
    }

    if (role === "user") {
      return { role: "user", text: contentText(message.content), toolUses: [] };
    }

    if (role === "bashExecution") {
      return {
        role: "user",
        text: `$ ${String(message.command ?? "")}\n${String(message.output ?? "")}`,
        toolUses: [],
      };
    }

    return {
      role: "user",
      text: String(message.summary ?? message.content ?? ""),
      toolUses: [],
    };
  });
}

function renderJevMessages(messages: readonly CompactionMessage[]): string {
  return messages
    .map((message) => {
      const lines: string[] = [];
      if (message.text.trim()) lines.push(`[${message.role}] ${message.text}`);
      for (const tool of message.toolUses) {
        lines.push(`[tool call ${tool.tool_use_id}] ${tool.tool} ${JSON.stringify(tool.input)}`);
      }
      for (const result of message.toolResults ?? []) {
        lines.push(
          `[tool result ${result.tool_use_id}${result.isError ? " error" : ""}] ${result.text}`,
        );
      }
      return lines.join("\n");
    })
    .filter(Boolean)
    .join("\n\n");
}

function collectCompactionCalls(
  messages: readonly CompactionMessage[],
  preserveRecentMessages: number,
): CompactionCall[] {
  const results = new Map<string, { index: number; result: CompactionToolResult }>();
  messages.forEach((message, index) => {
    for (const result of message.toolResults ?? []) {
      results.set(result.tool_use_id, { index, result });
    }
  });

  const calls: CompactionCall[] = [];
  messages.forEach((message, callIndex) => {
    for (const tool of message.toolUses) {
      const found = results.get(tool.tool_use_id);
      if (!found) continue;
      calls.push({
        id: `t${calls.length + 1}`,
        toolUseId: tool.tool_use_id,
        tool,
        result: found.result,
        callIndex,
        resultIndex: found.index,
        pinned:
          callIndex === 0 ||
          found.index === 0 ||
          callIndex >= messages.length - preserveRecentMessages ||
          found.index >= messages.length - preserveRecentMessages,
      });
    }
  });
  return calls;
}

function compactionState(
  messages: readonly CompactionMessage[],
  calls: readonly CompactionCall[],
  goal: string,
): { context: string; goal: string; history: unknown[] } {
  const byIndex = new Map<number, CompactionCall[]>();
  for (const call of calls) {
    const current = byIndex.get(call.callIndex) ?? [];
    current.push(call);
    byIndex.set(call.callIndex, current);
  }

  const history = messages
    .map((message, index) => {
      const toolCalls = (byIndex.get(index) ?? []).map((call) => ({
        id: call.id,
        tool: call.tool.tool,
        input: JSON.stringify(call.tool.input).slice(0, 1000),
        result: `${call.result.isError ? "error" : "ok"}, ${call.result.text.length} chars (omitted)`,
      }));
      return {
        i: index,
        role: message.role,
        text: message.text.length > 2000 ? `${message.text.slice(0, 1800)}…` : message.text,
        tool_calls: toolCalls.length > 0 ? toolCalls : undefined,
      };
    })
    .filter((entry) => entry.text.trim().length > 0 || entry.tool_calls);

  const state = {
    context:
      "A coding-agent conversation is being compacted. Decide which completed tool calls and outputs must remain verbatim. User and assistant text is preserved.",
    goal,
    history,
  };
  while (JSON.stringify(state).length > 90000 && state.history.length > 1) {
    state.history.splice(1, 1);
  }
  return state;
}

function truncateCompactionResult(text: string): string {
  if (text.length <= 300) return text;
  return `${text.slice(0, 300)}\n[JeV compaction truncated ${text.length - 300} chars; re-run the tool if needed]`;
}

function applyCompactionDecisions(
  messages: readonly CompactionMessage[],
  calls: readonly CompactionCall[],
  decisions: readonly CompactionDecision[],
): CompactionMessage[] {
  const actions = new Map(decisions.map((decision) => [decision.id, decision.action]));
  const callByToolId = new Map(calls.map((call) => [call.toolUseId, call]));

  return messages
    .map((message) => {
      const toolUses = message.toolUses.filter((tool) => {
        const call = callByToolId.get(tool.tool_use_id);
        return !call || actions.get(call.id) !== "drop_call";
      });
      const toolResults = (message.toolResults ?? [])
        .filter((result) => {
          const call = callByToolId.get(result.tool_use_id);
          return !call || actions.get(call.id) !== "drop_call";
        })
        .map((result) => {
          const call = callByToolId.get(result.tool_use_id);
          return call && actions.get(call.id) === "drop_result"
            ? { ...result, text: truncateCompactionResult(result.text) }
            : result;
        });

      if (message.text.trim() || toolUses.length > 0 || toolResults.length > 0) {
        return {
          ...message,
          toolUses,
          toolResults: toolResults.length > 0 ? toolResults : undefined,
        };
      }
      return undefined;
    })
    .filter((message): message is CompactionMessage => Boolean(message));
}

async function compactWithJev(
  client: TypeSafeClient,
  messages: readonly CompactionMessage[],
  goal: string,
  signal: AbortSignal,
): Promise<CompactionResult> {
  const calls = collectCompactionCalls(messages, 0);
  const candidates = calls.filter((call) => !call.pinned);
  if (candidates.length === 0) {
    return { messages: [...messages], decisions: [], stats: { calls: 0, callsDropped: 0, resultsDropped: 0 } };
  }

  const state = compactionState(messages, calls, goal);
  const decisions: CompactionDecision[] = [];
  for (let offset = 0; offset < candidates.length; offset += 20) {
    const batch = candidates.slice(offset, offset + 20);
    const questions: Record<string, { type: "noul"; instructions: string }> = {};
    for (const call of batch) {
      questions[`call_${call.id}`] = {
        type: "noul",
        instructions: `Knowing that ${call.tool.tool} was called with this input still matters for the ongoing task.`,
      };
      questions[`result_${call.id}`] = {
        type: "noul",
        instructions: `The full ${call.result.text.length}-character result from ${call.tool.tool} is still needed verbatim and cannot be safely re-created by rerunning the tool.`,
      };
    }

    const response = await client.systemOne(
      { model: JEV_MODEL, state, questions },
      { signal },
    );
    for (const call of batch) {
      const callAnswer = response.answers[`call_${call.id}`];
      const resultAnswer = response.answers[`result_${call.id}`];
      const keepCall =
        "noul" in callAnswer && typeof callAnswer.noul === "number" ? callAnswer.noul : 1;
      const keepResult =
        "noul" in resultAnswer && typeof resultAnswer.noul === "number"
          ? resultAnswer.noul
          : 1;
      const action =
        keepResult >= 0.5 ? "keep" : keepCall >= 0.5 ? "drop_result" : "drop_call";
      decisions.push({ id: call.id, tool: call.tool.tool, action, keepCall, keepResult });
    }
  }

  const kept = calls
    .filter((call) => call.pinned)
    .map((call) => ({
      id: call.id,
      tool: call.tool.tool,
      action: "keep" as const,
      keepCall: 1,
      keepResult: 1,
    }));
  const allDecisions = [...kept, ...decisions];
  return {
    messages: applyCompactionDecisions(messages, calls, allDecisions),
    decisions: allDecisions,
    stats: {
      calls: calls.length,
      callsDropped: allDecisions.filter((decision) => decision.action === "drop_call").length,
      resultsDropped: allDecisions.filter((decision) => decision.action === "drop_result").length,
    },
  };
}

function fileOperationTags(fileOps: unknown): string {
  if (!isRecord(fileOps)) return "";
  const readFiles = Array.isArray(fileOps.readFiles) ? fileOps.readFiles : [];
  const modifiedFiles = Array.isArray(fileOps.modifiedFiles) ? fileOps.modifiedFiles : [];
  const tags: string[] = [];
  if (readFiles.length > 0) tags.push(`<read-files>\n${readFiles.join("\n")}\n</read-files>`);
  if (modifiedFiles.length > 0) {
    tags.push(`<modified-files>\n${modifiedFiles.join("\n")}\n</modified-files>`);
  }
  return tags.join("\n");
}


function lastAssistantText(messages: readonly unknown[]): string {
  return messages
    .filter((message) => isRecord(message) && message.role === "assistant")
    .map((message) => contentText(message.content))
    .filter(Boolean)
    .pop() ?? "";
}

function sessionKey(ctx: ExtensionContext): string {
  return hashText(ctx.sessionManager.getSessionFile() ?? "ephemeral");
}

const MEMORY_KEEP_THRESHOLD = 0.65;

function envToggleEnabled(value: string | undefined): boolean {
  return value === undefined || !["0", "false", "off", "no"].includes(value.trim().toLowerCase());
}

const registered = new WeakSet<ExtensionAPI>();

export default function jevExtension(pi: ExtensionAPI) {
  if (registered.has(pi)) return;
  registered.add(pi);
  const client = createClient();
  const hindsight = new HindsightClient();
  const refinements: RefinementRecord[] = [];
  let refinementInFlight = false;
  let completedTurns = 0;
  const configuredAutoRefineTurns = Number(process.env.PI_JEV_AUTO_REFINE_TURNS ?? 10);
  const autoRefineTurns =
    Number.isFinite(configuredAutoRefineTurns) && configuredAutoRefineTurns >= 1
      ? Math.floor(configuredAutoRefineTurns)
      : 10;
  let autoRefineEnabled = envToggleEnabled(process.env.PI_JEV_AUTO_REFINE);

  pi.registerTool({
    name: "jev_test_quality",
    label: "Jev test quality judge",
    description: "Judge whether a generated test is contract-level or bullshit. Requires contract, test source, and baseline result.",
    promptSnippet: "Jev judges test quality; deterministic execution remains authoritative.",
    promptGuidelines: [
      "Provide the public contract or explicit requirement, not only implementation details.",
      "Treat the returned verdict as review evidence, never as proof that a test is correct.",
      "Reject tautological, implementation-mirroring, private-state, and mock-call-order assertions unless explicitly contractual.",
    ],
    parameters: Type.Object({
      contract: Type.String(),
      testSource: Type.String(),
      baselineResult: Type.String(),
      changedFiles: Type.Optional(Type.String()),
    }),
    async execute(_toolCallId, params, signal) {
      if (!client) {
        return {
          content: [{ type: "text", text: "Jev unavailable: configure the Jev API key." }],
          details: { available: false },
        };
      }
      const input = params as TestQualityInput;
      const judgment = await judgeTestQuality(client, input, signal);
      traceEvent("test_quality_judgment", { verdict: judgment.verdict, ...judgment.probabilities });
      return { content: [{ type: "text", text: JSON.stringify(judgment, null, 2) }], details: judgment };
    },
  });

  pi.registerTool({
    name: "jev_mutation_judge",
    label: "Jev mutation judge",
    description: "Judge whether a mutation result is a meaningful behavioral kill, test gap, equivalent mutant, or runner failure.",
    promptSnippet: "Jev interprets mutation evidence; the mutation runner and contract remain authoritative.",
    promptGuidelines: [
      "Supply the original and mutated code, public contract, test source, runner status, and runner output.",
      "Do not treat a surviving mutant as a defect without checking equivalence and scope.",
      "Do not treat a timeout or crash as a behavioral kill.",
    ],
    parameters: Type.Object({
      contract: Type.String(),
      originalCode: Type.String(),
      mutatedCode: Type.String(),
      testSource: Type.String(),
      runnerStatus: Type.Union([
        Type.Literal("killed"),
        Type.Literal("survived"),
        Type.Literal("no-coverage"),
        Type.Literal("timeout"),
        Type.Literal("error"),
      ]),
      runnerOutput: Type.String(),
    }),
    async execute(_toolCallId, params, signal) {
      if (!client) {
        return {
          content: [{ type: "text", text: "Jev unavailable: configure the Jev API key." }],
          details: { available: false },
        };
      }
      const input = params as MutationJudgmentInput;
      const judgment = await judgeMutation(client, input, signal);
      traceEvent("mutation_judgment", { runnerStatus: input.runnerStatus, verdict: judgment.verdict, ...judgment.probabilities });
      return { content: [{ type: "text", text: JSON.stringify(judgment, null, 2) }], details: judgment };
    },
  });

  function branchText(ctx: ExtensionContext): string {
    return ctx.sessionManager
      .getBranch()
      .slice(-80)
      .map((entry) => JSON.stringify(entry))
      .join("\n")
      .slice(-18_000);
  }

  async function runRefinement(ctx: ExtensionContext, trigger: "manual" | "auto"): Promise<boolean> {
    if (refinementInFlight || (trigger === "auto" && !client)) return false;
    const trajectory = branchText(ctx);
    if (!trajectory.trim()) return false;

    refinementInFlight = true;
    try {
      const decision =
        trigger === "manual"
          ? { shouldRefine: true, probability: 1 }
          : await reviewRefinement(client!, trajectory, refinements, ctx.signal);
      traceEvent("refinement_gate", {
        trigger,
        scope: "local",
        decision: decision.shouldRefine ? "run" : "skip",
        probability: decision.probability,
        threshold: refinementThreshold(),
      });
      if (!decision.shouldRefine) {
        ctx.ui.setStatus("jev", `Jev refinement skipped · ${(decision.probability * 100).toFixed(0)}%`);
        return false;
      }

      const summary = await generateRefinement(ctx, trajectory, ctx.signal);
      if (!summary) {
        traceEvent("refinement_complete", { trigger, applied: false, reason: "no_refinement" });
        return false;
      }

      const record: RefinementRecord = {
        id: hashText(`${Date.now()}-${summary}`),
        summary,
        scope: "local",
        probability: decision.probability,
        createdAt: new Date().toISOString(),
      };
      refinements.push(record);
      pi.appendEntry("jev-refinement", record);
      traceEvent("refinement_complete", {
        trigger,
        applied: true,
        id: record.id,
        summary: record.summary,
      });
      ctx.ui.setStatus("jev", `Jev refinement applied · ${record.summary.slice(0, 80)}`);
      return true;
    } catch (error) {
      traceEvent("refinement_gate_error", { trigger, scope: "local", error: String(error) });
      ctx.ui.notify(`Jev refinement failed; no lesson applied (${String(error)}).`, "warning");
      return false;
    } finally {
      refinementInFlight = false;
    }
  }

  const restoreRefinements = (ctx: ExtensionContext) => {
    refinements.length = 0;
    for (const entry of ctx.sessionManager.getBranch()) {
      if (entry.type !== "custom" || entry.customType !== "jev-refinement") continue;
      const data = entry.data as Partial<RefinementRecord>;
      if (
        typeof data.id === "string" &&
        typeof data.summary === "string" &&
        (data.scope === "local" || data.scope === "global") &&
        typeof data.probability === "number" &&
        typeof data.createdAt === "string"
      ) {
        refinements.push(data as RefinementRecord);
      }
    }
  };
  pi.on("session_start", async (_event, ctx) => restoreRefinements(ctx));
  pi.on("session_tree", async (_event, ctx) => restoreRefinements(ctx));

  function autoRefineSummary(): string {
    return autoRefineEnabled
      ? `Automatic Jev refinement enabled; checking every ${autoRefineTurns} completed turns.`
      : "Automatic Jev refinement disabled for this session.";
  }

  pi.registerCommand("refine", {
    description: "Run Jev refinement or toggle automatic refinement (on/off/status)",
    handler: async (args, ctx) => {
      const mode = args.trim().toLowerCase();
      if (mode === "auto" || mode === "on" || mode === "off" || mode === "toggle" || mode === "status") {
        if (mode === "toggle") {
          autoRefineEnabled = !autoRefineEnabled;
        } else if (mode !== "status") {
          autoRefineEnabled = mode !== "off";
        }
        if (mode !== "status") {
          traceEvent("refinement_auto_toggle", { enabled: autoRefineEnabled, source: "command" });
        }
        const summary = autoRefineSummary();
        ctx.ui.setStatus("jev", autoRefineEnabled ? "Jev auto-refinement" : "Jev refinement manual");
        ctx.ui.notify(summary, autoRefineEnabled ? "info" : "warning");
        return;
      }

      await ctx.waitForIdle();
      const applied = await runRefinement(ctx, "manual");
      ctx.ui.notify(applied ? "Jev refinement applied to this Pi session." : "No refinement applied.", applied ? "info" : "warning");
    },
  });

  pi.on("agent_end", async (_event, ctx) => {
    completedTurns += 1;
    if (autoRefineEnabled && completedTurns % autoRefineTurns === 0) {
      await runRefinement(ctx, "auto");
    }
  });

  let lastTriage: Triage | undefined;
  let activePrompt: { text: string; traceId: string; turnIndex: number } | undefined;
  pi.on("turn_start", async (event, ctx) => {
    if (activePrompt) activePrompt.turnIndex = event.turnIndex;
    traceEvent("turn_start", { turnIndex: event.turnIndex, session: sessionKey(ctx) });
  });

  pi.on("turn_end", async (event, ctx) => {
    traceEvent("turn_end", {
      turnIndex: event.turnIndex,
      session: sessionKey(ctx),
      hasMessage: Boolean(event.message),
      toolResults: Array.isArray(event.toolResults) ? event.toolResults.length : 0,
    });
  });

  pi.on("session_before_compact", async (event, ctx) => {
    if (!client) return;

    const messages = toJevMessages(event.preparation.messagesToSummarize);
    const hasCompletedToolCall =
      messages.some((message) => message.toolUses.length > 0) &&
      messages.some((message) => (message.toolResults?.length ?? 0) > 0);
    if (!hasCompletedToolCall) return;

    try {
      const result = await compactWithJev(
        client,
        messages,
        event.customInstructions ?? "",
        event.signal,
      );
      if (result.stats.calls === 0) return;

      const summaryParts = [
        "## Jev-compacted history",
        "Jev removed stale tool calls and truncated results while preserving user and assistant text verbatim.",
        "",
        renderJevMessages(result.messages),
        fileOperationTags(event.preparation.fileOps),
      ].filter(Boolean);

      ctx.ui.setStatus(
        "jev",
        `Jev compacted ${result.stats.calls} calls · -${result.stats.callsDropped} calls · -${result.stats.resultsDropped} results`,
      );

      return {
        compaction: {
          summary: summaryParts.join("\n"),
          firstKeptEntryId: event.preparation.firstKeptEntryId,
          tokensBefore: event.preparation.tokensBefore,
          details: {
            jev: true,
            stats: result.stats,
            decisions: result.decisions,
          },
        },
      };
    } catch (error) {
      ctx.ui.notify(`Jev compaction failed; using Pi's built-in compaction (${String(error)}).`, "warning");
    }
  });



  pi.registerCommand("jev", {
    description: "Show the latest Jev triage and memory decision",
    handler: async (_args, ctx) => {
      if (!lastTriage) {
        ctx.ui.notify(client ? "Jev is ready; no request has been triaged yet." : "Jev is disabled: no API key configured.", "info");
        return;
      }
      ctx.ui.notify(
        `${lastTriage.route} route · ${lastTriage.category} · ${lastTriage.urgency} urgency · complexity ${lastTriage.complexity}`,
        "info",
      );
    },
  });

  pi.registerCommand("jev-approvals", {
    description: "Toggle Jev auto-run or show pending approval checkpoints (auto/on, prompt/off, toggle, status)",
    handler: async (args, ctx) => {
      const mode = args.trim().toLowerCase();
      if (mode === "" || mode === "auto" || mode === "on" || mode === "prompt" || mode === "off" || mode === "toggle") {
        const enabled = mode === "auto" || mode === "on"
          ? true
          : mode === "prompt" || mode === "off"
          ? false
          : !approvalBypassEnabled(pi);
        setApprovalBypass(pi, enabled);
        const autoRun = approvalBypassEnabled(pi);
        const status = autoRun
          ? "Jev auto-run enabled for this Pi process."
          : "Jev approval prompts enabled for this Pi process.";
        ctx.ui.setStatus("jev-approvals", autoRun ? "Jev auto-run" : "Jev approvals");
        ctx.ui.notify(status, autoRun ? "warning" : "info");
        return;
      }
      if (mode !== "status") {
        ctx.ui.notify("Usage: /jev-approvals [auto|on|prompt|off|toggle|status]", "warning");
        return;
      }
      const pending = pendingApprovals(ctx);
      const status = approvalBypassEnabled(pi)
        ? "Jev auto-run enabled for this Pi process."
        : "Jev approval prompts enabled for this Pi process.";
      if (pending.length === 0) {
        ctx.ui.notify(`${status}\nNo pending Jev approval checkpoints.`, "info");
        return;
      }
      ctx.ui.notify(
        `${status}\n${pending.map((record) => `${record.id}: ${record.tool} · ${record.severity}`).join("\n")}`,
        "warning",
      );
    },
  });

  pi.registerCommand("jev-eval", {
    description: "Run deterministic Jev checks, optionally with live judge cases",
    handler: async (args, ctx) => {
      const deterministic = runDeterministicEvals();
      const passed = deterministic.filter((item) => item.passed).length;
      const requestedJudge = args.trim() === "judge";
      if (requestedJudge && client) {
        const judged = await runJudgeEvals(client, ctx.signal);
        const judgePassed = judged.filter((item) => item.passed).length;
        ctx.ui.notify(`Deterministic ${passed}/${deterministic.length}; judge ${judgePassed}/${judged.length}`, judgePassed === judged.length ? "info" : "warning");
        traceEvent("eval_judge", { deterministic: `${passed}/${deterministic.length}`, judge: `${judgePassed}/${judged.length}` });
        return;
      }
      ctx.ui.notify(`Deterministic ${passed}/${deterministic.length}${requestedJudge ? "; judge unavailable" : ""}`, passed === deterministic.length ? "info" : "warning");
      traceEvent("eval_deterministic", { passed, total: deterministic.length });
    },
  });

  pi.on("after_provider_response", async (event, ctx) => {
    if (event.status >= 400) {
      ctx.ui.notify(`Pinned Astra model returned HTTP ${event.status}; no model fallback was applied.`, "warning");
      traceEvent("model_failure", {
        model: MAIN_MODEL,
        status: event.status,
        fallback: false,
      });
    }
  });

  pi.on("before_agent_start", async (event, ctx) => {
    try {
      const mainModel = pickConfiguredModel(ctx, MAIN_MODEL);
      if (!mainModel) throw new Error(`Pinned model is unavailable: ${MAIN_MODEL}`);
      if (
        process.env.PI_RLM_EXPLICIT_MODEL !== "1" &&
        `${mainModel.provider}/${mainModel.id}` !== `${ctx.model?.provider}/${ctx.model?.id}`
      ) {
        await pi.setModel(mainModel);
      }
      if (!client || !event.prompt.trim()) {
        if (!client) ctx.ui.setStatus("jev", "Astra pinned · Jev off");
        return;
      }

      const [nextTriage, memory] = await Promise.all([
        triage(client, event.prompt, ctx.signal),
        recallMemory(client, hindsight, event.prompt, ctx.signal),
      ]);
      lastTriage = nextTriage;
      activePrompt = { text: event.prompt, traceId: hashText(event.prompt), turnIndex: 0 };
      traceEvent("triage", {
        traceId: activePrompt.traceId,
        route: nextTriage.route,
        category: nextTriage.category,
        urgency: nextTriage.urgency,
        complexity: nextTriage.complexity,
      });

      traceEvent("memory_gate", {
        traceId: activePrompt.traceId,
        retrieve: memory.gate.retrieve,
        probability: memory.gate.probability,
        resultCount: memory.results.length,
      });
      const recalled = formatRecall(memory.results);
      const childRoute =
        nextTriage.route === "fast" &&
        (nextTriage.routeConfidence < ROUTE_THRESHOLD || nextTriage.complexity >= 2)
          ? "powerful"
          : nextTriage.route;
      traceEvent("model_route", {
        route: childRoute,
        intentRoute: nextTriage.route,
        category: nextTriage.category,
        model: MAIN_MODEL,
        childModel: configuredChildModelForRoute(childRoute),
        thinking: configuredChildThinkingForRoute(childRoute),
        pinned: true,
      });
      ctx.ui.setStatus(
        "jev",
        `Astra pinned · intent ${nextTriage.category} · child ${childRoute} · ${nextTriage.urgency} · c${nextTriage.complexity}`,
      );
      const priorRefinements = refinementContext(refinements);
      const context = [recalled, priorRefinements].filter(Boolean).join("\n\n");
      if (context) {
        return {
          message: {
            customType: "jev-context",
            content: context,
            display: false,
          },
        };
      }
    } catch (error) {
      ctx.ui.setStatus("jev", "Astra pinned · Jev unavailable");
      ctx.ui.notify(`Jev triage/memory failed; keeping the pinned model (${String(error)}).`, "warning");
      traceEvent("before_agent_start_error", { error: String(error), model: MAIN_MODEL });
    }
  });

  pi.on("agent_end", async (event, ctx) => {
    if (!client || !activePrompt) return;
    const prompt = activePrompt;
    activePrompt = undefined;
    const responseText = lastAssistantText(event.messages);
    if (!responseText.trim()) return;

    try {
      const policy = await decideMemoryPolicy(client, prompt.text, responseText, ctx.signal);
      traceEvent("memory_policy", {
        traceId: prompt.traceId,
        action: policy.action,
        confidence: policy.confidence,
        responseHash: hashText(responseText),
      });
      if (policy.action !== "keep" || policy.confidence < MEMORY_KEEP_THRESHOLD) return;

      await hindsight.retain({
        content: `[User]\\n${prompt.text.slice(0, 6000)}\\n\\n[Assistant]\\n${responseText.slice(0, 6000)}`,
        context: "Pi coding-agent interaction",
        document_id: `pi-${sessionKey(ctx)}-${prompt.traceId}`,
        metadata: { source: "pi", trace_id: prompt.traceId },
        tags: ["source:pi"],
        update_mode: "replace",
      }, ctx.signal);
      traceEvent("memory_retained", { traceId: prompt.traceId, confidence: policy.confidence });
    } catch (error) {
      traceEvent("memory_policy_error", { traceId: prompt.traceId, error: String(error) });
    }
  });

  pi.on("tool_call", async (event, ctx) => {
    if (!["bash", "write", "edit", "ipython"].includes(event.toolName)) return;
    if (event.toolName === "read" || event.toolName === "grep" || event.toolName === "find") return;

    if (
      !isToolCallEventType("bash", event) &&
      !isToolCallEventType("write", event) &&
      !isToolCallEventType("edit", event) &&
      !isToolCallEventType("ipython", event)
    ) {
      return;
    }

    return guardToolCall(client, pi, ctx, event.toolName, event.input, ctx.signal);
  });
}
