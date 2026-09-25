import { randomUUID } from "node:crypto";
import type { ExtensionUIContext, ExtensionUIDialogOptions } from "../../core/extensions/types.ts";
import type { RpcExtensionUIRequest } from "../../modes/rpc/rpc-types.ts";
import type { ExtensionUIAnswer, ExtensionUIRequestItem, ExtensionUI as ExtensionUIService } from "./extension-ui.ts";

/** How long after its last poll a presentation still counts as serving extension UI. */
const SERVING_GRACE_MS = 2_000;
/** Longest single poll; clients re-poll, so this only bounds how long one request stays open. */
const MAX_POLL_MS = 30_000;
/** Fire-and-forget requests kept for a client that is between polls. */
const MAX_BUFFERED_EVENTS = 256;
/** How long a worker started for an interactive client waits for its first poll before running headless. */
const EXPECTED_CLIENT_WAIT_MS = 30_000;

type DistributiveOmit<T, K extends PropertyKey> = T extends unknown ? Omit<T, K> : never;
type RequestBody = DistributiveOmit<RpcExtensionUIRequest, "type" | "id">;

interface PendingDialog {
	readonly item: ExtensionUIRequestItem;
	settle(answer: ExtensionUIAnswer | undefined): void;
}

/**
 * Worker side of the extension UI bridge: an `ExtensionUIContext` whose dialogs and notifications reach whichever
 * presentation polls `ExtensionUI`, with Pi's RPC semantics.
 */
export class ExtensionUIBridge {
	#seq = 0;
	#events: ExtensionUIRequestItem[] = [];
	readonly #dialogs = new Map<string, PendingDialog>();
	readonly #waiters = new Set<() => void>();
	#polls = 0;
	#lastPollEnd = Number.NEGATIVE_INFINITY;
	#graceTimer: ReturnType<typeof setTimeout> | undefined;
	readonly #now: () => number;
	#polled = false;
	/** Until then, a worker started for an interactive client counts as served before that client's first poll. */
	readonly #expectedUntil: number;

	/**
	 * `expectClient`: the worker was started for an interactive client (the TUI or RPC) that has not polled yet.
	 * Until it does (or `expectedClientWaitMs` passes), the UI counts as served: requests queue for it, and its first
	 * poll receives them.
	 */
	constructor(options: { now?: () => number; expectClient?: boolean; expectedClientWaitMs?: number } = {}) {
		this.#now = options.now ?? Date.now;
		this.#expectedUntil =
			options.expectClient === true
				? this.#now() + (options.expectedClientWaitMs ?? EXPECTED_CLIENT_WAIT_MS)
				: Number.NEGATIVE_INFINITY;
		if (options.expectClient === true) this.#scheduleServingCheck(this.#expectedUntil - this.#now());
	}

