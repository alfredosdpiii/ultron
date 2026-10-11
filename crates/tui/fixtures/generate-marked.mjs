// Generates crates/tui/fixtures/marked-parity.json: lexer-focused Markdown documents (GFM block and inline rules and
// markdown.ts's LaTeX extensions) and the TS component's output lines per width and configuration.
// Usage: node crates/tui/fixtures/generate-marked.mjs   (deterministic; output is written next to this file)
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
	"See [the docs][ref] and [Ref] and [missing][nope].\n\n[ref]: https://example.com/docs \"Title\"\n[REF]: <https://other.example>",
	"Para then def\n[x]: https://x.io\n\nuse [x]",
	"![alt text](https://img.example/a.png) and ![ref img][r]\n\n[r]: /img.png",
	"Link with parens [p](https://e.com/a_(b)_c) and [angle](<https://e.com/a b>) and [t](/u 'single')",
	"[outer [inner](https://in.example) text](https://out.example)",
	"Links: www.example.com/path?q=1. and https://a.example/x), and (https://b.example/y) and foo@bar.example.org!",
	"<span class=\"x\">inline</span> <!-- c --> <a href=\"/h\">anchor www.not.example</a> <br/>",
	"<!-- block comment -->\n\ntext\n\n<script>\nlet x = 1;\n</script>\n\n<custom-tag attr=\"v\">\n\nafter",
	"*a **b** c* and **a *b* c** and ***x*** and *a*b* and **unclosed",
	"snake_case_word and _em_ and __strong__ and *intra*word and _a_b",
	"punct *\"quoted\"* and **«guillemets»** and *a* , *.*",
	"~single~ and ~~double~~ and ~~ spaced ~~ and ~~a\\~b~~ and x~~y~~z",
	"``code with ` tick`` and ` spaced ` and `  ` and ``` ``triple`` ```",
	"# Closing hashes ##\n## Only #\n### a # b\n#NoSpace\n####### seven",
	"Lazy\n> quote\ncontinuation\n> > nested\n> back",
	"> - a\nlazy item\n> - b\n\n> code:\n>\n>     indented in quote",
	"- item\n\n      indented code in item\n\n- [ ] loose task\n\n- [x] checked loose",
	"5. five\n6. six\n\n- a\n- b\n+ new list",
	"- a\n    - four space nested\n        - eight\n- b\n\n1. x\n   - y\n     ```\n     fenced in nested\n     ```",
	"Line one\r\nLine two\r\n\r\n- crlf item\r\n",
	"| Left | Center | Right |\n|:-----|:------:|------:|\n| a \\| pipe | `c|d` | **r** |\n| missing |\n| x | y | z | extra |",
	"Not | a table\n--- | ---\n\n|a|b|\n|-|-|\n|1|2|\nafter table",
	"Setext with *em*\n---\n\nTwo lines\nof setext\n===",
	"Hard  \nbreak and trailing\\\n\nnot a break at end  ",
	"Escapes \\` \\* \\_ \\[ \\] \\\\ \\| \\~ \\$ \\a",
	"$E=mc^2$ and $$x+y$$ inline and \\[a^2\\] and \\(b_1\\)",
	"Costs $5 and $HOME_DIR path and $PATH$suffix and $ a$ and $x $ and $`code`$",
	"Cost $a *b$5 and c* d, then $A_B$c and $x$ *y*",
	"Pending $\\frac{1}{2 and more",
	"Pending \\(x + y",
	"Before\n$$\na + b\n$$\nAfter",
	"\\[\n\\sum_{i=0}^n i\n\\]\n\ntext",
	"$$\nunfinished = 1",
	"$$\nplain words",
	"text $$ not block $$ here\n\n  $$ indented $$",
	"Text before $math$ text after, then `$not$` in code",
	"~~~\nfence ~~~ inside\n~~~~\n\n````md\n```\nnested\n```\n````",
	"   ```\n   indented fence\n     more\n   ```",
	"***\n___\n- - -\n* * *",
	"<div>\n*not md*\n</div>\n\n<p>para</p> trailing",
	"Emoji 😀 **bold 日本** *em ü* [link ü](https://ü.example)",
];
const WIDTHS = [12, 40, 100];

const cases = [];
for (const doc of DOCS) {
	for (const [ci, c] of CONFIGS.entries()) {
		ti.setCapabilities({ images: null, trueColor: true, hyperlinks: c.hyperlinks });
		const style = c.style && {
			...(c.style.color ? { color: sty(31, 39) } : {}), ...(c.style.bg ? { bgColor: sty(44, 49) } : {}),
			bold: c.style.bold, italic: c.style.italic, underline: c.style.underline, strikethrough: c.style.strikethrough,
		};
		const outputs = WIDTHS.map((w) =>
			new Markdown(doc, c.px, c.py, theme(c.highlight, c.indent), style ?? undefined, { ...c.options, renderLatex: false }).render(w),
		);
		cases.push({ doc, config: ci, outputs });
	}
}
ti.setCapabilities(null);
writeFileSync(join(here, "marked-parity.json"), `${JSON.stringify({ configs: CONFIGS, widths: WIDTHS, cases })}\n`);
