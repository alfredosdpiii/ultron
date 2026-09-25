import { spawnSync } from "node:child_process";
import { readdirSync, readFileSync } from "node:fs";

/**
 * Whole-tree memory cap for an RLM kernel: the kernel plus every process its cells spawn.
 *
 * - "cgroup": the kernel is started inside a transient systemd user scope
 *   (`systemd-run --user --scope -p MemoryMax=… -p MemorySwapMax=0`), so the kernel's own memory
 *   controller enforces the cap; the host reads the scope's `memory.events` to report an OOM kill clearly.
 *   Needs no root, only a running user manager with the memory controller delegated.
 * - "watchdog": the host sums anonymous and shared resident memory (RssAnon + RssShmem) of the kernel's
 *   descendant tree from /proc every ~500 ms and SIGKILLs the tree when it is over the cap. Shared anonymous
 *   pages of forked children are counted once per process, so the sum can overestimate, never underestimate.
 * - "off": no tree cap (non-Linux, or the cap is 0).
 *
 * Per-process RLIMIT_DATA stays in force in every mode.
 */
export type TreeMemoryBackend = "cgroup" | "watchdog" | "off";

/** Limit from ULTRON_RLM_MAX_TREE_MEMORY_MB, else twice the per-process limit; 0 disables. */
export function kernelTreeMemoryLimit(
	source: NodeJS.ProcessEnv,
	perProcessMb: number,
	override: number | undefined,
): number {
	const text = source.ULTRON_RLM_MAX_TREE_MEMORY_MB?.trim();
	const fallback = perProcessMb * 2;
	const value = override ?? (text && /^[0-9]+$/.test(text) ? Number(text) : fallback);
	if (!Number.isSafeInteger(value) || value < 0) throw new Error("RLM maxTreeMemoryMb must be a non-negative integer");
	return value;
}

let cgroupAvailable: boolean | undefined;

/**
 * Whether a transient user scope with a memory cap can be created here. Probed once per process by actually
 * starting a tiny scope and reading back its `memory.max`, which checks systemd-run, the user manager, and
 * memory-controller delegation together.
 */
export function cgroupScopeAvailable(): boolean {
	if (cgroupAvailable !== undefined) return cgroupAvailable;
	cgroupAvailable = false;
	if (process.platform !== "linux") return false;
	try {
		const probe = spawnSync(
			"systemd-run",
			[
				"--user",
				"--scope",
				"--quiet",
				"--collect",
				"-p",
				"MemoryMax=64M",
				"-p",
				"MemorySwapMax=0",
				"-p",
				"OOMPolicy=continue",
				"--",
				"/bin/sh",
				"-c",
				'cat "/sys/fs/cgroup$(cut -d: -f3 /proc/self/cgroup)/memory.max"',
			],
			{ encoding: "utf8", timeout: 5_000, stdio: ["ignore", "pipe", "ignore"] },
		);
		cgroupAvailable = probe.status === 0 && probe.stdout.trim() === String(64 * 1024 * 1024);
	} catch {
		cgroupAvailable = false;
	}
	return cgroupAvailable;
}

/** Backend for a cap: ULTRON_RLM_TREE_MEMORY_BACKEND (auto|cgroup|watchdog|off) or an explicit choice. */
export function treeMemoryBackend(
	capMb: number,
	requested: string | undefined = process.env.ULTRON_RLM_TREE_MEMORY_BACKEND,
): TreeMemoryBackend {
	if (capMb <= 0 || process.platform !== "linux") return "off";
	const choice = requested?.trim().toLowerCase() || "auto";
	if (choice === "off") return "off";
	if (choice === "watchdog") return "watchdog";
	// "cgroup", "auto" and anything unrecognized: a capped scope when this system allows one, else the watchdog.
	return cgroupScopeAvailable() ? "cgroup" : "watchdog";
}

/** Command prefix that starts the kernel inside a capped transient scope; systemd-run execs the command itself. */
export function cgroupScopeCommand(capMb: number): string[] {
	return [
		"systemd-run",
		"--user",
		"--scope",
		"--quiet",
		"--collect",
		"-p",
		`MemoryMax=${capMb}M`,
		"-p",
		"MemorySwapMax=0",
		// Let the host see the OOM kill and stop the tree itself with a clear error, instead of systemd
		// stopping the scope with SIGTERM.
		"-p",
		"OOMPolicy=continue",
		"--",
	];
}

/** The cgroup v2 directory of `pid`, or undefined. */
export function processCgroupDir(pid: number): string | undefined {
	try {
		const line = readFileSync(`/proc/${pid}/cgroup`, "utf8")
			.split("\n")
			.find((entry) => entry.startsWith("0::"));
		return line ? `/sys/fs/cgroup${line.slice(3)}` : undefined;
	} catch {
		return undefined;
	}
}

/** OOM kills recorded in a cgroup's memory.events, or undefined when the cgroup is gone. */
export function cgroupOomKills(dir: string): number | undefined {
	try {
		const match = /^oom_kill (\d+)$/m.exec(readFileSync(`${dir}/memory.events`, "utf8"));
		return match ? Number(match[1]) : 0;
	} catch {
		return undefined;
	}
}

/**
 * Linux descendants of `root`, found before the group is killed so that children
 * which left the kernel's process group (setsid, setpgid) are still terminated.
 * A process that already reparented away (double-fork daemon) is not owned anymore.
 */
export function descendantPids(root: number): number[] {
	if (process.platform !== "linux") return [];
	const children = new Map<number, number[]>();
	let entries: string[];
	try {
		entries = readdirSync("/proc");
	} catch {
		return [];
	}
	for (const entry of entries) {
		if (!/^[0-9]+$/.test(entry)) continue;
		try {
			const stat = readFileSync(`/proc/${entry}/stat`, "utf8");
			// Field 4 (ppid) follows the parenthesized command name, which may contain spaces.
			const parent = Number(stat.slice(stat.lastIndexOf(")") + 2).split(" ")[1]);
			const list = children.get(parent) ?? [];
			list.push(Number(entry));
			children.set(parent, list);
		} catch {
			/* The process exited during the scan. */
		}
	}
	const result: number[] = [];
	const queue = [root];
	while (queue.length > 0) {
		for (const child of children.get(queue.shift()!) ?? []) {
			result.push(child);
			queue.push(child);
		}
	}
	return result;
}

/** RssAnon + RssShmem of one process in bytes; 0 when it is gone or a kernel thread. */
export function processPrivateRssBytes(pid: number): number {
	try {
		const status = readFileSync(`/proc/${pid}/status`, "utf8");
		let kib = 0;
		for (const match of status.matchAll(/^(?:RssAnon|RssShmem):\s+(\d+) kB$/gm)) kib += Number(match[1]);
		return kib * 1024;
	} catch {
		return 0;
	}
}

/** Resident memory of `root` and its descendant tree, as counted by the watchdog. */
export function treeRssBytes(root: number): number {
	let total = processPrivateRssBytes(root);
	for (const pid of descendantPids(root)) total += processPrivateRssBytes(pid);
	return total;
}
