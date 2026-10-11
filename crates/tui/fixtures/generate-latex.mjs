// Generates crates/tui/fixtures/latex-parity.json from packages/tui/src/latex.ts (TypeScript, this checkout):
// renderLatex inline and display output for every symbol/accent/wrapper command, the string inputs of
// packages/tui/test/latex.test.ts, and a hand-written corpus of layouts, environments and malformed input.
// Usage: node crates/tui/fixtures/generate-latex.mjs   (deterministic; output is written next to this file)
import { readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
const here = dirname(fileURLToPath(import.meta.url));
const pkg = join(here, "../../../packages/tui");
const { renderLatex } = await import(join(pkg, "src/latex.ts"));

const source = readFileSync(join(pkg, "src/latex.ts"), "utf8");
const commands = new Set();
for (const m of source.matchAll(/^\t(\w+): "/gm)) commands.add(m[1]);
for (const m of source.matchAll(/new Set\(\[([\s\S]*?)\]\)/g)) for (const s of m[1].matchAll(/"([^"]*)"/g)) commands.add(s[1]);

const inputs = [];
for (const c of commands) {
	inputs.push(`\\${c}`, `x \\${c} y`, `\\${c}{ab}`, `\\${c}_{i=1}^{n} x`, `\\${c} x`);
}
const test = readFileSync(join(pkg, "test/latex.test.ts"), "utf8");
for (const m of test.matchAll(/String\.raw`([^`]*)`|^\t+\["((?:[^"\\]|\\.)*)"/gm)) {
	inputs.push(m[1] ?? JSON.parse(`"${m[2]}"`));
}
inputs.push(
	"x^2 + y^2 = z^2", "a_i^2", "a^{n+1}_{k}", "e^{i\\pi} + 1 = 0", "x^{\\frac{1}{2}}", "x_{\\alpha\\beta}", "x^{ab}", "x^{AB}",
	"x^*", "x_{ijk}", "x^{a/b}", "\\frac{a+b}{c}", "\\frac{1}{2}", "\\tfrac{x}{y}", "\\dfrac{\\frac{a}{b}}{c}", "\\frac12",
	"\\sqrt{x}", "\\sqrt[3]{x+1}", "\\sqrt[4]{y}", "\\sqrt[n]{y}", "\\sqrt[2]{ab}", "\\sqrt[", "\\sum_{i=1}^{n} i^2",
	"\\int_0^1 f(x)\\,dx", "\\lim_{x \\to 0} \\frac{\\sin x}{x}", "\\lim\\limits_{n} a_n", "\\sum\\nolimits_{k} k",
	"\\operatorname{rank}(A)", "\\operatorname*{argmax}_{x} f", "\\sin x \\cos y", "\\sin^2 x", "2\\sin x", "\\log_2 n",
	"a \\bmod b", "a \\equiv b \\pmod{n}", "x \\pod{3}", "\\overset{!}{=}", "\\underset{x}{\\min}", "\\stackrel{def}{=}",
	"\\not\\in", "\\not=", "\\not{abc}", "\\not{}", "\\mathbb{R}^n", "\\mathbb{RQx}", "\\hat{x}", "\\hat{xy}", "\\vec{v}",
	"\\text{if } x", "\\textbf{ bold }", "\\mathrm{ d }x", "\\boxed{ 42 }", "\\binom{n}{k}", "\\left( x \\right.", "\\big( x \\Big)",
	"a\\!b", "\\sin\\!x", "a\\,b\\;c\\quad d", "a~b", "a & b", "\\{ x \\}", "\\$5 \\% \\# \\_ \\&", "a \\| b", "a\\\\b",
	"\\bf x y", "\\displaystyle x", "\\unknown", "\\end{x}", "{a", "a}", "\\", "x^", "\\frac{a}", "",
	"\\begin{cases} x, & x > 0 \\\\ -x, & \\text{otherwise} \\end{cases}",
	"f(x) = \\begin{cases} 1 & x \\ge 0 \\\\ 0 & x < 0 \\end{cases}.",
	"\\begin{cases} 1 & \\text{if } a \\\\ 2 & \\text{when } b \\\\ 3 & c \\\\ 4 \\end{cases}",
	"\\begin{cases} 1 & a \\end{cases}",
	"\\begin{pmatrix} 1 & 2 \\\\ 3 & 4 \\end{pmatrix}",
	"A = \\begin{bmatrix} a & b & c \\\\ d & e & f \\\\ g & h & i \\end{bmatrix}.",
	"\\begin{Bmatrix} 1 \\\\ 22 \\end{Bmatrix}", "\\begin{vmatrix} a & b \\\\ c & d \\end{vmatrix}",
	"\\begin{Vmatrix} x \\\\ y \\end{Vmatrix}", "\\begin{matrix} 1 & 2 \\end{matrix}",
	"\\begin{array}{cc} 1 & 2 \\\\ 3 & 4 \\end{array}", "\\begin{smallmatrix} a & b \\\\ c & d \\end{smallmatrix}",
	"\\begin{aligned} a &= b + c \\\\[2pt] d &= e \\end{aligned}", "\\begin{align*} x &= 1 \\\\ y &= 2 \\end{align*}",
	"\\begin{alignedat}{2} a &= b & c &= d \\end{alignedat}", "\\begin{equation} E = mc^2 \\end{equation}",
	"\\begin{gather} a \\\\ b \\end{gather}", "\\begin{unknown} x \\end{unknown}", "\\begin{cases} x", "\\begin x",
	"x = \\frac{-b \\pm \\sqrt{b^2 - 4ac}}{2a}", "\\frac{\\sum_{i=1}^n x_i}{n}", "\\sum_{i=1}^{n} \\frac{1}{i^2} = \\frac{\\pi^2}{6}",
	"\\prod_{k} \\left( 1 - \\frac{1}{p_k} \\right)", "\\int_{-\\infty}^{\\infty} e^{-x^2} dx = \\sqrt{\\pi}",
	"\\begin{pmatrix} \\frac{1}{2} & 0 \\\\ 0 & 1 \\end{pmatrix} x", "a\\\nb", "\\sum_{i}_{j} x", "x^{y^{z}}", "x_{a_{b}}",
	"\\max_{x \\in S} f(x)", "\\arg\\max_x f", "\\sin_{x}", "\\cos\\theta", "\\ln(x)\\exp(y)", "\\mathbf{v} \\cdot \\mathbf{w}",
	"a \\times b", "a \\le b \\leq c", "\\alpha\\beta\\gamma", "x' + y''", "1.5 \\cdot 10^{-3}", "x^{2\\pi}", "x^{-1}", "x_{n-1}",
	"\\frac{a}{b}\\frac{c}{d}", "\\frac{a}{\\frac{b}{c}}", "\\text{a}\n\\text{b}", "a\n\nb", "x^{\\sum_{i} i}",
);

const cases = [];
const seen = new Set();
for (const input of inputs) {
	if (seen.has(input)) continue;
	seen.add(input);
	const inline = renderLatex(input);
	const display = renderLatex(input, { display: true });
	cases.push({ input, inline: inline ?? null, display: display ?? null });
}
writeFileSync(join(here, "latex-parity.json"), `${JSON.stringify({ cases }, null, "\t")}\n`);
console.log(`latex-parity.json: ${cases.length} cases`);
