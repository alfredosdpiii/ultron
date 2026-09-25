import type { KernelExecutionResult } from "./kernel.ts";

/** The part of RlmKernel the pool needs. */
export interface PoolableKernel {
	snapshot(path?: string): Promise<KernelExecutionResult>;
	shutdown(): Promise<void>;
}

export type KernelPoolOptions<K extends PoolableKernel> = {
	/** Create the kernel for a lane. To restore after eviction, pass the same snapshotPath to RlmKernel. */
	create: (lane: string) => K;
	/** Live kernels, counting pinned and running ones. */
	maxLive: number;
	/** Idle time after which sweep() evicts an unpinned kernel. */
	idleTtlMs: number;
	/** Pinned lanes allowed at once (default maxLive). */
	maxPinned?: number;
	/** When this returns a path, eviction snapshots first and refuses if that snapshot fails. */
	snapshotPath?: (lane: string) => string | undefined;
	now?: () => number;
};

export type KernelEvictionReason = "idle" | "capacity" | "explicit" | "close";

/** Record of an eviction attempt. Refusals are recorded too. */
export type KernelEviction = {
	lane: string;
	reason: KernelEvictionReason;
	at: number;
	evicted: boolean;
	refused?: "running" | "pinned" | "snapshot-failed" | "absent";
	/** full: every name was saved; partial: `nonRestorable` names are lost; none: no snapshot. */
	restorable: "full" | "partial" | "none";
	nonRestorable: string[];
	snapshotPath?: string;
	error?: string;
};

export type KernelLease<K> = { kernel: K; release(): void };

/** Typed capacity result: every live kernel is running or pinned. Nothing is queued. */
export class KernelPoolCapacityError extends Error {
	readonly code = "kernel_pool_capacity";
	readonly live: number;
	readonly maxLive: number;
	constructor(live: number, maxLive: number) {
		super(`RLM kernel capacity exhausted: ${live}/${maxLive} kernels are running or pinned`);
		this.name = "KernelPoolCapacityError";
		this.live = live;
		this.maxLive = maxLive;
	}
}

type Entry<K> = {
	lane: string;
	kernel: K;
	running: number;
	lastUsed: number;
	evicting?: Promise<KernelEviction>;
};

const MAX_EVICTION_RECORDS = 256;

/**
 * Bounds live RLM kernels per worker. Idle kernels are evicted by TTL or to make room,
 * never while a cell runs (an unreleased lease) or while any holder pins the lane.
 * With snapshotPath configured, state is saved first and non-restorable names are
 * recorded; a failed snapshot keeps the kernel alive rather than losing its state.
 */
export class KernelPool<K extends PoolableKernel> {
	private readonly entries = new Map<string, Entry<K>>();
	private readonly pins = new Map<string, Set<string>>();
	private readonly records: KernelEviction[] = [];
	private admission: Promise<void> = Promise.resolve();
	private closed = false;
	private readonly options: KernelPoolOptions<K>;
	private readonly now: () => number;

	constructor(options: KernelPoolOptions<K>) {
		if (!Number.isSafeInteger(options.maxLive) || options.maxLive < 1)
			throw new TypeError("maxLive must be a positive integer");
		if (!Number.isFinite(options.idleTtlMs) || options.idleTtlMs < 0)
			throw new TypeError("idleTtlMs must be a nonnegative number");
		const maxPinned = options.maxPinned ?? options.maxLive;
		if (!Number.isSafeInteger(maxPinned) || maxPinned < 0 || maxPinned > options.maxLive)
			throw new TypeError("maxPinned must be an integer between 0 and maxLive");
		this.options = { ...options, maxPinned };
		this.now = options.now ?? Date.now;
	}

	/** Lease a lane's kernel, creating it if needed. The lease marks it running until release(). */
	acquire(lane: string): Promise<KernelLease<K>> {
		const pending = this.admission.then(() => this.admit(lane));
		this.admission = pending.then(
			() => {},
			() => {},
		);
		return pending;
	}

	/** Run work on a lane's kernel, releasing it afterwards. */
	async use<T>(lane: string, work: (kernel: K) => Promise<T>): Promise<T> {
		const lease = await this.acquire(lane);
		try {
			return await work(lease.kernel);
		} finally {
			lease.release();
		}
	}

	private async admit(lane: string): Promise<KernelLease<K>> {
		if (this.closed) throw new Error("RLM kernel pool is closed");
		let entry = this.entries.get(lane);
		if (entry?.evicting) {
			await entry.evicting;
			entry = this.entries.get(lane);
		}
		if (!entry) {
			while (this.entries.size >= this.options.maxLive) {
				const victim = this.victims()[0];
				if (!victim) throw new KernelPoolCapacityError(this.entries.size, this.options.maxLive);
				const record = await this.evict(victim.lane, "capacity");
				if (!record.evicted) throw new KernelPoolCapacityError(this.entries.size, this.options.maxLive);
			}
			if (this.closed) throw new Error("RLM kernel pool is closed");
			entry = { lane, kernel: this.options.create(lane), running: 0, lastUsed: this.now() };
			this.entries.set(lane, entry);
		}
		const leased = entry;
		leased.running += 1;
		leased.lastUsed = this.now();
		let released = false;
		return {
			kernel: leased.kernel,
			release: () => {
				if (released) return;
				released = true;
				leased.running -= 1;
				leased.lastUsed = this.now();
			},
		};
	}

