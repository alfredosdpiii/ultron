import { noul, TypeSafeClient } from "@typesafe-ai/sdk";

export type TestQualityInput = {
  contract: string;
  testSource: string;
  baselineResult: string;
  changedFiles?: string;
};

export type TestQualityJudgment = {
  verdict: "credible" | "needs_review" | "bullshit";
  probabilities: {
    bullshit: number;
    contractSupported: number;
    implementationCoupled: number;
    selfOracle: number;
    observableBehavior: number;
  };
  reasons: string[];
};

export type MutationStatus = "killed" | "survived" | "no-coverage" | "timeout" | "error";

export type MutationJudgmentInput = {
  contract: string;
  originalCode: string;
  mutatedCode: string;
  testSource: string;
  runnerStatus: MutationStatus;
  runnerOutput: string;
};

export type MutationJudgment = {
  verdict: "behavioral_kill" | "incidental_kill" | "test_gap" | "likely_equivalent" | "runner_failure";
  probabilities: {
    meaningfulMutant: number;
    shouldKill: number;
    likelyEquivalent: number;
    behavioralEvidence: number;
    testGap: number;
  };
  reasons: string[];
};

const MODEL = process.env.PI_JEV_MODEL ?? "jev-latest";
const THRESHOLD = 0.72;
const MAX_FIELD_CHARS = 12_000;

function clip(value: string | undefined): string {
  return (value ?? "").slice(-MAX_FIELD_CHARS);
}

function redact(value: string): string {
  return value
    .replace(/\b(?:sk|ghp|github_pat|xox[baprs]|AIza|AKIA)[A-Za-z0-9_-]{8,}\b/gi, "[REDACTED_TOKEN]")
    .replace(/\b(?:api[_ -]?key|access[_ -]?token|auth[_ -]?token|password|passwd|secret|private[_ -]?key)\s*[:=]\s*\S+/gi, "[REDACTED_SECRET]")
    .replace(/\bbearer\s+[A-Za-z0-9._~+/=-]{16,}\b/gi, "Bearer [REDACTED_TOKEN]");
}

function stateFor(input: TestQualityInput): Record<string, string> {
  return {
    task: "Judge whether a generated software test is a useful contract-level test.",
    contract: redact(clip(input.contract)),
    test_source: redact(clip(input.testSource)),
    baseline_result: redact(clip(input.baselineResult)),
    changed_files: redact(clip(input.changedFiles)),
  };
}

function mutationStateFor(input: MutationJudgmentInput): Record<string, string> {
  return {
    task: "Judge what a mutation-testing result says about a test suite without treating the language model as the executable oracle.",
    contract: redact(clip(input.contract)),
    original_code: redact(clip(input.originalCode)),
    mutated_code: redact(clip(input.mutatedCode)),
    test_source: redact(clip(input.testSource)),
    runner_status: input.runnerStatus,
    runner_output: redact(clip(input.runnerOutput)),
  };
}

function probability(answers: Record<string, { noul: number }>, key: string): number {
  const value = Number(answers[key]?.noul);
  return Number.isFinite(value) ? Math.max(0, Math.min(1, value)) : 0;
}

export async function judgeTestQuality(
  client: TypeSafeClient,
  input: TestQualityInput,
  signal?: AbortSignal,
): Promise<TestQualityJudgment> {
  const response = await client.systemOne(
    {
      model: MODEL,
      state: stateFor(input),
      questions: {
        bullshit: noul(
          "Is this test mostly bullshit: tautological, vacuous, implementation-mirroring, or asserting setup rather than meaningful behavior?",
          {
            true: "The test would pass while the intended behavior is broken or is coupled to irrelevant implementation details.",
            false: "The test checks a meaningful, externally observable behavior and could fail on a real regression.",
          },
        ),
        contractSupported: noul(
          "Is the asserted behavior supported by the supplied public contract or an explicit requirement?",
          {
            true: "The expected outcome is grounded in an explicit contract, invariant, example, reference, or property.",
            false: "The expected outcome is invented, incidental, or unsupported by the supplied evidence.",
          },
        ),
        implementationCoupled: noul(
          "Does this test unnecessarily assert private state, helper structure, exact mock calls, or representation details?",
          {
            true: "The test is likely to break during a behavior-preserving refactor.",
            false: "The test primarily exercises a public API and observable result.",
          },
        ),
        selfOracle: noul(
          "Does the test derive its expected value by repeating or directly calling the implementation under test?",
          {
            true: "The test can agree with the same bug because its oracle duplicates the implementation.",
            false: "The expected value is independently specified or computed.",
          },
        ),
        observableBehavior: noul(
          "Would this test fail for at least one realistic regression in the described behavior?",
          {
            true: "The test has a concrete observable failure mode tied to the contract.",
            false: "The test is unlikely to detect a meaningful regression.",
          },
        ),
      },
    },
    { signal },
  );

  const answers = response.answers as unknown as Record<string, { noul: number }>;
  const probabilities = {
    bullshit: probability(answers, "bullshit"),
    contractSupported: probability(answers, "contractSupported"),
    implementationCoupled: probability(answers, "implementationCoupled"),
    selfOracle: probability(answers, "selfOracle"),
    observableBehavior: probability(answers, "observableBehavior"),
  };
  const reasons: string[] = [];
  if (probabilities.contractSupported < THRESHOLD) reasons.push("expected behavior lacks clear contract evidence");
  if (probabilities.implementationCoupled >= THRESHOLD) reasons.push("assertion appears coupled to implementation or mock interaction");
  if (probabilities.selfOracle >= THRESHOLD) reasons.push("test may derive its oracle from the implementation under test");
  if (probabilities.observableBehavior < THRESHOLD) reasons.push("test has no strong observable regression signal");

  const verdict = probabilities.bullshit >= THRESHOLD || reasons.length >= 2
    ? "bullshit"
    : reasons.length === 1
      ? "needs_review"
      : "credible";
  return { verdict, probabilities, reasons };
}

