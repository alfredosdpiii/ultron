/**
 * `ultron autoreview install | uninstall`: a user service that runs `ultron autoreview run` and restarts it.
 * Linux: a systemd user unit; macOS: a launchd agent. The files are written (or removed) and the command that
 * enables them is printed: enabling is left to the user.
 */
import { existsSync, mkdirSync, rmSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, join } from "node:path";
import { type SelfCommand, shellQuote } from "../claude/self.ts";

export const SERVICE_NAME = "ultron-autoreview";
export const LAUNCHD_LABEL = "com.ultron.autoreview";

export interface ServiceTarget {
	readonly platform: NodeJS.Platform;
	readonly home?: string;
	readonly env?: NodeJS.ProcessEnv;
}

export interface ServiceFile {
	readonly path: string;
	readonly content: string;
	/** Commands that enable the service (printed, never run). */
	readonly enable: readonly string[];
	readonly disable: readonly string[];
}

function xml(value: string): string {
	return value.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
}

/** The service file for this platform, or undefined where neither systemd nor launchd applies. */
export function serviceFile(self: SelfCommand, target: ServiceTarget): ServiceFile | undefined {
	const home = target.home ?? homedir();
	const env = target.env ?? process.env;
	const command = [self.command, ...self.args, "autoreview", "run"];
	// The service starts without a login shell: carry the PATH that finds gh, git and python now.
	const path = env.PATH ?? "/usr/local/bin:/usr/bin:/bin";
	const agentDir = env.ULTRON_CODING_AGENT_DIR;
	if (target.platform === "linux") {
		const config = env.XDG_CONFIG_HOME?.trim() || join(home, ".config");
		const systemdQuote = (value: string) =>
			/^[A-Za-z0-9_@%+=:,./-]+$/.test(value) ? value : `"${value.replace(/(["\\])/g, "\\$1")}"`;
		const content = [
			"[Unit]",
			"Description=Ultron automated pull-request reviewer",
			"After=network-online.target",
			"Wants=network-online.target",
			"",
			"[Service]",
			`ExecStart=${command.map(systemdQuote).join(" ")}`,
			`Environment=${systemdQuote(`PATH=${path}`)}`,
			...(agentDir ? [`Environment=${systemdQuote(`ULTRON_CODING_AGENT_DIR=${agentDir}`)}`] : []),
			"Restart=always",
			"RestartSec=15",
			"",
			"[Install]",
			"WantedBy=default.target",
			"",
		].join("\n");
		return {
			path: join(config, "systemd", "user", `${SERVICE_NAME}.service`),
			content,
			enable: ["systemctl --user daemon-reload", `systemctl --user enable --now ${SERVICE_NAME}.service`],
			disable: [`systemctl --user disable --now ${SERVICE_NAME}.service`],
		};
	}
	if (target.platform === "darwin") {
		const file = join(home, "Library", "LaunchAgents", `${LAUNCHD_LABEL}.plist`);
		const logs = join(home, "Library", "Logs", `${SERVICE_NAME}.log`);
		const variables: Array<[string, string]> = [["PATH", path]];
		if (agentDir) variables.push(["ULTRON_CODING_AGENT_DIR", agentDir]);
		const content = [
			'<?xml version="1.0" encoding="UTF-8"?>',
			'<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">',
			'<plist version="1.0">',
			"<dict>",
			`\t<key>Label</key><string>${LAUNCHD_LABEL}</string>`,
			"\t<key>ProgramArguments</key>",
			"\t<array>",
			...command.map((part) => `\t\t<string>${xml(part)}</string>`),
			"\t</array>",
			"\t<key>EnvironmentVariables</key>",
			"\t<dict>",
			...variables.map(([name, value]) => `\t\t<key>${name}</key><string>${xml(value)}</string>`),
			"\t</dict>",
			"\t<key>KeepAlive</key><true/>",
			"\t<key>RunAtLoad</key><true/>",
			`\t<key>StandardErrorPath</key><string>${xml(logs)}</string>`,
			"</dict>",
			"</plist>",
			"",
		].join("\n");
		return {
			path: file,
			content,
			enable: [`launchctl load -w ${shellQuote(file)}`],
			disable: [`launchctl unload -w ${shellQuote(file)}`],
		};
	}
	return undefined;
}

export function installService(file: ServiceFile): void {
	mkdirSync(dirname(file.path), { recursive: true });
	writeFileSync(file.path, file.content, { mode: 0o644 });
}

/** Remove the service file; false when there was none. */
export function uninstallService(file: ServiceFile): boolean {
	if (!existsSync(file.path)) return false;
	rmSync(file.path, { force: true });
	return true;
}
