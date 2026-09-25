import { readFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";
import { choice, noul, TypeSafeClient } from "@typesafe-ai/sdk";
import type { ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent";
import { recordApproval, type ApprovalRecord } from "./approval.ts";
import { hashText, traceEvent } from "./telemetry.ts";

const SCREEN_THRESHOLD = 0.7;
const approvalModeStateKey = Symbol.for("pi.jev.approval-mode");
type ApprovalModeState = { inheritedAutoRun: boolean; override?: boolean };

function isApprovalModeState(value: unknown): value is ApprovalModeState {
  return typeof value === "object" && value !== null &&
    "inheritedAutoRun" in value && typeof value.inheritedAutoRun === "boolean" &&
    (!("override" in value) || typeof value.override === "boolean");
}

const storedApprovalModeState: unknown = Reflect.get(globalThis, approvalModeStateKey);
let approvalModeState: ApprovalModeState;
if (isApprovalModeState(storedApprovalModeState)) {
  approvalModeState = storedApprovalModeState;
} else {
  approvalModeState = { inheritedAutoRun: (process.env.PI_JEV_AUTO_RUN ?? "1") === "1" };
  Reflect.set(globalThis, approvalModeStateKey, approvalModeState);
}

export function setApprovalBypass(_pi: ExtensionAPI, enabled: boolean): void {
  approvalModeState.override = enabled;
}

export function approvalBypassEnabled(_pi: ExtensionAPI): boolean {
  return approvalModeState.override ?? approvalModeState.inheritedAutoRun;
}

type ToolScreen = {
  risky: number;
  severity: "low" | "medium" | "high";
  severityConfidence: number;
  requiresConfirmation: number;
};

export function createClient(): TypeSafeClient | undefined {
  let apiKey = process.env.TYPESAFE_API_KEY?.trim();
  if (!apiKey) {
    try {
      apiKey = readFileSync(join(homedir(), ".pi", "agent", "jev-api-key"), "utf8").trim();
    } catch {
      return undefined;
    }
  }
  if (!apiKey) return undefined;
  return new TypeSafeClient({
    apiKey,
    defaultModel: process.env.PI_JEV_MODEL ?? "jev-latest",
    timeout: 5000,
    retry: { maxRetries: 1 },
  });
}

function obviousRisk(toolName: string, input: unknown): boolean {
  const text = `${toolName} ${JSON.stringify(input)}`.toLowerCase();
  return /\brm\s+-rf\b|\bsudo\b|\bdoas\b|git\s+reset\s+--hard|git\s+clean\s+-f|\bmkfs\b|\bdd\s+if=|\bkill\s+-9\b|terraform\s+destroy|kubectl\s+delete|shutil\.rmtree|(?:os|pathlib)\.(?:remove|unlink)|\bsubprocess\.(?:run|popen|call)\b/.test(text);
}

async function screenTool(client: TypeSafeClient, toolName: string, input: unknown, signal?: AbortSignal): Promise<ToolScreen> {
  const response = await client.systemOne({
    state: {
      tool: toolName,
      input: JSON.stringify(input),
      task: "Screen this proposed coding-agent tool call before execution.",
    },
    questions: {
      risky: noul("Could this tool call cause destructive, irreversible, privileged, or sensitive side effects?", {
        true: "It can delete, overwrite, publish, exfiltrate, alter permissions, affect production, or otherwise cause consequential side effects.",
        false: "It is read-only or has a reversible, local, low-impact effect.",
      }),
      severity: choice("If this tool call has side effects, how severe could the impact be?", {
        low: "Local and easily reversible.",
        medium: "Could modify user work, dependencies, credentials, or external state but is recoverable.",
        high: "Destructive, privileged, irreversible, production-impacting, or likely to expose sensitive data.",
      }),
      requiresConfirmation: noul("Should a human confirm this tool call before it runs?", {
        true: "The call deserves explicit human approval because of its possible impact or ambiguity.",
        false: "The call is routine and safe to execute without an extra confirmation.",
      }),
    },
  }, { signal });
  return {
    risky: response.answers.risky.noul,
    severity: response.answers.severity.choice,
    severityConfidence: response.answers.severity.confidence,
    requiresConfirmation: response.answers.requiresConfirmation.noul,
  };
}

export async function guardToolCall(
  client: TypeSafeClient | undefined,
  pi: ExtensionAPI,
  ctx: ExtensionContext,
  toolName: string,
  input: unknown,
  signal?: AbortSignal,
): Promise<{ block: true; reason: string } | undefined> {
  signal?.throwIfAborted();
  if (approvalBypassEnabled(pi)) {
    traceEvent("tool_screen_bypass", {
      tool: toolName,
      inputHash: hashText(JSON.stringify(input)),
      reason: "session-auto-run",
    });
    return;
  }
  let severity = "high";
  let reason = "Deterministic risk check requires approval.";
  if (client) {
    try {
      const result = await screenTool(client, toolName, input, signal);
      signal?.throwIfAborted();
      const needsConfirmation = obviousRisk(toolName, input) ||
        result.risky >= SCREEN_THRESHOLD || result.requiresConfirmation >= SCREEN_THRESHOLD ||
        (result.severity === "high" && result.severityConfidence >= 0.5);
      traceEvent("tool_screen", {
        tool: toolName, inputHash: hashText(JSON.stringify(input)),
        risky: result.risky, severity: result.severity,
        requiresConfirmation: result.requiresConfirmation, blocked: needsConfirmation && !ctx.hasUI,
      });
      if (!needsConfirmation) return;
      severity = result.severity;
      reason = `Risk ${(result.risky * 100).toFixed(0)}% · ${severity} severity.`;
    } catch (error) {
      signal?.throwIfAborted();
      traceEvent("tool_screen_error", { tool: toolName, error: String(error) });
      ctx.ui.setStatus("jev-approvals", "Jev screen unavailable");
      if (obviousRisk(toolName, input)) {
        return { block: true, reason: `Jev screening failed for an obviously risky call: ${String(error)}` };
      }
      return;
    }
  } else if (!obviousRisk(toolName, input)) {
    return;
  }

  const approval: ApprovalRecord = {
    id: `approval-${hashText(`${Date.now()}-${toolName}-${JSON.stringify(input)}`)}`,
    status: "pending", tool: toolName, inputHash: hashText(JSON.stringify(input)),
    severity, timestamp: new Date().toISOString(),
  };
  recordApproval(pi, approval);
  ctx.ui.setStatus("jev-approvals", `Jev flagged ${toolName} · ${severity}`);
  if (!ctx.hasUI) {
    return { block: true, reason: `Approval checkpoint ${approval.id} requires an interactive session.` };
  }
  const approved = await ctx.ui.confirm(`Jev flagged ${toolName}`, `${reason} Execute this tool call?\n\n${JSON.stringify(input)}`);
  recordApproval(pi, { ...approval, status: approved ? "approved" : "rejected" });
  signal?.throwIfAborted();
  if (!approved) return { block: true, reason: "Blocked after Jev screening." };
}