export async function judgeMutation(
  client: TypeSafeClient,
  input: MutationJudgmentInput,
  signal?: AbortSignal,
): Promise<MutationJudgment> {
  const response = await client.systemOne(
    {
      model: MODEL,
      state: mutationStateFor(input),
      questions: {
        meaningfulMutant: noul(
          "Is the code change a meaningful mutation of behavior rather than an equivalent or invalid mutation?",
          {
            true: "The mutation can change a behavior required by the contract.",
            false: "The mutation is likely equivalent, unreachable, invalid, or outside the contract.",
          },
        ),
        shouldKill: noul(
          "Should a good contract-level test kill this mutant?",
          {
            true: "The mutant violates a stated behavior and should be detected.",
            false: "The mutant is equivalent, irrelevant, or not required by the supplied contract.",
          },
        ),
        likelyEquivalent: noul(
          "Is this likely an equivalent or out-of-scope mutant that should not be used to demand a new test?",
          {
            true: "The mutation does not create a distinguishable in-scope behavior under the contract.",
            false: "The mutation represents a distinguishable behavior gap.",
          },
        ),
        behavioralEvidence: noul(
          "If the runner says killed, does the available evidence indicate a behavioral assertion failure rather than timeout, crash, or incidental setup failure?",
          {
            true: "The mutant was rejected by an assertion tied to the intended behavior.",
            false: "The result may be an incidental failure, timeout, crash, or unrelated setup error.",
          },
        ),
        testGap: noul(
          "If the runner says survived or no-coverage, is there a meaningful missing test for an in-scope behavior?",
          {
            true: "A focused test could distinguish the mutant through a public observable contract.",
            false: "No justified test gap is established by the supplied evidence.",
          },
        ),
      },
    },
    { signal },
  );

  const answers = response.answers as unknown as Record<string, { noul: number }>;
  const probabilities = {
    meaningfulMutant: probability(answers, "meaningfulMutant"),
    shouldKill: probability(answers, "shouldKill"),
    likelyEquivalent: probability(answers, "likelyEquivalent"),
    behavioralEvidence: probability(answers, "behavioralEvidence"),
    testGap: probability(answers, "testGap"),
  };
  const reasons: string[] = [];
  let verdict: MutationJudgment["verdict"];
  if (input.runnerStatus === "timeout" || input.runnerStatus === "error") {
    verdict = "runner_failure";
    reasons.push("mutation runner did not produce a reliable test outcome");
  } else if (input.runnerStatus === "killed") {
    verdict = probabilities.behavioralEvidence >= THRESHOLD ? "behavioral_kill" : "incidental_kill";
    if (verdict === "incidental_kill") reasons.push("kill evidence is not clearly an assertion tied to the contract");
  } else if (probabilities.likelyEquivalent >= THRESHOLD || probabilities.meaningfulMutant < THRESHOLD) {
    verdict = "likely_equivalent";
    reasons.push("survivor is not established as a meaningful in-scope behavior change");
  } else if (probabilities.shouldKill >= THRESHOLD && probabilities.testGap >= THRESHOLD) {
    verdict = "test_gap";
    reasons.push("meaningful surviving mutation indicates a missing contract-level test");
  } else {
    verdict = "likely_equivalent";
    reasons.push("evidence is insufficient to demand a new test");
  }
  if (input.runnerStatus === "no-coverage") reasons.push("mutant was not exercised by the test suite");
  if (input.runnerStatus === "survived") reasons.push("mutant survived the executable test suite");
  return { verdict, probabilities, reasons };
}
