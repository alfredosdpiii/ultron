// Generates crates/tui/fixtures/markdown-latex-parity.json: Markdown documents with LaTeX math rendered by the TS
// component with renderLatex on (the default) and off, per width and configuration.
// Usage: node crates/tui/fixtures/generate-markdown-latex.mjs   (deterministic; output is written next to this file)
import { writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
const here = dirname(fileURLToPath(import.meta.url));
const { Markdown } = await import(join(here, "../../../packages/tui/src/components/markdown.ts"));

const sty = (open, close = 0) => (s) => `\x1b[${open}m${s}\x1b[${close}m`;
const theme = {
	heading: sty(36, 39), link: sty(34, 39), linkUrl: sty(90, 39), code: sty(33, 39), codeBlock: sty(32, 39),
	codeBlockBorder: sty(90, 39), quote: sty(37, 39), quoteBorder: sty(90, 39), hr: sty(90, 39), listBullet: sty(35, 39),
	bold: sty(1, 22), italic: sty(3, 23), strikethrough: sty(9, 29), underline: sty(4, 24),
};
const CONFIGS = [
	{ px: 0, py: 0, style: false, renderLatex: true },
	{ px: 1, py: 1, style: true, renderLatex: true },
	{ px: 0, py: 0, style: false, renderLatex: false },
];
const DOCS = [
	"$E=mc^2$ and $$x+y$$ inline and \\[a^2\\] and \\(b_1 + \\alpha\\)",
	"Euler: $e^{i\\pi} + 1 = 0$, roots $x = \\frac{-b \\pm \\sqrt{b^2-4ac}}{2a}$.",
	"Before\n$$\n\\sum_{i=1}^{n} i = \\frac{n(n+1)}{2}\n$$\nAfter",
	"\\[\n\\begin{pmatrix} 1 & 2 \\\\ 3 & 4 \\end{pmatrix}\n\\]\n\ntext",
	"$$\nf(x) = \\begin{cases} x, & x > 0 \\\\ -x, & \\text{otherwise} \\end{cases}\n$$",
	"$$\\int_0^1 x\\,dx$$\n\nnext paragraph",
	"Unsupported $\\foo{x}$ and block:\n\n$$\n\\badcommand\n$$",
	"Pending $\\frac{1}{2 and more",
	"Pending \\(x + y",
	"$$\nunfinished = 1",
	"Escaped $a \\$ b$ and $x\\$y$ done",
	"- item $\\alpha$\n- **bold $\\beta^2$** and *em $\\gamma_1$*",
	"> quote with $\\sqrt{2}$\n\n| a | b |\n|---|---|\n| $\\pi$ | $\\infty$ |",
	"# Heading $\\Omega$\n\nText $\\mathbb{R}^n \\to \\mathbb{R}$",
	"$$\n\\lim_{x \\to 0} \\frac{\\sin x}{x} = 1\n$$",
];
const WIDTHS = [12, 40, 100];
const cases = [];
for (const doc of DOCS) {
	for (const [ci, c] of CONFIGS.entries()) {
		const style = c.style ? { color: sty(31, 39), bold: true } : undefined;
		const outputs = WIDTHS.map((w) => new Markdown(doc, c.px, c.py, theme, style, { renderLatex: c.renderLatex }).render(w));
		cases.push({ doc, config: ci, outputs });
	}
}
writeFileSync(join(here, "markdown-latex-parity.json"), `${JSON.stringify({ configs: CONFIGS, widths: WIDTHS, cases })}\n`);
console.log(`markdown-latex-parity.json: ${cases.length} cases`);
