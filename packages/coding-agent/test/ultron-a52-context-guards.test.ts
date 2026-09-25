/**
 * A52: `ctx.forget` never removes the current user turn or pinned items, and an extension can observe the edit.
 * The model forgets an earlier user message through the kernel's `ctx` API: the next request no longer carries it,
 * while the durable transcript does. Forgetting the current user message and a pinned item is refused. An
 * extension loaded with `-e` receives the edit on `pi.events`. After a compaction, the note and the pinned item
 * are still in the model's context and the forgotten message stays out of it.
 */
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { describe, expect, test } from "vitest";
import { RpcClient } from "../src/modes/rpc/rpc-client.ts";
import { ScriptedProvider, type ScriptedRequest, scriptedModelsJson } from "./support/scripted-provider.ts";
import { tempServerDir } from "./support/server-dir.ts";

// Built by concatenation in the model's code so the code itself never carries the secret.
const SECRET = "SECRET_ALPHA_4471";
const PINNED = "SECRET_BRAVO_9082";
const NOTE = "NOTE_TEXT_3350";
const SUMMARY = "SCRIPTED_COMPACTION_SUMMARY";

const EDIT_CELL = [
	"h = await ctx.history(limit=50)",
	"items = h['items']",
	"secret = 'SECRET_' + 'ALPHA_4471'",
	"old = [i for i in items if i['kind'] == 'user' and secret in i['preview']][0]",
	"current = [i for i in items if i['kind'] == 'user'][-1]",
	"ack = [i for i in items if i['kind'] == 'assistant' and ('SECRET_' + 'BRAVO') in i['preview']][0]",
	"r = await ctx.forget([old['id']], 'no longer needed')",
	"print('FORGOT', r['forgotten'] == [old['id']], r['freed_bytes'] > 0)",
	"try:",
	"    await ctx.forget([current['id']], 'try the current turn')",
	"    print('CURRENT_FORGOTTEN')",
	"except Exception as e:",
	"    print('REFUSED_CURRENT', 'current user turn' in str(e))",
	"await ctx.pin(ack['id'])",
	"try:",
	"    await ctx.forget([ack['id']], 'try a pinned item')",
	"    print('PINNED_FORGOTTEN')",
	"except Exception as e:",
	"    print('REFUSED_PINNED', 'pinned' in str(e))",
	"await ctx.note('NOTE_' + 'TEXT_3350')",
	"after = {i['id']: i for i in (await ctx.history(limit=50))['items']}",
	"print('STATE', after[old['id']]['state'], after[ack['id']]['pinned'], after[current['id']]['protected'])",
	"full = await ctx.get(old['id'])",
	"print('DURABLE', secret in full['text'], full['visible_text'] is None)",
].join("\n");

function text(content: unknown): string {
	return typeof content === "string"
		? content
		: Array.isArray(content)
			? content.map((part) => (part as { text?: string }).text ?? "").join("")
			: "";
}

/** The newest user prompt of the conversation (notes and other custom messages also arrive as user messages). */
function latestPrompt(request: ScriptedRequest): string {
	const prompts = request.body.messages
		.filter((message) => message.role === "user")
		.map((message) => text(message.content));
	return [...prompts].reverse().find((prompt) => /^P\d/.test(prompt)) ?? "";
}

function toolOutputs(request: ScriptedRequest): string[] {
	return request.body.messages.filter((message) => message.role === "tool").map((message) => text(message.content));
}

