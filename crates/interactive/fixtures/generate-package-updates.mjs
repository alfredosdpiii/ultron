// Generates crates/interactive/fixtures/package-updates.json: the startup package update notice of
// modes/interactive/interactive-mode.ts (`showPackageUpdateNotification`, called after `checkForAvailableUpdates`
// finds packages behind their source), run as the real method over a bare chat container, rendered in both themes
// at several widths. crates/interactive/tests/package_updates.rs replays it.
// Usage: node --import packages/coding-agent/src/experimental/source-resolver.ts \
//   crates/interactive/fixtures/generate-package-updates.mjs
process.env.FORCE_COLOR = "3";
import "../../cli/fixtures/offline-guard.mjs";
import { mkdtempSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { createRequire } from "node:module";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const tmp = realpathSync(mkdtempSync(join(tmpdir(), "ultron-package-updates-")));
Object.assign(process.env, { HOME: join(tmp, "home"), ULTRON_CODING_AGENT_DIR: join(tmp, "agent"), ULTRON_SERVER_DIR: join(tmp, "server") });
const ca = (p) => import(join(here, "../../../packages/coding-agent/src", p));
const T = await ca("modes/interactive/theme/theme.ts");
const { validateThemeJson } = await ca("modes/interactive/theme/theme-json.ts");
T.setThemeJsonValidator(validateThemeJson);
const tuiUrl = pathToFileURL(createRequire(join(here, "../../../packages/coding-agent/package.json")).resolve("@ultron/tui")).href;
const { Container } = await import(tuiUrl);
const { InteractiveMode } = await ca("modes/interactive/interactive-mode.ts");
const themeDir = join(here, "../../../packages/coding-agent/src/modes/interactive/theme");

const LISTS = [["foo"], ["@scope/bar", "github.com/test/ext", "a-much-longer-package-name-that-wraps-at-narrow-widths"]];
const cases = [];
for (const [name, mode] of [["dark", "truecolor"], ["light", "256color"]]) {
	T.setThemeInstance(T.loadThemeFromPath(join(themeDir, `${name}.json`), mode));
	for (const width of [30, 80]) {
		for (const packages of LISTS) {
			const host = { chatContainer: new Container(), ui: { requestRender() {} } };
			InteractiveMode.prototype.showPackageUpdateNotification.call(host, packages);
			cases.push({ theme: `${name}-${mode}`, width, packages, lines: host.chatContainer.render(width) });
		}
	}
}
rmSync(tmp, { recursive: true, force: true });
writeFileSync(join(here, "package-updates.json"), `${JSON.stringify({ cases }, null, 1)}\n`);
console.log(`wrote ${cases.length} cases`);
