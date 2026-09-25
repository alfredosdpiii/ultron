import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { afterEach, describe, expect, test } from "vitest";
import { RpcClient } from "../src/modes/rpc/rpc-client.ts";
import { ScriptedProvider, type ScriptedRequest, scriptedModelsJson } from "./support/scripted-provider.ts";

const cliPath = resolve(__dirname, "../src/cli.ts");
const sourceResolverPath = resolve(__dirname, "../src/experimental/source-resolver.ts");

const SKILL = `"""Most frequent word in a text (ties broken alphabetically)."""
import collections
import re

def top_word(text: str) -> str:
    """Return the most frequent lowercase word; ties go to the alphabetically first."""
    counts = collections.Counter(re.findall(r"[a-z']+", text.lower()))
    return min(counts, key=lambda word: (-counts[word], word))
`;
const SKILL_TEST = `from code_skills import top_word
def test_counts_and_ties():
    assert top_word.top_word("b a b a c") == "a"
    assert top_word.top_word("The cat. the DOG, the end") == "the"
`;

/**
 * The first run derives the procedure over several cells and saves it as a code skill; a later run sees the
 * skill in the rlm tool description, imports it, and answers in one cell.
 */
const DERIVE = [
	"text = open('corpus.txt').read()\nprint('WORDS', len(text.split()))",
	"import re, collections\ncounts = collections.Counter(re.findall(r\"[a-z']+\", text.lower()))\nprint('TOP3', counts.most_common(3))",
	"best = min(counts, key=lambda w: (-counts[w], w))\nprint('BEST', best)",
	`r = await skills.propose_code("top_word", ${JSON.stringify(SKILL)}, ${JSON.stringify(SKILL_TEST)}, {"task": "top word of corpus.txt", "cells": 3})\nprint('PROPOSED', r['status'], r['version'])`,
];
const REUSE = [
	"from code_skills import top_word\nprint('SKILL', top_word.__skill_version__, top_word.top_word(open('corpus.txt').read()))",
];

function toolDescription(request: ScriptedRequest): string {
	const tools = (request.body as unknown as { tools?: Array<{ function: { name: string; description: string } }> })
		.tools;
	return tools?.find((tool) => tool.function.name === "rlm")?.function.description ?? "";
}

function script(request: ScriptedRequest) {
	if (!request.firstUser.startsWith("TASK")) return { text: "ok" };
	const steps = toolDescription(request).includes("- top_word v1:") ? REUSE : DERIVE;
	const code = steps[request.turn];
	if (code !== undefined) return { tool: "rlm", args: { code } };
	const result = [...request.body.messages].reverse().find((message) => message.role === "tool");
	const text = typeof result?.content === "string" ? result.content : JSON.stringify(result?.content ?? "");
	const answer = /(?:SKILL \d+|BEST) (\w+)/.exec(
		request.body.messages
			.filter((message) => message.role === "tool")
			.map((message) => (typeof message.content === "string" ? message.content : JSON.stringify(message.content)))
			.join("\n"),
	);
	return { text: `TOP ${answer?.[1] ?? `unknown (${text.slice(0, 80)})`}` };
}

const roots: string[] = [];
afterEach(() => {
	for (const root of roots.splice(0)) rmSync(root, { recursive: true, force: true });
});

async function runTask(provider: ScriptedProvider, agentDir: string, projectDir: string) {
	const first = provider.requests.length;
	const client = new RpcClient({
		cliPath,
		cwd: projectDir,
		provider: "scripted",
		model: "scripted",
		args: ["--no-session"],
		env: {
			NODE_OPTIONS: `--import ${sourceResolverPath}`,
			ULTRON_CODING_AGENT_DIR: agentDir,
			// A fresh server and worker for every run: nothing but the profile carries over.
			ULTRON_SERVER_DIR: mkdtempSync(join("/tmp", "u-a54-")),
			ULTRON_HINDSIGHT_URL: "off",
			TYPESAFE_API_KEY: "",
			ULTRON_CODE_SKILLS_DIR: "",
			PI_OFFLINE: "1",
		},
	});
	try {
		await client.start();
		await client.promptAndWait("TASK: which word occurs most often in corpus.txt?", undefined, 120_000);
		const answer = await client.getLastAssistantText();
		const messages = await client.getMessages();
		const toolOutput = messages
			.filter((message) => message.role === "toolResult")
			.flatMap((message) =>
				(message.content as Array<{ type: string; text?: string }>).map((part) => part.text ?? ""),
			)
			.join("\n");
		const requests = provider.requests.slice(first);
		const toolsByName: Record<string, number> = {};
		for (const message of requests.at(-1)?.body.messages ?? [])
			for (const call of message.tool_calls ?? [])
				toolsByName[call.function.name] = (toolsByName[call.function.name] ?? 0) + 1;
		// The scripted provider reports prompt tokens as request bytes / 4, so this is the run's input cost.
		const promptTokens = requests.reduce((sum, request) => sum + Math.ceil(request.raw.length / 4), 0);
		return { answer, toolOutput, calls: requests.length, promptTokens, toolsByName, requests };
	} finally {
		await client.stop();
	}
}

describe("A54: a repeated task reuses its saved code skill", () => {
	test("the second run imports the skill in its kernel and makes fewer, cheaper model calls", async () => {
		const root = mkdtempSync(join(tmpdir(), "ultron-a54-"));
		roots.push(root);
		const agentDir = join(root, "agent");
		const projectDir = join(root, "project");
		mkdirSync(agentDir, { recursive: true });
		mkdirSync(projectDir, { recursive: true });
		const words = ["delta", "alpha", "beta", "alpha", "gamma", "beta", "alpha", "delta", "beta"];
		writeFileSync(
			join(projectDir, "corpus.txt"),
			`${Array.from({ length: 400 }, (_, i) => words[i % words.length]).join(" ")}\n`,
		);
		const provider = new ScriptedProvider(script);
		await provider.start();
		writeFileSync(join(agentDir, "models.json"), scriptedModelsJson(provider.baseUrl));
		try {
			const first = await runTask(provider, agentDir, projectDir);
			expect(first.answer).toBe("TOP alpha");
			expect(first.toolOutput).toContain("PROPOSED active 1");
			expect(first.toolsByName).toEqual({ rlm: 4 });
			expect(toolDescription(first.requests[0]!)).not.toContain("- top_word v1:");
			// The saved skill is on disk in the profile, tested and active.
			const history = JSON.parse(readFileSync(join(agentDir, "skills", "top_word", ".history.json"), "utf8"));
			expect(history).toMatchObject({ active: 1, versions: [{ state: "active", test: { passed: true } }] });

			const second = await runTask(provider, agentDir, projectDir);
			expect(second.answer).toBe("TOP alpha");
			// The new worker lists the skill's docstring in the rlm tool description ...
			expect(toolDescription(second.requests[0]!)).toContain(
				"- top_word v1: Most frequent word in a text (ties broken alphabetically). [top_word(text: str) -> str]",
			);
			// ... and its kernel imported the tested version.
			expect(second.toolOutput).toContain("SKILL 1 alpha");
			expect(second.toolsByName).toEqual({ rlm: 1 });
			expect(second.calls).toBeLessThan(first.calls);
			expect(second.promptTokens).toBeLessThan(first.promptTokens);
		} finally {
			await provider.stop();
		}
	}, 240_000);
});
