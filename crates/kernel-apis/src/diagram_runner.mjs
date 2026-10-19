// diagram.mermaid's runner (crates/kernel-apis/src/diagram.rs): reads Mermaid text on stdin, renders it with the
// vendored Pele beside this file and prints one JSON line: {ok: true, type, mermaidType, svg, width, height} or
// {ok: false, error, line, column}.
import { render } from "./pele.mjs";

let text = "";
process.stdin.setEncoding("utf8");
for await (const chunk of process.stdin) text += chunk;
let answer;
try {
	const out = render(text, { responsive: false });
	answer = {
		ok: true,
		type: out.type,
		mermaidType: out.mermaidType,
		svg: out.svg,
		width: out.width,
		height: out.height,
	};
} catch (error) {
	answer = {
		ok: false,
		error: String(error?.message ?? error),
		line: typeof error?.line === "number" ? error.line : 0,
		column: typeof error?.column === "number" ? error.column : 0,
	};
}
process.stdout.write(`${JSON.stringify(answer)}\n`);