	/** Idle, unpinned, not-evicting kernels, least recently used first. */
	private victims(): Entry<K>[] {
		return [...this.entries.values()]
			.filter((entry) => entry.running === 0 && !entry.evicting && !this.isPinned(entry.lane))
			.sort((left, right) => left.lastUsed - right.lastUsed);
	}

	private isPinned(lane: string): boolean {
		return (this.pins.get(lane)?.size ?? 0) > 0;
	}

	/** Retain a lane (checkpoint, retained agent instance, evidence) against eviction. */
	/** The lane's live kernel without leasing it, or undefined when the lane has none (never started or evicted). */
	live(lane: string): K | undefined {
		const entry = this.entries.get(lane);
		return entry && !entry.evicting ? entry.kernel : undefined;
	}

	pin(lane: string, holder: string): void {
		if (!holder) throw new TypeError("pin holder must be nonempty");
		const holders = this.pins.get(lane) ?? new Set<string>();
		if (holders.size === 0 && this.pinnedLanes().length >= this.options.maxPinned!) {
			throw new KernelPoolCapacityError(this.pinnedLanes().length, this.options.maxPinned!);
		}
		holders.add(holder);
		this.pins.set(lane, holders);
	}

	unpin(lane: string, holder: string): void {
		const holders = this.pins.get(lane);
		holders?.delete(holder);
		if (holders?.size === 0) this.pins.delete(lane);
	}

	private pinnedLanes(): string[] {
		return [...this.pins.keys()].filter((lane) => this.isPinned(lane));
	}

	/** Evict one lane. Refuses (and records the refusal) when running, pinned or unsaveable. */
	evict(lane: string, reason: KernelEvictionReason = "explicit"): Promise<KernelEviction> {
		const entry = this.entries.get(lane);
		if (entry?.evicting) return entry.evicting;
		const refuse = (refused: KernelEviction["refused"]): Promise<KernelEviction> =>
			Promise.resolve(
				this.record({
					lane,
					reason,
					at: this.now(),
					evicted: false,
					refused,
					restorable: "none",
					nonRestorable: [],
				}),
			);
		if (!entry) return refuse("absent");
		if (reason !== "close") {
			if (entry.running > 0) return refuse("running");
			if (this.isPinned(lane)) return refuse("pinned");
		}
		entry.evicting = (async () => {
			// Only close reaches a running kernel; its snapshot would wait behind the cell.
			const busy = entry.running > 0;
			const path = busy ? undefined : this.options.snapshotPath?.(lane);
			let result: KernelEviction = {
				lane,
				reason,
				at: this.now(),
				evicted: true,
				restorable: "none",
				nonRestorable: [],
				...(busy ? { error: "cell running at close; not snapshotted" } : {}),
			};
			if (path) {
				let snapshot: KernelExecutionResult | undefined;
				let error: string | undefined;
				try {
					snapshot = await entry.kernel.snapshot(path);
					if (snapshot.status === "error") error = snapshot.error?.evalue ?? "snapshot failed";
				} catch (caught) {
					error = caught instanceof Error ? caught.message : String(caught);
				}
				if (error !== undefined || !snapshot?.snapshot) {
					if (reason !== "close") {
						entry.evicting = undefined;
						return this.record({
							...result,
							evicted: false,
							refused: "snapshot-failed",
							snapshotPath: path,
							error: error ?? "snapshot returned no report",
						});
					}
					result = { ...result, snapshotPath: path, error: error ?? "snapshot returned no report" };
				} else {
					const skipped = snapshot.snapshot.skipped;
					result = {
						...result,
						snapshotPath: path,
						restorable: skipped.length === 0 ? "full" : "partial",
						nonRestorable: skipped,
					};
				}
			}
			this.entries.delete(lane);
			await entry.kernel.shutdown();
			return this.record(result);
		})();
		return entry.evicting;
	}

	/** Evict every idle, unpinned kernel whose idle time reached idleTtlMs. */
	async sweep(): Promise<KernelEviction[]> {
		const now = this.now();
		const expired = this.victims().filter((entry) => now - entry.lastUsed >= this.options.idleTtlMs);
		const results: KernelEviction[] = [];
		for (const entry of expired) results.push(await this.evict(entry.lane, "idle"));
		return results;
	}

	stats(): {
		live: number;
		maxLive: number;
		lanes: { lane: string; running: number; pinnedBy: string[]; idleMs: number }[];
	} {
		const now = this.now();
		return {
			live: this.entries.size,
			maxLive: this.options.maxLive,
			lanes: [...this.entries.values()].map((entry) => ({
				lane: entry.lane,
				running: entry.running,
				pinnedBy: [...(this.pins.get(entry.lane) ?? [])],
				idleMs: entry.running > 0 ? 0 : now - entry.lastUsed,
			})),
		};
	}

	/** Eviction history, including refusals, oldest first (bounded). */
	get evictions(): readonly KernelEviction[] {
		return this.records;
	}

	private record(record: KernelEviction): KernelEviction {
		this.records.push(record);
		if (this.records.length > MAX_EVICTION_RECORDS) this.records.shift();
		return record;
	}

	/** Shut down every kernel, snapshotting first when configured. Pins do not block close. */
	async close(): Promise<void> {
		this.closed = true;
		await this.admission;
		await Promise.all([...this.entries.keys()].map((lane) => this.evict(lane, "close")));
	}
}
