// Generates crates/interactive/fixtures/startup-notices.json: interactive-mode.ts's startup notices, run as the real
// methods over a bare host:
// - `getChangelogForDisplay` (which entries of CHANGELOG.md are new since `lastChangelogVersion`, and what it saves),
//   over a package directory (PI_PACKAGE_DIR) with a sample CHANGELOG.md at version 1.4.0;
// - `showStartupNoticesIfNeeded` (the "What's New" block, and the condensed line under `collapseChangelog`);
// - `showNewVersionNotification` (the "Update Available" block, with and without OSC 8 hyperlinks);
// - `showWarning` with the Anthropic subscription-auth warning, and `maybeWarnAboutAnthropicSubscriptionAuth`'s
//   decision over providers, the `warnings.anthropicExtraUsage` setting and the stored auth.
// Rendered in both themes at two widths. crates/interactive/tests/startup_notices.rs replays it.
// Usage: node --import packages/coding-agent/src/experimental/source-resolver.ts \
//   crates/interactive/fixtures/generate-startup-notices.mjs
process.env.FORCE_COLOR = "3";
import "../../cli/fixtures/offline-guard.mjs";
import { mkdirSync, mkdtempSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { createRequire } from "node:module";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const tmp = realpathSync(mkdtempSync(join(tmpdir(), "ultron-startup-notices-")));
const pkg = join(tmp, "package");
mkdirSync(pkg, { recursive: true });
writeFileSync(join(pkg, "package.json"), JSON.stringify({ name: "ultron-agent", version: "1.4.0", piConfig: { name: "ultron", configDir: ".ultron" } }));
const CHANGELOG = [
	"# Changelog",
	"",
	"## [Unreleased]",
	"",
	"### Added",
	"- Pending work",
	"",
	"## [1.4.0] - 2026-10-01",
	"",
	"### Added",
	"- A [guide](docs/guide.md) for the `x` flag",
	"",
	"```",
	"ultron --x",
	"```",
	"",
	"## [1.3.2] - 2026-09-20",
	"",
	"### Fixed",
	"- Fixed a crash ([#12](https://github.com/alfredosdpiii/ultron/issues/12))",
	"",
	"## [1.3.0] - 2026-09-01",
	"",
	"- Old news",
	"",
].join("\n");
writeFileSync(join(pkg, "CHANGELOG.md"), CHANGELOG);
Object.assign(process.env, {
	HOME: join(tmp, "home"),
	ULTRON_CODING_AGENT_DIR: join(tmp, "agent"),
	ULTRON_SERVER_DIR: join(tmp, "server"),
	PI_PACKAGE_DIR: pkg,
});
const ca = (p) => import(join(here, "../../../packages/coding-agent/src", p));
const T = await ca("modes/interactive/theme/theme.ts");
const { validateThemeJson } = await ca("modes/interactive/theme/theme-json.ts");
T.setThemeJsonValidator(validateThemeJson);
const tuiUrl = pathToFileURL(createRequire(join(here, "../../../packages/coding-agent/package.json")).resolve("@ultron/tui")).href;
const { Container, setCapabilities } = await import(tuiUrl);
const { InteractiveMode } = await ca("modes/interactive/interactive-mode.ts");
const { SettingsManager } = await ca("core/settings-manager.ts");
const { getMarkdownTheme } = await ca("modes/interactive/theme/theme.ts");
const themeDir = join(here, "../../../packages/coding-agent/src/modes/interactive/theme");
const proto = InteractiveMode.prototype;

// getChangelogForDisplay: resumed sessions skip it; no lastChangelogVersion records the version and shows nothing.
const changelog = [];
for (const [last, messages] of [[undefined, 0], ["1.3.0", 0], ["1.3.2", 0], ["1.4.0", 0], ["2.0.0", 0], ["1.3.0", 2], ["1.3", 0], ["v1.3.1", 0]]) {
	const settingsManager = SettingsManager.inMemory(last === undefined ? {} : { lastChangelogVersion: last });
	const host = { session: { state: { messages: Array.from({ length: messages }, () => ({})) } }, settingsManager };
	const markdown = proto.getChangelogForDisplay.call(host);
	changelog.push({ last: last ?? null, messages, markdown: markdown ?? null, saved: settingsManager.getLastChangelogVersion() ?? null });
}

const WARNING =
	"Anthropic subscription auth is active. Third-party harness usage draws from extra usage and is billed per token, not your Claude plan limits. Manage extra usage at https://claude.ai/settings/usage. Disable this warning in /settings.";

// maybeWarnAboutAnthropicSubscriptionAuth: which model and auth warn (once).
const warnings = [];
for (const [provider, settings, authType, apiKey] of [
	["anthropic", {}, "oauth", undefined],
	["anthropic", {}, "api_key", "sk-ant-oat01-abc"],
	["anthropic", {}, "api_key", "sk-ant-api03-abc"],
	["anthropic", {}, undefined, undefined],
	["anthropic", { warnings: { anthropicExtraUsage: false } }, "oauth", undefined],
	["anthropic", { warnings: { anthropicExtraUsage: true } }, "oauth", undefined],
	["openai", {}, "oauth", "sk-ant-oat01-abc"],
	["claude-subscription", {}, "oauth", undefined],
]) {
	const shown = [];
	const host = {
		settingsManager: SettingsManager.inMemory(settings),
		anthropicSubscriptionWarningShown: false,
		session: {
			model: { provider, id: "m" },
			modelRuntime: {
				checkAuth: async () => (authType ? { type: authType } : undefined),
				getAuth: async () => (apiKey ? { auth: { apiKey } } : undefined),
			},
		},
		showWarning: (m) => shown.push(m),
	};
	await proto.maybeWarnAboutAnthropicSubscriptionAuth.call(host);
	await proto.maybeWarnAboutAnthropicSubscriptionAuth.call(host);
	warnings.push({ provider, settings, authType: authType ?? null, apiKey: apiKey ?? null, shown });
}

const render = [];
const whatsNew = changelog.find((c) => c.last === "1.3.0" && c.messages === 0).markdown;
for (const [name, mode] of [["dark", "truecolor"], ["light", "256color"]]) {
	T.setThemeInstance(T.loadThemeFromPath(join(themeDir, `${name}.json`), mode));
	for (const width of [40, 100]) {
		for (const collapse of [false, true]) {
			for (const codeBlockIndent of ["  ", "\t> "]) {
				if (collapse && codeBlockIndent !== "  ") continue;
				setCapabilities({ images: null, trueColor: mode === "truecolor", hyperlinks: false });
				const host = {
					startupNoticesShown: false,
					changelogMarkdown: whatsNew,
					chatContainer: new Container(),
					settingsManager: { getCollapseChangelog: () => collapse },
					version: "1.4.0",
					getMarkdownThemeWithSettings: () => ({ ...getMarkdownTheme(), codeBlockIndent }),
				};
				proto.showStartupNoticesIfNeeded.call(host);
				render.push({ kind: "whats_new", theme: `${name}-${mode}`, width, collapse, codeBlockIndent, lines: host.chatContainer.render(width) });
			}
		}
		for (const hyperlinks of [false, true]) {
			setCapabilities({ images: null, trueColor: mode === "truecolor", hyperlinks });
			const host = { chatContainer: new Container(), ui: { requestRender() {} } };
			proto.showNewVersionNotification.call(host, { version: "1.5.0", packageName: "ultron-agent" });
			render.push({ kind: "new_version", theme: `${name}-${mode}`, width, hyperlinks, version: "1.5.0", lines: host.chatContainer.render(width) });
		}
		const host = { chatContainer: new Container(), ui: { requestRender() {} } };
		proto.showWarning.call(host, WARNING);
		render.push({ kind: "warning", theme: `${name}-${mode}`, width, lines: host.chatContainer.render(width) });
	}
}
rmSync(tmp, { recursive: true, force: true });
writeFileSync(join(here, "startup-notices.json"), `${JSON.stringify({ warning: WARNING, changelog, warnings, render }, null, 1)}\n`);
console.log(`wrote ${changelog.length} changelog cases, ${warnings.length} warning cases, ${render.length} renders`);
