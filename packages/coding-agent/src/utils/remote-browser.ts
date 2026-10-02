import { existsSync, readFileSync } from "node:fs";

/**
 * Where this process runs relative to the user's browser, for logins that finish with a redirect to a callback
 * server on `localhost`: in a container or over SSH the browser is on another machine (or another network
 * namespace), where `localhost:<port>` is not this process, so the redirect ends on "localhost refused to connect".
 */
export type BrowserReach = "local" | "container" | "ssh";

type Probe = {
	env?: NodeJS.ProcessEnv;
	exists?: (path: string) => boolean;
	read?: (path: string) => string;
};

/** Docker, Podman, Kubernetes, LXC and systemd-nspawn, by the markers each leaves for the processes it runs. */
export function isRunningInContainer(probe: Probe = {}): boolean {
	const env = probe.env ?? process.env;
	const exists = probe.exists ?? existsSync;
	const read = probe.read ?? ((path: string) => readFileSync(path, "utf8"));
	if (env.container?.trim()) return true;
	if (env.KUBERNETES_SERVICE_HOST?.trim()) return true;
	if (exists("/.dockerenv") || exists("/run/.containerenv")) return true;
	try {
		return /docker|containerd|kubepods|libpod|lxc/.test(read("/proc/1/cgroup"));
	} catch {
		return false;
	}
}

export function browserReach(probe: Probe = {}): BrowserReach {
	const env = probe.env ?? process.env;
	// The callback server was put on another interface on purpose (a published container port, an SSH forward):
	// the redirect is expected to arrive, so the login is described as on the user's own machine.
	if (env.ULTRON_OAUTH_CALLBACK_HOST?.trim() || env.PI_OAUTH_CALLBACK_HOST?.trim()) return "local";
	if (isRunningInContainer(probe)) return "container";
	return env.SSH_CONNECTION?.trim() || env.SSH_TTY?.trim() ? "ssh" : "local";
}
