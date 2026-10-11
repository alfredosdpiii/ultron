// Generates crates/interactive/fixtures/omp-copy-parity.json by running omp's /copy target extraction (oh-my-pi
// packages/tui/src/overlays/copy-targets.ts) and the block/label helpers of overlays/copy-selector.ts
// (collectBlocks, targetCopy, turnSummary; sliced out of the module because the component imports the whole TUI).
//
// omp's extractMarkdownLinks walks its renderer's token tree; the walk and plainInlineTokens are copied from omp
// components/markdown.ts and run over the lexer the Rust crate ports (this checkout's packages/tui markdown.ts
// parser: marked 18 with strict strikethrough and the LaTeX extensions, loaded from a copy with absolute imports).
// The theme's `sep.dot` is omp's unicode preset (" · ").
//
// Usage: OMP_SRC=<oh-my-pi checkout> node crates/interactive/fixtures/generate-omp-copy.mjs
//        (deterministic; default OMP_SRC is ../omp-reference-src/oh-my-pi beside the worktree's parent)
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { createRequire } from "node:module";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const omp = resolve(process.env.OMP_SRC ?? join(here, "../../../../../omp-reference-src/oh-my-pi"));
const ompTui = join(omp, "packages/tui/src");
const ultronMarkdown = join(here, "../../../packages/tui/src/components/markdown.ts");

const dir = mkdtempSync(join(tmpdir(), "omp-copy-"));
// Ultron's markdown parser, exported from a copy whose imports point back at the checkout.
const require = createRequire(ultronMarkdown);
const md = readFileSync(ultronMarkdown, "utf8").replace(/from "([^"]+)"/g, (_m, spec) =>
	spec.startsWith(".")
		? `from ${JSON.stringify(pathToFileURL(resolve(dirname(ultronMarkdown), spec)).href)}`
		: `from ${JSON.stringify(pathToFileURL(require.resolve(spec)).href)}`,
);
writeFileSync(join(dir, "markdown.ts"), `${md}\nexport { markdownParser };\n`);

// omp's link walk and label flattening.
const ompMarkdown = readFileSync(join(ompTui, "components/markdown.ts"), "utf8");
const slice = (source, start, end) => {
	const a = source.indexOf(start);
	const b = source.indexOf(end, a + 1);
	if (a < 0 || b < 0) throw new Error(`slice not found: ${start}`);
	return source.slice(a, b);
};
const walk = slice(ompMarkdown, "export function extractMarkdownLinks", "/**\n * Offset just past");
const plain = slice(ompMarkdown, "function plainInlineTokens", "/**\n * Classify an inline");
writeFileSync(
	join(dir, "index.ts"),
	`import { markdownParser } from "./markdown.ts";
type Token = any; type Tokens = any; type MarkdownLink = { text: string; href: string };
function isMathToken(_t: unknown): boolean { return false; }
function renderMathToken(t: string): string { return t; }
${walk}
${plain}
`,
);
writeFileSync(
	join(dir, "copy-targets.ts"),
	readFileSync(join(ompTui, "overlays/copy-targets.ts"), "utf8").replace(`from "../index"`, `from "./index.ts"`),
);

// copy-selector.ts helpers.
const selector = readFileSync(join(ompTui, "overlays/copy-selector.ts"), "utf8");
const helpers = [
	slice(selector, "/** First non-blank line of `source`", "/** Local `HH:MM`"),
	slice(selector, "/** Argument keys naming what a tool call works on", "/** The timeline pickers"),
	selector.slice(selector.indexOf("/** Raw multi-line text of a user message")),
].join("\n");
writeFileSync(
	join(dir, "selector.ts"),
	`import { commandFromToolCall, extractBlocks, extractLinks } from "./copy-targets.ts";
const theme = { sep: { dot: " · " } };
type TranscriptEntry = any; type SessionMessageEntry = any; type OutlineTarget = any; type CopyBlock = any;
function transcriptEntryMessage(entry: any) { return entry.message; }
function textContent(content: any, separator = ""): string {
	if (typeof content === "string") return content;
	let text = ""; let boundary = "";
	for (const block of content) { if (block.type !== "text") continue; text += boundary + (block.text ?? ""); boundary = separator; }
	return text;
}
function userMessageLabel(content: any): string { return textContent(content, " ").replace(/\\s+/g, " ").trim(); }
function userTurnDraft(_entry: any): string | undefined { return undefined; }
${helpers}
export { turnSummary, toolCallLabel, firstLine };
`,
);
const T = await import(pathToFileURL(join(dir, "copy-targets.ts")).href);
const S = await import(pathToFileURL(join(dir, "selector.ts")).href);
rmSync(dir, { recursive: true });

