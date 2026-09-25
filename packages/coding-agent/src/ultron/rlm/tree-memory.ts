import { spawnSync } from "node:child_process";
import { readdirSync, readFileSync } from "node:fs";

/**
 * Whole-tree memory cap for an RLM kernel: the kernel plus every process its cells spawn.
 *
 * - "cgroup": the kernel is started inside a transient systemd user scope
 *   (`systemd-run --user --scope -p MemoryMax=… -p MemorySwapMax=0`), so the kernel's own memory
 *   controller enforces the cap; the host reads the scope's `memory.events` to report an OOM kill clearly.
 *   Needs no root, only a running user manager with the memory controller delegated.
 * - "watchdog": the host sums the proportional anonymous and shared memory (Pss_Anon + Pss_Shmem + SwapPss, so
 *   copy-on-write pages shared by forked children count once; RssAnon + RssShmem + VmSwap where PSS is
 *   unavailable) of the kernel's descendant tree from /proc and SIGKILLs the tree when it is over the cap. It
 *   polls every 500 ms, every 250 ms above 40% of the cap and every 100 ms above 70%, so an allocation burst
 *   can overshoot by what the tree allocates within one poll; RLIMIT_DATA still bounds each process.
 *
 * In every mode the runtime makes the kernel a child subreaper (PR_SET_CHILD_SUBREAPER), so processes that
 * double-fork out of a cell are reparented to the kernel and stay counted and killable, and the host kills the
 * kernel's process group, its descendant tree, and (for a scope) every process left in the scope.
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

interface ProcStat {
	ppid: number;
	state: string;
	/** Start time in clock ticks since boot; tells a live process apart from a later one reusing its pid. */
	startTime: number;
}

/** Parent, state and start time of one process from /proc/<pid>/stat, or undefined when it is gone. */
export function processStat(pid: number): ProcStat | undefined {
	try {
		const stat = readFileSync(`/proc/${pid}/stat`, "utf8");
		// After the parenthesized command name (which may contain spaces): state(3) ppid(4) ... starttime(22).
		const fields = stat.slice(stat.lastIndexOf(")") + 2).split(" ");
		return { state: fields[0]!, ppid: Number(fields[1]), startTime: Number(fields[19]) };
	} catch {
		return undefined;
	}
}

/**
 * Live Linux descendants of `root` (zombies are skipped: they hold no memory and cannot be signalled).
 * They are found before the group is killed so that children which left the kernel's process group
 * (setsid, setpgid) are still terminated. The runtime makes the kernel a child subreaper, so a process that
 * double-forks away from its parent is reparented to the kernel and stays in this tree while the kernel lives.
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
		const stat = processStat(Number(entry));
		// The process exited during the scan, or is a zombie (which has no children of its own).
		if (!stat || stat.state === "Z" || stat.state === "X") continue;
		const list = children.get(stat.ppid) ?? [];
		list.push(Number(entry));
		children.set(stat.ppid, list);
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

/** RssAnon + RssShmem + VmSwap of one process in bytes, from /proc/<pid>/status; 0 when it is gone. */
export function processPrivateRssBytes(pid: number): number {
	try {
		const status = readFileSync(`/proc/${pid}/status`, "utf8");
		let kib = 0;
		for (const match of status.matchAll(/^(?:RssAnon|RssShmem|VmSwap):\s+(\d+) kB$/gm)) kib += Number(match[1]);
		return kib * 1024;
	} catch {
		return 0;
	}
}

/**
 * Proportional anonymous and shared memory of one process in bytes: Pss_Anon + Pss_Shmem + SwapPss from
 * /proc/<pid>/smaps_rollup, where a page shared by N processes (copy-on-write pages after fork, shared memory)
 * counts 1/N in each, so summing over a tree counts it once. Falls back to RssAnon + RssShmem + VmSwap when
 * smaps_rollup or its Pss_Anon field (Linux 5.9+) is unavailable. 0 when the process is gone or a kernel thread.
 */
