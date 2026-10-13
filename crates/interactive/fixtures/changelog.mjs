// `/changelog` evidence: utils/changelog.ts on link-normalization samples and on the package's CHANGELOG.md (entry
// versions and the five newest entries after normalization). Writes changelog.json.
// Usage: node crates/interactive/fixtures/changelog.mjs
import { readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { normalizeChangelogLinks, parseChangelog } from "../../../packages/coding-agent/src/utils/changelog.ts";

const here = dirname(fileURLToPath(import.meta.url));
const changelogPath = join(here, "../../../packages/coding-agent/CHANGELOG.md");
const samples = [
	"See [docs](docs/rlm.md) and [dir](docs/) and [abs](/README.md).",
	"[up](../../README.md) [frag](#heading) [web](https://example.com/x) [proto](//cdn.example.com/a)",
	"[legacy](https://github.com/badlogic/pi-mono/blob/main/packages/ai/README.md) ![img](images/a b.png)",
	"[q](docs/a.md?raw=1#top) [tree](https://github.com/earendil-works/pi/tree/master/packages) [t](src/x \"title\")",
	"[unicode](docs/naïve file.md) [mailto](mailto:a@b.c) [dots](./docs/../src/./main.ts)",
];
const entries = parseChangelog(changelogPath);
const out = {
	samples: samples.map((markdown) => ({ markdown, version: "1.2.3", normalized: normalizeChangelogLinks(markdown, "1.2.3") })),
	changelog: {
		versions: entries.map((e) => `${e.major}.${e.minor}.${e.patch}`),
		newest: entries.slice(0, 5).map((e) => normalizeChangelogLinks(e.content, e)),
		content: readFileSync(changelogPath, "utf8").length,
	},
};
writeFileSync(join(here, "changelog.json"), `${JSON.stringify(out, null, 1)}\n`);