const texts = [
	"intro\n```ts\nconst x = 1;\n```\ntail",
	"```\nplain\n```\n\n```py\nprint(1)\n```",
	"intro\n> line one\n> line two\ntail",
	"> first\n>\n> second\n\nbreak\n> later",
	"> real quote\n```\n> not a quote\n```",
	"```unclosed\n> quoted after\ntext",
	"```  rust  \nfn main() {}\n```\n>no space\n>  two spaces",
	"```\n```",
	"",
	">",
	"a\n```js\nx\n``` trailing info\n> q",
	'See [the docs](https://example.com/docs "Docs") and <https://example.com/auto>.\nBare: https://example.com/bare/path?x=1&y=2 then again https://example.com/docs.',
	"(https://en.wikipedia.org/wiki/Foo_(bar)).",
	"Try https://example.com/a, https://example.com/b; or https://example.com/c?",
	"Run `curl https://example.com/in-code` then:\n```bash\ncurl https://example.com/fenced\n```\nDocs: https://example.com/prose",
	"Run: https://github.com/dalilshorja/dalilshorja/actions/runs/33654874668/job/100330811133?check_suite_focus=true&pr=4",
	"[file](file:///tmp/x) [mail](mailto:a@b.c) ftp://x.y/z a@b.c",
	"[ref link][r] and www.example.com/www\n\n[r]: https://example.com/ref",
	"- item [one](https://one.example)\n- **[bold `two`](https://two.example)**\n\n| a | b |\n|---|---|\n| [c](https://c.example) | d |",
	"> quoted [link](https://q.example)\n\n# Heading [h](https://h.example)",
	"[](https://empty.example) [  spaced\n label  ](https://spaced.example) [https://dup.example](https://dup.example)",
	"HTTPS://UPPER.EXAMPLE/x and [x](HTTP://Upper.example)",
];
const blocks = texts.map((text) => ({
	text,
	blocks: T.extractBlocks(text),
	links: T.extractLinks(text),
}));

const calls = [
	["bash", { command: "echo hi" }],
	["bash", { command: 3 }],
	["eval", { language: "py", code: "print(1)" }],
	["eval", { language: "js", code: "log(1)" }],
	["eval", { cells: [{ language: "py", code: "print(1)" }, { code: "print(2)" }] }],
	["eval", { cells: [{ code: "" }, { language: "js", code: "a" }, { language: "py", code: "b" }] }],
	["eval", { cells: [] }],
	["eval", { code: "" }],
	["eval", {}],
	["read", { path: "x" }],
].map(([name, args]) => ({ name, args, command: T.commandFromToolCall({ name, arguments: args }) ?? null }));

const asst = (content) => ({ role: "assistant", content });
const histories = [
	[
		asst([{ type: "toolCall", id: "1", name: "bash", arguments: { command: "echo old" } }]),
		asst([{ type: "toolCall", id: "2", name: "read", arguments: { path: "x" } }]),
		asst([
			{ type: "toolCall", id: "3", name: "bash", arguments: { command: "echo a" } },
			{ type: "toolCall", id: "4", name: "bash", arguments: { command: "echo b" } },
		]),
	],
	[
		asst([{ type: "text", text: "old https://example.com/old\n```py\nold()\n```" }]),
		asst([{ type: "text", text: "a https://example.com/a and b https://example.com/b" }]),
		{ role: "user", content: "thanks https://example.com/user-link" },
		asst([{ type: "text", text: "no links here" }]),
	],
	[asst([{ type: "text", text: "   " }]), { role: "user", content: "x" }],
];
const lasts = histories.map((messages) => ({
	messages,
	command: T.extractLastCommand(messages) ?? null,
	link: T.extractLastLink(messages) ?? null,
	code: T.extractLastCodeBlock(messages) ?? null,
	text: messages.map((m) => T.assistantText(m) ?? null),
}));

