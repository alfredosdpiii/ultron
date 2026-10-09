/**
 * `/autoreview [on|off]` (client-tui-pi-commands.ts): the status; `on` lists the gh accounts, asks which one when
 * there are several, saves `autoreview.enabled` and `autoreview.accounts` through the worker as a patch, writes
 * the user service and runs its enable commands; `off` saves, stops and removes the service. The worker's setting
 * case patches the object (other `autoreview.*` keys stay), and `ultron autoreview run`/`once` refuse while off.
 */
import { existsSync, mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, describe, expect, test, vi } from "vitest";
import { SettingsManager } from "../src/core/settings-manager.ts";
import { setAutoreview } from "../src/experimental/client-tui-pi-commands.ts";
import { SERVICE_NAME } from "../src/ultron/autoreview/service.ts";

const dirs: string[] = [];
afterEach(() => {
	for (const dir of dirs.splice(0)) rmSync(dir, { recursive: true, force: true });
});

function fakeHost(options: {
	accounts: Array<{ login: string; host: string }>;
	choice?: string;
	settings?: Record<string, unknown>;
	gh?: "missing";
}) {
	const settingsManager = SettingsManager.inMemory(options.settings ?? {});
	const commands: string[] = [];
	const status: string[] = [];
	const setSetting = vi.fn(async (key: string, value: Record<string, unknown>) => {
		// The worker's case: a patch of the autoreview object, null removes a key.
		if (key !== "autoreview") throw new Error(`unexpected key ${key}`);
		settingsManager.setAutoreviewSettings({
			...("enabled" in value ? { enabled: value.enabled === null ? undefined : (value.enabled as boolean) } : {}),
			...("accounts" in value
				? { accounts: value.accounts === null ? undefined : (value.accounts as string[]) }
				: {}),
		});
		return { applied: "live" as const };
	});
	const host = {
		settingsManager,
		settingsMirror: { apply: async () => {} },
		control: () => ({ setSetting }),
		showStatus: (text: string) => status.push(text),
		applySettingLocally: () => {},
		select: vi.fn(async () => options.choice),
		runCommand: vi.fn(async (command: string, args: readonly string[]) => {
			commands.push([command, ...args].join(" "));
			if (command === "gh") {
				if (options.gh === "missing")
					return { code: 127, stdout: "", stderr: "", error: new Error("gh: not found") };
				const hosts: Record<string, Array<{ login: string; state: string; active: boolean }>> = {};
				for (const account of options.accounts) {
					hosts[account.host] ??= [];
					hosts[account.host]!.push({ login: account.login, state: "success", active: true });
				}
				return { code: 0, stdout: JSON.stringify({ hosts }), stderr: "" };
			}
			return { code: 0, stdout: "", stderr: "" };
		}),
	} as never;
	return {
		host,
		settingsManager,
		commands,
		status,
		setSetting,
		select: (host as { select: ReturnType<typeof vi.fn> }).select,
	};
}

