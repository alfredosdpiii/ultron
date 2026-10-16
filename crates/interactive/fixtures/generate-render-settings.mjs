// Generates crates/interactive/fixtures/render-settings.json: the transcript's markdown under the render settings
// interactive-mode.ts applies: `markdown.codeBlockIndent` (`getMarkdownThemeWithSettings`, handed to
// AssistantMessageComponent) and `getCapabilities().hyperlinks` (OSC 8 links, after `setCapabilityOverrides`), for an
// assistant message and a user message with a code block and a link, dark truecolor at two widths.
// crates/interactive/tests/render_settings.rs replays it.
// Usage: node crates/interactive/fixtures/generate-render-settings.mjs
process.env.FORCE_COLOR = "3";
import "../../cli/fixtures/offline-guard.mjs";
import { writeFileSync } from "node:fs";
import { createRequire } from "node:module";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const ca = (p) => import(join(here, "../../../packages/coding-agent/src", p));
const T = await ca("modes/interactive/theme/theme.ts");
const { validateThemeJson } = await ca("modes/interactive/theme/theme-json.ts");
T.setThemeJsonValidator(validateThemeJson);
const tuiUrl = pathToFileURL(createRequire(join(here, "../../../packages/coding-agent/package.json")).resolve("@ultron/tui")).href;
const { setCapabilities } = await import(tuiUrl);
const { AssistantMessageComponent } = await ca("modes/interactive/components/assistant-message.ts");
const { UserMessageComponent } = await ca("modes/interactive/components/user-message.ts");
const themeDir = join(here, "../../../packages/coding-agent/src/modes/interactive/theme");
T.setThemeInstance(T.loadThemeFromPath(join(themeDir, "dark.json"), "truecolor"));

const TEXT = "See [the docs](https://example.com/docs) and run:\n\n```\nmake test\n  indented\n```\n\nDone.";
const message = { role: "assistant", content: [{ type: "text", text: TEXT }], stopReason: "stop", api: "x", provider: "x", model: "x", usage: {}, timestamp: 1 };
const cases = [];
for (const codeBlockIndent of [undefined, "  ", "\t> ", ""]) {
	for (const hyperlinks of [false, true]) {
		setCapabilities({ images: null, trueColor: true, hyperlinks });
		const markdownTheme = codeBlockIndent === undefined ? undefined : { ...T.getMarkdownTheme(), codeBlockIndent };
		for (const width of [30, 80]) {
			const a = new AssistantMessageComponent(undefined, false, markdownTheme, "Thinking...", 1);
			a.updateContent(message);
			cases.push({ kind: "assistant", codeBlockIndent: codeBlockIndent ?? null, hyperlinks, width, lines: a.render(width) });
			if (codeBlockIndent === undefined) {
				const u = new UserMessageComponent(TEXT, undefined, 1);
				cases.push({ kind: "user", codeBlockIndent: null, hyperlinks, width, lines: u.render(width) });
			}
		}
	}
}
writeFileSync(join(here, "render-settings.json"), `${JSON.stringify({ text: TEXT, cases }, null, 1)}\n`);
console.log(`wrote ${cases.length} cases`);