// Turns: entries spanned by one outline target.
const e = (id, message) => ({ type: "message", id, timestamp: "2026-10-11T10:00:00Z", message });
const turns = [
	[e("u1", { role: "user", content: "Look at ```\nnot fenced``` and\n```sh\nls -la\n```\n> quoted\nhttps://u.example" })],
	[e("u2", { role: "user", content: [{ type: "text", text: "first" }, { type: "image" }, { type: "text", text: "second\n  line" }] })],
	[
		e("a1", asst([
			{ type: "thinking", thinking: "hmm" },
			{ type: "text", text: "Here:\n```rust\nfn x() {}\n```\nSee [docs](https://docs.example)." },
			{ type: "toolCall", id: "c1", name: "bash", arguments: { command: "cargo test" } },
			{ type: "toolCall", id: "c2", name: "eval", arguments: { code: "print(2)" } },
		])),
		e("r1", { role: "toolResult", toolName: "bash", toolCallId: "c1", content: [{ type: "text", text: "ok\n" }, { type: "text", text: "done" }] }),
		e("r2", { role: "toolResult", toolName: "eval", toolCallId: "c2", content: [{ type: "image" }] }),
	],
	[
		e("a2", asst([{ type: "toolCall", id: "c3", name: "read", arguments: { path: "/tmp/file.txt" } }, { type: "toolCall", id: "c4", name: "bash", arguments: { command: "ls" } }])),
		e("r3", { role: "toolResult", toolName: "read", toolCallId: "c3", content: [{ type: "text", text: "  file body  " }] }),
	],
	[e("a3", asst([{ type: "thinking", thinking: "only thinking" }]))],
	[e("b1", { role: "bashExecution", command: "  git status\n--short", output: "M a\n" })],
	[e("b2", { role: "bashExecution", command: "true", output: "   " })],
	[e("p1", { role: "pythonExecution", code: "\n\nx = 1\ny = 2", output: "" })],
	[e("s1", { role: "compactionSummary", summary: "compacted summary" })],
	[e("s2", { role: "branchSummary", summary: "branch summary" })],
	[e("c1", { role: "custom", customType: "note", content: "custom\nnotice" })],
	[e("c2", { role: "custom", customType: "empty-note", content: [] })],
	[e("t1", { role: "toolResult", toolName: "grep", toolCallId: "x", content: [] })],
	[e("z1", { role: "mystery" })],
];
const turnOut = turns.map((entries) => {
	const blocks = S.collectBlocks(entries);
	const target = { entries };
	return {
		entries: entries.map((x) => ({ id: x.id, message: x.message })),
		blocks: blocks.map((b) => ({ ...b, entry: entries.indexOf(b.entry) })),
		copy: S.targetCopy(target, blocks),
		summary: S.turnSummary(entries[0]),
	};
});

const labels = [
	["grep", { pattern: "ack" }],
	["bash", { command: "  ls   -la\nmore" }],
	["read", { path: "", file_path: "/a/b" }],
	["fetch", { url: "https://x" }],
	["search", { query: 42 }],
	["noargs", undefined],
	["eval", { code: "\n\nprint(1)" }],
].map(([name, args]) => [name, args ?? null, S.toolCallLabel(name, args)]);

writeFileSync(
	join(here, "omp-copy-parity.json"),
	`${JSON.stringify({ blocks, calls, lasts, turns: turnOut, labels })}\n`,
);