export function processMemoryBytes(pid: number): number {
	let rollup: string;
	try {
		rollup = readFileSync(`/proc/${pid}/smaps_rollup`, "utf8");
	} catch {
		return processPrivateRssBytes(pid);
	}
	if (!/^Pss_Anon:/m.test(rollup)) return rollup.trim() === "" ? 0 : processPrivateRssBytes(pid);
	let kib = 0;
	for (const match of rollup.matchAll(/^(?:Pss_Anon|Pss_Shmem|SwapPss):\s+(\d+) kB$/gm)) kib += Number(match[1]);
	return kib * 1024;
}

/** Memory of `root` and its live descendant tree as counted by the watchdog, with the descendants seen. */
export function treeMemoryUsage(root: number): { bytes: number; descendants: number[] } {
	const descendants = descendantPids(root);
	let bytes = processMemoryBytes(root);
	for (const pid of descendants) bytes += processMemoryBytes(pid);
	return { bytes, descendants };
}

/** Watchdog poll interval for a tree at `bytes` of a `capBytes` cap: faster as it nears the cap. */
export function watchdogPollMs(bytes: number, capBytes: number): number {
	if (bytes >= capBytes * 0.7) return 100;
	if (bytes >= capBytes * 0.4) return 250;
	return 500;
}

/** Pids listed in a cgroup's cgroup.procs (and its child cgroups), or [] when it is gone. */
export function cgroupPids(dir: string): number[] {
	const pids: number[] = [];
	try {
		for (const line of readFileSync(`${dir}/cgroup.procs`, "utf8").split("\n")) {
			if (/^[0-9]+$/.test(line)) pids.push(Number(line));
		}
		for (const entry of readdirSync(dir, { withFileTypes: true })) {
			if (entry.isDirectory()) pids.push(...cgroupPids(`${dir}/${entry.name}`));
		}
	} catch {
		/* The scope is gone. */
	}
	return pids;
}

function pause(ms: number): void {
	Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, ms);
}

function signal(pid: number, name: NodeJS.Signals): void {
	try {
		process.kill(pid, name);
	} catch {
		/* Already gone. */
	}
}

export interface KillTreeOptions {
	/** Whether `root` leads its own process group (spawned detached), so the group is killed too. */
	ownsProcessGroup: boolean;
	/** Whether `root` is still the host's unreaped child, so its pid cannot belong to another process yet. */
	rootAlive: boolean;
	/** Processes known to have been in the tree earlier, with their start times, killed if still the same process. */
	known?: ReadonlyMap<number, number>;
	/** The kernel's cgroup scope; every process still in it is killed. */
	cgroupDir?: string;
}

/**
 * SIGKILLs `root` and every process in its tree. The root is stopped first, so it can neither start new
 * processes nor exit while its descendants are killed; as a child subreaper it then adopts the orphans of each
 * killed descendant, and repeated sweeps (bounded, a few ms apart) kill those too before the root itself dies.
 * The process group, processes seen in earlier watchdog scans, and the cgroup scope's members are killed as well.
 */
export function killProcessTree(root: number, options: KillTreeOptions): void {
	if (options.rootAlive) signal(root, "SIGSTOP");
	if (options.rootAlive && process.platform === "linux") {
		for (let sweep = 0; sweep < 20; sweep++) {
			const pids = descendantPids(root);
			if (pids.length === 0) break;
			for (const pid of pids) signal(pid, "SIGKILL");
			pause(sweep < 5 ? 2 : 10);
		}
	}
	if (options.ownsProcessGroup) signal(-root, "SIGKILL");
	if (options.rootAlive) signal(root, "SIGKILL");
	for (const [pid, startTime] of options.known ?? []) {
		const stat = processStat(pid);
		if (stat && stat.startTime === startTime && stat.state !== "Z") signal(pid, "SIGKILL");
	}
	if (options.cgroupDir) {
		for (const pid of cgroupPids(options.cgroupDir)) if (pid !== process.pid) signal(pid, "SIGKILL");
	}
}
