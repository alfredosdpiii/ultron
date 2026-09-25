import { choice, TypeSafeClient } from "@typesafe-ai/sdk";
import {
  enforceDeterministicMemoryPolicy,
  deterministicMemoryPolicy,
  MEMORY_RECALL_THRESHOLD,
  shouldRetrieveMemory,
} from "./memory.ts";

export type DeterministicEval = { name: string; passed: boolean; detail: string };

function expectAction(
  name: string,
  prompt: string,
  response: string,
  expected: "keep" | "skip" | "sensitive",
  detail: string,
): DeterministicEval {
  const actual = deterministicMemoryPolicy(prompt, response)?.action;
  return { name, passed: actual === expected, detail: `${detail}; got ${actual ?? "delegate"}` };
}

export function runDeterministicEvals(): DeterministicEval[] {
  const privateKeyHeader = ["-----BEGIN ", "PRIVATE ", "KEY-----"].join("");
  const cases: DeterministicEval[] = [
    {
      name: "recall threshold rejects below boundary",
      passed: !shouldRetrieveMemory(MEMORY_RECALL_THRESHOLD - 0.01),
      detail: `${MEMORY_RECALL_THRESHOLD - 0.01} must not retrieve`,
    },
    {
      name: "recall threshold accepts exact boundary",
      passed: shouldRetrieveMemory(MEMORY_RECALL_THRESHOLD),
      detail: `${MEMORY_RECALL_THRESHOLD} must retrieve`,
    },
    {
      name: "recall threshold accepts above boundary",
      passed: shouldRetrieveMemory(MEMORY_RECALL_THRESHOLD + 0.01),
      detail: `${MEMORY_RECALL_THRESHOLD + 0.01} must retrieve`,
    },
    expectAction(
      "API keys are never retained",
      "Remember this for later.",
      "The API key is sk-example-secret-value.",
      "sensitive",
      "credential-shaped content must be blocked",
    ),
    expectAction(
      "private keys are never retained",
      "Store this credential.",
      privateKeyHeader,
      "sensitive",
      "private-key material must be blocked",
    ),
    expectAction(
      "bearer tokens are never retained",
      "Keep this token.",
      "Bearer abcdefghijklmnopQRSTUV12",
      "sensitive",
      "bearer tokens must be blocked",
    ),
    expectAction(
      "sensitive content overrides a keep decision",
      "Remember my password: hunter2.",
      "I will remember it.",
      "sensitive",
      "deterministic guard must win over model output",
    ),
    expectAction(
      "ordinary explanations are skipped",
      "Explain what a mutex is.",
      "A mutex serializes access to shared state.",
      "skip",
      "self-contained explanations are ephemeral",
    ),
    {
      name: "durable preferences delegate to Jev",
      passed: deterministicMemoryPolicy("Remember that I prefer concise commit messages.", "Preference recorded.") === undefined,
      detail: "durable requests must not be misclassified as ephemeral",
    },
    {
      name: "non-sensitive durable content delegates to Jev",
      passed: deterministicMemoryPolicy("Remember my preferred editor.", "Neovim.") === undefined,
      detail: "ordinary durable facts require the model decision",
    },
    {
      name: "deterministic guard preserves safe model decisions",
      passed:
        enforceDeterministicMemoryPolicy("Remember my preferred editor.", "Neovim.", {
          action: "keep",
          confidence: 0.91,
        }).action === "keep",
      detail: "guard must not override safe durable content",
    },
    {
      name: "deterministic guard overrides unsafe model decisions",
      passed:
        enforceDeterministicMemoryPolicy("Remember my password: hunter2.", "Stored.", {
          action: "keep",
          confidence: 0.99,
        }).action === "sensitive",
      detail: "guard must override a mistaken keep decision",
    },
  ];
  return cases;
}

export async function runJudgeEvals(client: TypeSafeClient, signal?: AbortSignal) {
  const cases = [
    { input: "Remember that I prefer concise commit messages.", expected: "keep" },
    { input: "Explain what a mutex is.", expected: "skip" },
    { input: "My API key is sk-example-secret; keep it for later.", expected: "sensitive" },
  ];
  const results = [];
  for (const test of cases) {
    const response = await client.systemOne(
      {
        state: { interaction: test.input, task: "Classify whether this belongs in durable personal memory." },
        questions: {
          action: choice("What should happen to this interaction?", {
            keep: "Keep durable preferences, facts, decisions, or commitments.",
            skip: "Do not keep ephemeral or self-contained information.",
            sensitive: "Do not keep secrets or unusually sensitive data.",
          }),
        },
      },
      { signal },
    );
    const answer = response.answers.action;
    results.push({ input: test.input, expected: test.expected, actual: answer.choice, confidence: answer.confidence, passed: answer.choice === test.expected });
  }
  return results;
}