describe("A52 context edits are guarded and observable", () => {
	test("forget spares the current turn and pins, extensions observe it, compaction keeps notes and pins", async () => {
		const root = mkdtempSync(join(tmpdir(), "ultron-a52-"));
		const agentDir = join(root, "agent");
		const projectDir = join(root, "project");
		mkdirSync(agentDir, { recursive: true });
		mkdirSync(projectDir, { recursive: true });
		const eventsFile = join(root, "context-edits.jsonl");
		const extension = join(root, "observe-context-edits.ts");
		writeFileSync(
			extension,
			[
				'import { appendFileSync } from "node:fs";',
				"export default function (pi: any) {",
				`\tpi.events.on("ultron:context_edit", (event: unknown) => appendFileSync(${JSON.stringify(eventsFile)}, \`\${JSON.stringify(event)}\\n\`));`,
				"}",
			].join("\n"),
		);
		const provider = new ScriptedProvider((request) => {
			if (request.system.includes("context summarization assistant")) return { text: SUMMARY };
			const last = latestPrompt(request);
			if (last.startsWith("P1")) return { text: `ACK1 ${PINNED}` };
			if (last.startsWith("P2"))
				return toolOutputs(request).some((text) => text.includes("REFUSED_CURRENT"))
					? { text: "DONE2" }
					: { tool: "rlm", args: { code: EDIT_CELL } };
			if (last.startsWith("P3")) return { text: "ACK3" };
			if (last.startsWith("P4")) return { text: "ACK4" };
			return { text: "unexpected" };
		});
		await provider.start();
		writeFileSync(join(agentDir, "models.json"), scriptedModelsJson(provider.baseUrl));
		const client = new RpcClient({
			cliPath: resolve(__dirname, "../src/cli.ts"),
			cwd: projectDir,
			provider: "scripted",
			model: "scripted",
			args: ["--no-session", "-e", extension],
			env: {
				NODE_OPTIONS: `--import ${resolve(__dirname, "../src/experimental/source-resolver.ts")}`,
				ULTRON_CODING_AGENT_DIR: agentDir,
				ULTRON_SERVER_DIR: tempServerDir("u-a52-"),
				ULTRON_HINDSIGHT_URL: "off",
				PI_OFFLINE: "1",
			},
		});
		try {
			await client.start();
			await client.promptAndWait(`P1 remember ${SECRET}`, undefined, 120_000);
			await client.promptAndWait("P2 tidy your context", undefined, 120_000);
			expect(await client.getLastAssistantText()).toBe("DONE2");

			const afterEdit = provider.requests.find(
				(request) => latestPrompt(request).startsWith("P2") && request.raw.includes("REFUSED_CURRENT"),
			);
			expect(afterEdit).toBeDefined();
			const output = toolOutputs(afterEdit!).find((text) => text.includes("REFUSED_CURRENT"))!;
			expect(output).toContain("FORGOT True True");
			expect(output).toContain("REFUSED_CURRENT True");
			expect(output).toContain("REFUSED_PINNED True");
			expect(output).toContain("STATE forgotten True True");
			expect(output).toContain("DURABLE True True");
			expect(output).not.toContain("CURRENT_FORGOTTEN");
			expect(output).not.toContain("PINNED_FORGOTTEN");
			// The forgotten user message is out of the model's view; the pinned answer, the current turn, and the
			// note are in it.
			expect(afterEdit!.raw).not.toContain(SECRET);
			expect(afterEdit!.raw).toContain(PINNED);
			expect(afterEdit!.raw).toContain("P2 tidy your context");
			expect(afterEdit!.raw).toContain(NOTE);
			// The durable transcript still has it.
			expect(JSON.stringify(await client.getMessages())).toContain(SECRET);

			// The extension observed exactly one model edit: the omission of the old user message.
			const events = readFileSync(eventsFile, "utf8")
				.trim()
				.split("\n")
				.map((line) => JSON.parse(line) as { source: string; lane: string; reason?: string; edits: unknown[] });
			const forgets = events.filter((event) => event.source === "ctx.forget");
			expect(forgets).toHaveLength(1);
			expect(forgets[0]).toMatchObject({ lane: "main", reason: "no longer needed" });
			expect(forgets[0]!.edits).toEqual([{ targetId: expect.any(String), action: "omit" }]);

			// Push the edited turns behind a compaction: the note and the pin survive it, the forgotten text does not.
			await client.promptAndWait(`P3 ${"filler ".repeat(14_000)}`, undefined, 120_000);
			await client.compact();
			await client.promptAndWait("P4 continue", undefined, 120_000);
			expect(await client.getLastAssistantText()).toBe("ACK4");
			const afterCompaction = provider.requests.at(-1)!;
			expect(latestPrompt(afterCompaction)).toBe("P4 continue");
			expect(afterCompaction.raw).toContain(SUMMARY);
			expect(afterCompaction.raw).not.toContain("P2 tidy your context");
			expect(afterCompaction.raw).toContain("kept across compaction");
			expect(afterCompaction.raw).toContain(NOTE);
			expect(afterCompaction.raw).toContain(PINNED);
			expect(afterCompaction.raw).not.toContain(SECRET);
			// The summarizer never saw the forgotten message either.
			const summarizer = provider.requests.find((request) =>
				request.system.includes("context summarization assistant"),
			);
			expect(summarizer).toBeDefined();
			expect(summarizer!.raw).not.toContain(SECRET);
		} finally {
			await client.stop().catch(() => {});
			await provider.stop();
			rmSync(root, { recursive: true, force: true });
		}
	}, 300_000);
});