	/** True while a presentation polls, polled within the grace period, or an expected client has not arrived yet. */
	get serving(): boolean {
		return (
			this.#polls > 0 ||
			this.#now() - this.#lastPollEnd < SERVING_GRACE_MS ||
			(!this.#polled && this.#now() < this.#expectedUntil)
		);
	}

	readonly service: ExtensionUIService = {
		poll: async (requestedAfter, waitMs, context) => {
			// A cursor from before this worker started (a reattached client) restarts from the beginning.
			let after = requestedAfter !== null && requestedAfter > this.#seq ? 0 : requestedAfter;
			// The first client of a worker started for it also receives what was queued before it arrived.
			if (after === null && !this.#polled && this.#expectedUntil !== Number.NEGATIVE_INFINITY) after = 0;
			this.#polled = true;
			this.#polls += 1;
			try {
				const collect = (): ExtensionUIRequestItem[] => {
					if (after === null) return [...this.#dialogs.values()].map((dialog) => dialog.item);
					const dialogs = [...this.#dialogs.values()].map((dialog) => dialog.item);
					return [...this.#events, ...dialogs]
						.filter((item) => item.seq > after)
						.sort((left, right) => left.seq - right.seq);
				};
				let requests = collect();
				if (requests.length === 0 && after !== null) {
					await this.#waitForRequest(Math.max(0, Math.min(waitMs, MAX_POLL_MS)), context);
					requests = collect();
				}
				return {
					cursor: after === null ? this.#seq : Math.max(after, ...requests.map((item) => item.seq)),
					requests,
				};
			} finally {
				this.#polls -= 1;
				this.#lastPollEnd = this.#now();
				this.#scheduleServingCheck();
			}
		},
		respond: async (id, answer) => {
			this.#dialogs.get(id)?.settle(answer);
		},
	};

	/** The extension UI context: dialogs and notifications go to the serving presentation, the rest to `fallback`. */
	createContext(fallback: ExtensionUIContext): ExtensionUIContext {
		const bridge = this;
		return {
			...fallback,
			select: (title, options, opts) =>
				this.#dialog(opts, undefined, { method: "select", title, options, timeout: opts?.timeout }, (answer) =>
					"value" in answer ? answer.value : undefined,
				),
			confirm: (title, message, opts) =>
				this.#dialog(opts, false, { method: "confirm", title, message, timeout: opts?.timeout }, (answer) =>
					"confirmed" in answer ? answer.confirmed : false,
				),
			input: (title, placeholder, opts) =>
				this.#dialog(opts, undefined, { method: "input", title, placeholder, timeout: opts?.timeout }, (answer) =>
					"value" in answer ? answer.value : undefined,
				),
			// Pi's RPC editor dialog has no timeout or signal.
			editor: (title, prefill) =>
				this.#dialog(undefined, undefined, { method: "editor", title, prefill }, (answer) =>
					"value" in answer ? answer.value : undefined,
				),
			notify: (message, type) => this.#emit({ method: "notify", message, notifyType: type }),
			setStatus: (key, text) => this.#emit({ method: "setStatus", statusKey: key, statusText: text }),
			setWidget: (key: string, content: unknown, options?: { placement?: "aboveEditor" | "belowEditor" }) => {
				// As in Pi's RPC mode, only string arrays cross the wire; component factories stay local.
				if (content !== undefined && !Array.isArray(content)) return;
				this.#emit({
					method: "setWidget",
					widgetKey: key,
					widgetLines: content as string[] | undefined,
					widgetPlacement: options?.placement,
				});
			},
			setTitle: (title) => this.#emit({ method: "setTitle", title }),
			setEditorText: (text) => this.#emit({ method: "set_editor_text", text }),
			pasteToEditor(text) {
				bridge.#emit({ method: "set_editor_text", text });
			},
		} as ExtensionUIContext;
	}

	/** Settle every open dialog with its default (the worker is closing). */
	close(): void {
		if (this.#graceTimer !== undefined) clearTimeout(this.#graceTimer);
		this.#graceTimer = undefined;
		for (const dialog of [...this.#dialogs.values()]) dialog.settle(undefined);
		this.#wake();
	}

	#dialog<T>(
		opts: ExtensionUIDialogOptions | undefined,
		defaultValue: T,
		body: RequestBody,
		parse: (answer: Exclude<ExtensionUIAnswer, { cancelled: true }>) => T,
	): Promise<T> {
		if (opts?.signal?.aborted || !this.serving) return Promise.resolve(defaultValue);
		const id = randomUUID();
		return new Promise<T>((resolve) => {
			let timeout: ReturnType<typeof setTimeout> | undefined;
			const settle = (answer: ExtensionUIAnswer | undefined): void => {
				if (!this.#dialogs.has(id)) return;
				this.#dialogs.delete(id);
				if (timeout !== undefined) clearTimeout(timeout);
				opts?.signal?.removeEventListener("abort", onAbort);
				resolve(answer === undefined || "cancelled" in answer ? defaultValue : parse(answer));
			};
			const onAbort = (): void => settle(undefined);
			opts?.signal?.addEventListener("abort", onAbort, { once: true });
			if (opts?.timeout) timeout = setTimeout(() => settle(undefined), opts.timeout);
			const item = this.#item({ id, ...body });
			this.#dialogs.set(id, { item, settle });
			this.#wake();
		});
	}

	#emit(body: RequestBody): void {
		if (!this.serving) return;
		this.#events.push(this.#item({ id: randomUUID(), ...body }));
		if (this.#events.length > MAX_BUFFERED_EVENTS) this.#events = this.#events.slice(-MAX_BUFFERED_EVENTS);
		this.#wake();
	}

	#item(request: { id: string } & RequestBody): ExtensionUIRequestItem {
		this.#seq += 1;
		// Drop undefined fields so the request is plain JSON, as Pi's JSONL output would be.
		const wire = JSON.parse(JSON.stringify({ type: "extension_ui_request", ...request })) as RpcExtensionUIRequest;
		return { seq: this.#seq, request: wire };
	}

	#waitForRequest(waitMs: number, context: { readonly abortSignal?: AbortSignal }): Promise<void> {
		return new Promise((resolve) => {
			const signal = context.abortSignal;
			const done = (): void => {
				clearTimeout(timer);
				this.#waiters.delete(done);
				signal?.removeEventListener("abort", done);
				resolve();
			};
			const timer = setTimeout(done, waitMs);
			this.#waiters.add(done);
			signal?.addEventListener("abort", done, { once: true });
			if (signal?.aborted) done();
		});
	}

	#wake(): void {
		for (const waiter of [...this.#waiters]) waiter();
	}

	/** Once nobody serves, open dialogs get Pi's defaults instead of waiting for an answer that cannot come. */
	#scheduleServingCheck(delayMs = SERVING_GRACE_MS): void {
		if (this.#graceTimer !== undefined) clearTimeout(this.#graceTimer);
		this.#graceTimer = setTimeout(() => {
			this.#graceTimer = undefined;
			if (this.serving) return;
			this.#events = [];
			for (const dialog of [...this.#dialogs.values()]) dialog.settle(undefined);
		}, delayMs + 50);
		this.#graceTimer.unref?.();
	}
}