describe("/autoreview", () => {
	test("on with several accounts asks which, saves the choice and enabled, writes and starts the service; off stops and removes it", async () => {
		const home = mkdtempSync(join(tmpdir(), "ultron-autoreview-toggle-"));
		dirs.push(home);
		const env = { PATH: "/usr/bin:/bin", ULTRON_SELF_COMMAND: JSON.stringify(["/opt/ultron/bin/ultron"]) };
		const { host, settingsManager, commands, status, setSetting, select } = fakeHost({
			accounts: [
				{ login: "me", host: "github.com" },
				{ login: "bot", host: "github.com" },
			],
			choice: "bot (github.com)",
			settings: { autoreview: { model: "p/m" } },
		});
		const options = { platform: "linux" as const, home, env };
		await setAutoreview(host, "", options);
		expect(status.at(-1)).toBe(
			"Autoreview is on: reviewing as every logged-in gh account; service not installed. /autoreview on|off",
		);

		await setAutoreview(host, "on", options);
		expect(select).toHaveBeenCalledWith("Review pull requests as", [
			"me (github.com)",
			"bot (github.com)",
			"All logged-in accounts",
		]);
		expect(setSetting.mock.calls.map(([key, value]) => [key, value])).toEqual([
			["autoreview", { enabled: true, accounts: ["github.com/bot"] }],
		]);
		// Other autoreview settings survive the patch.
		expect(settingsManager.getAutoreviewSettings()).toMatchObject({
			enabled: true,
			accounts: ["github.com/bot"],
			model: "p/m",
		});
		const unit = join(home, ".config", "systemd", "user", `${SERVICE_NAME}.service`);
		expect(existsSync(unit)).toBe(true);
		expect(readFileSync(unit, "utf8")).toContain("ExecStart=/opt/ultron/bin/ultron autoreview run");
		expect(commands).toEqual([
			"gh auth status --json hosts",
			"systemctl --user daemon-reload",
			`systemctl --user enable --now ${SERVICE_NAME}.service`,
		]);
		expect(status.at(-1)).toBe(
			`Autoreview on, reviewing as github.com/bot: the service is started (${unit}); it reviews pull requests that request or mention the account`,
		);
		await setAutoreview(host, "", options);
		expect(status.at(-1)).toBe(
			`Autoreview is on: reviewing as github.com/bot; service installed (${unit}). /autoreview on|off`,
		);

		commands.length = 0;
		await setAutoreview(host, "off", options);
		expect(setSetting.mock.calls.at(-1)!.slice(0, 2)).toEqual(["autoreview", { enabled: false }]);
		expect(settingsManager.getAutoreviewSettings()).toMatchObject({
			enabled: false,
			accounts: ["github.com/bot"],
			model: "p/m",
		});
		expect(commands).toEqual([`systemctl --user disable --now ${SERVICE_NAME}.service`]);
		expect(existsSync(unit)).toBe(false);
		expect(status.at(-1)).toBe(
			`Autoreview off: ultron autoreview run and its service review nothing; service removed (${unit})`,
		);
		await setAutoreview(host, "maybe", options);
		expect(status.at(-1)).toBe("Usage: /autoreview on|off");
	});

	test("one account needs no picker and is saved as every account; a cancelled picker changes nothing; no gh or no login says so", async () => {
		const home = mkdtempSync(join(tmpdir(), "ultron-autoreview-toggle-"));
		dirs.push(home);
		const env = { PATH: "/usr/bin:/bin", ULTRON_SELF_COMMAND: JSON.stringify(["ultron"]) };
		const one = fakeHost({ accounts: [{ login: "me", host: "github.com" }] });
		await setAutoreview(one.host, "on", { platform: "linux", home, env });
		expect(one.select).not.toHaveBeenCalled();
		expect(one.setSetting.mock.calls.map(([, value]) => value)).toEqual([{ enabled: true, accounts: null }]);
		expect(one.status.at(-1)).toContain("Autoreview on, reviewing as github.com/me: the service is started");

		const cancelled = fakeHost({
			accounts: [
				{ login: "me", host: "github.com" },
				{ login: "bot", host: "ghe.corp" },
			],
		});
		await setAutoreview(cancelled.host, "on", { platform: "linux", home, env });
		expect(cancelled.setSetting).not.toHaveBeenCalled();
		expect(cancelled.status.at(-1)).toBe("Autoreview left as it was");

		const all = fakeHost({
			accounts: [
				{ login: "me", host: "github.com" },
				{ login: "bot", host: "ghe.corp" },
			],
			choice: "All logged-in accounts",
		});
		await setAutoreview(all.host, "on", { platform: "linux", home, env });
		expect(all.setSetting.mock.calls.map(([, value]) => value)).toEqual([{ enabled: true, accounts: null }]);
		expect(all.status.at(-1)).toContain("reviewing as every logged-in account");

		const none = fakeHost({ accounts: [] });
		await setAutoreview(none.host, "on", { platform: "linux", home, env });
		expect(none.status.at(-1)).toBe("No gh account is logged in: run gh auth login, then /autoreview on");
		const missing = fakeHost({ accounts: [], gh: "missing" });
		await setAutoreview(missing.host, "on", { platform: "linux", home, env });
		expect(missing.status.at(-1)).toBe(
			"Autoreview needs the GitHub CLI (gh): install it and run gh auth login, then /autoreview on",
		);

		// No user service on this platform: the setting is saved and the loop is named instead.
		const other = fakeHost({ accounts: [{ login: "me", host: "github.com" }] });
		await setAutoreview(other.host, "on", { platform: "freebsd", home, env });
		expect(other.status.at(-1)).toBe(
			"Autoreview on, reviewing as github.com/me. No user service on freebsd: run ultron autoreview run under your own supervisor",
		);
	});
});
