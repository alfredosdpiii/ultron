// Generates crates/tui/fixtures/markdown-parity.json: for each Markdown document, the top-level marked tokens the TS
// component renders (after its partial-fence trimming) and its output lines per width and configuration.
// Usage: node crates/tui/fixtures/generate-markdown.mjs   (deterministic; output is written next to this file)
import { writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
const here = dirname(fileURLToPath(import.meta.url));
const src = (p) => import(join(here, "../../../packages/tui/src", p));
const { Markdown } = await src("components/markdown.ts");
const ti = await src("terminal-image.ts");

const sty = (open, close = 0) => (s) => `\x1b[${open}m${s}\x1b[${close}m`;
const theme = (highlight, indent) => ({
	heading: sty(36, 39), link: sty(34, 39), linkUrl: sty(90, 39), code: sty(33, 39), codeBlock: sty(32, 39),
	codeBlockBorder: sty(90, 39), quote: sty(37, 39), quoteBorder: sty(90, 39), hr: sty(90, 39), listBullet: sty(35, 39),
	bold: sty(1, 22), italic: sty(3, 23), strikethrough: sty(9, 29), underline: sty(4, 24),
	...(highlight ? { highlightCode: (code, lang) => code.split("\n").map((l, i) => `<${lang ?? "-"}:${i}>${l}`) } : {}),
	...(indent !== undefined ? { codeBlockIndent: indent } : {}),
});
const CONFIGS = [
	{ px: 0, py: 0, highlight: false, style: null, options: {}, hyperlinks: false },
	{ px: 1, py: 1, highlight: true, indent: "| ", style: { color: true, bold: true }, options: { preserveOrderedListMarkers: true }, hyperlinks: true },
	{ px: 2, py: 0, highlight: false, style: { bg: true, italic: true, underline: true, strikethrough: true }, options: { preserveBackslashEscapes: true }, hyperlinks: false },
];
const DOCS = [
	"# Title\n\nSome *emphasis*, **strong**, `code`, ~~gone~~ and a [link](https://example.com) here.",
	"## Second level\nText right after.\n\n### Third `code` **bold**\n\n#### Fourth",
	"Paragraph one with a long sentence that should wrap across several lines when the width is narrow enough.\n\nParagraph two.",
	"- item one\n- item two with **bold**\n  - nested a\n  - nested b\n    1. deep ordered\n- item three",
	"3. three\n4. four\n10. ten",
	"1) paren\n2) paren two",
	"* star\n+ plus",
	"- [ ] todo\n- [x] done\n- plain",
	"- loose one\n\n- loose two\n\n- loose three",
	"-\n- after empty",
	"```ts\nconst x = 1;\nconsole.log(x);\n```\n\nAfter code.",
	"```\nno lang\n```",
	"~~~py\nprint('streaming')\n~~",
	"```js\nlet a\n``",
	"> quote line one\n> continues **bold** `c`\n>\n> second para\n\nafter quote",
	"> - list in quote\n> - two\n\n> ```\n> code in quote\n> ```",
	"Above\n\n---\n\nBelow",
	"| Name | Value | Notes |\n|------|------:|:-----:|\n| a | 1 | short |\n| **bold** | `code` | a much longer note that will need wrapping in narrow tables |",
	"| x |\n|---|\n| y |",
	"| supercalifragilisticexpialidocious-word | b |\n|---|---|\n| verylongunbreakablewordthatexceedsthirtycharacters | c |",
	"Line with hard break  \nnext line\\\nthird",
	"Escapes: \\*not em\\* and \\_x\\_ and \\# hash",
	"<div>html block</div>\n\nInline <b>tag</b> text",
	"Autolink <https://auto.example> and bare https://bare.example/x and mailto <me@x.io>",
	"[same](same) and [text](https://u.rl \"title\")",
	"Math $x^2 + y$ and \\(a+b\\) and $5 and $10\n\n$$\n\\int_0^1 f(x) dx\n$$\n\nend",
	"Mixed 日本語のテキストと **太字** and emoji 😀 in a paragraph that wraps.",
	"Setext\n======\n\nSub\n---",
	"    indented code block\n    second line",
	"Tabs\there\tand\tthere",
	"***bold italic*** and _underscore em_ and __underscore strong__",
	"",
	"   \n  ",
	"# Heading only",
	"1. one\n\n   para in item\n\n2. two",
	"- a\n```\ncode after list\n```",
	"Intro paragraph\n- item right after\n- second",
	"> quote then list\n> - one\n> - two\n>\n> ",
	"> ## Heading in quote\n>\n> > nested quote\n> > more",
	"| a | b |\n|---|---|\n| abcdefghijklmnopqrstuvwxyz012345678 | long text with many words that goes on and on and on |",
];
const WIDTHS = [1, 8, 20, 33, 60, 100];

let depth = 0;
let captured = null;
const original = Markdown.prototype.renderToken;
Markdown.prototype.renderToken = function (token, ...rest) {
	if (depth === 0 && captured) captured.push(structuredClone(token));
	depth++;
	try {
		return original.call(this, token, ...rest);
	} finally {
		depth--;
	}
};

const cases = [];
for (const doc of DOCS) {
	for (const [ci, c] of CONFIGS.entries()) {
		ti.setCapabilities({ images: null, trueColor: true, hyperlinks: c.hyperlinks });
		const style = c.style && {
			...(c.style.color ? { color: sty(31, 39) } : {}), ...(c.style.bg ? { bgColor: sty(44, 49) } : {}),
			bold: c.style.bold, italic: c.style.italic, underline: c.style.underline, strikethrough: c.style.strikethrough,
		};
		const outputs = [];
		let tokens = null;
		for (const w of WIDTHS) {
			const md = new Markdown(doc, c.px, c.py, theme(c.highlight, c.indent), style ?? undefined, { ...c.options, renderLatex: false });
			captured = [];
			outputs.push(md.render(w));
			tokens ??= captured;
		}
		cases.push({ doc, config: ci, tokens, outputs });
	}
}
ti.setCapabilities(null);
writeFileSync(join(here, "markdown-parity.json"), `${JSON.stringify({ configs: CONFIGS, widths: WIDTHS, cases })}\n`);
