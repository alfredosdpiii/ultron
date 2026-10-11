// Generates crates/tui/fixtures/autocomplete-sources-parity.json by running omp's TypeScript (oh-my-pi
// packages/tui/src/prompt/emoji-autocomplete.ts with its data/emojis.json, and github-ref-autocomplete.ts) over
// scripted text-before-cursor inputs. Both modules only import types, so they run under plain node from a temporary
// copy.
//
// Usage: OMP_SRC=<oh-my-pi checkout> node crates/tui/fixtures/generate-autocomplete-sources.mjs   (deterministic)
import { copyFileSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const omp = resolve(process.env.OMP_SRC ?? join(here, "../../../../../omp-reference-src/oh-my-pi"));
const prompt = join(omp, "packages/tui/src/prompt");
const dir = mkdtempSync(join(tmpdir(), "ac-sources-"));
mkdirSync(join(dir, "data"));
copyFileSync(join(prompt, "data/emojis.json"), join(dir, "data/emojis.json"));
copyFileSync(join(prompt, "emoji-autocomplete.ts"), join(dir, "emoji.ts"));
copyFileSync(join(prompt, "github-ref-autocomplete.ts"), join(dir, "gh.ts"));
const emoji = await import(pathToFileURL(join(dir, "emoji.ts")).href);
const gh = await import(pathToFileURL(join(dir, "gh.ts")).href);
rmSync(dir, { recursive: true });

const emojiInputs = [
	":", "note:", ":joy", "hi :sm", "http://example", "foo:bar", ":zzzzzz", ":jor", ":s", ":D", ":-", ":p", ":3",
	"(:tad", "[:+", "{:-1", "> :thumbs", "\t:fire", "x\n:ro", ":ROCKET", ":1", ":o", ":'", "é :heart", ":a", ":b",
];
const inlineInputs = [
	":joy", "hello", ":joy:", "hi :tada:", ":notrealemoji:", "foo:joy:", ":jordan:", "::", ":JOY:", "(:+1:",
	":) ", "x :-) ", "a;) ", "url:) ", ":D\t", "<3 ", "</3 ", ":-\\ ", "B) ", ":) x", ":'( ", "o.O\n", ":", "",
];
const expandInputs = [
	"hi :)", "x :-) y", "a:)", ":) :( :D", "no emoticons", "<3", "end ;)", ":-) :)", "(:) [;)", "x", "8) B)", ":)x",
];
const ghInputs = [
	"#3164", "see #12", "pr #3164", "PULL #7", "issue #42", "Issue\t#9", "foo#12", "C#12", "owner/repo#5", "#0",
	"#012", "(#5", "\"#6", "<#7", "=#8", "`#9", "issues #3", "pr#3", "#", "text #12a", "line\n#4",
];

const out = {
	source: "oh-my-pi prompt/emoji-autocomplete.ts, prompt/github-ref-autocomplete.ts",
	emoji: emojiInputs.map((input) => {
		const r = emoji.getEmojiSuggestions(input);
		let applied = null;
		if (r) {
			const line = `${input}|after`;
			const col = input.length;
			const a = emoji.applyEmojiCompletion([line], 0, col, r.items[0], r.prefix);
			applied = { lines: a.lines, cursorCol: Buffer.byteLength(a.lines[0].slice(0, a.cursorCol)) };
		}
		return { input, result: r, applied };
	}),
	inline: inlineInputs.map((input) => ({ input, result: emoji.tryEmojiInlineReplace(input) })),
	expand: expandInputs.map((input) => ({ input, output: emoji.expandEmoticons(input) })),
	github: ghInputs.map((input) => ({
		input,
		context: gh.getGithubRefContext(input),
		suggestions: gh.getGithubRefSuggestions(input),
	})),
};
writeFileSync(join(here, "autocomplete-sources-parity.json"), `${JSON.stringify(out, null, 1)}\n`);
console.log(`${out.emoji.length + out.inline.length + out.expand.length + out.github.length} cases`);
