import { type Context, defineService } from "@ultron/chord";
import type { RpcExtensionUIRequest } from "../../modes/rpc/rpc-types.ts";

/** One extension UI request, in Pi's RPC wire format, with its position in the worker's request stream. */
export interface ExtensionUIRequestItem {
	seq: number;
	request: RpcExtensionUIRequest;
}

export interface ExtensionUIPollResult {
	/** Pass back as `after` on the next poll. */
	cursor: number;
	requests: ExtensionUIRequestItem[];
}

/** Pi's `extension_ui_response` without its `type`/`id`. */
export type ExtensionUIAnswer = { value: string } | { confirmed: boolean } | { cancelled: true };

/**
 * Bridge from the worker's extension UI (`ctx.ui.select/confirm/input/editor/notify/...`) to an attached presentation.
 *
 * A presentation serves the bridge by polling: while one polls, extension dialogs wait for its answer as they would in
 * Pi's RPC mode (honouring the extension's own `timeout` and `signal`). With nobody polling, dialogs resolve at once
 * with Pi's defaults (`undefined` / `false`) and fire-and-forget requests are dropped, as in Pi's print mode.
 */
export interface ExtensionUI {
	/**
	 * Wait up to `waitMs` for requests after `after`. `after: null` starts serving now: it returns only the dialogs
	 * still waiting for an answer.
	 */
	poll(after: number | null, waitMs: number, context: Context): Promise<ExtensionUIPollResult>;
	/** Answer a dialog. Unknown or already-settled ids are ignored, as in Pi. */
	respond(id: string, answer: ExtensionUIAnswer, context: Context): Promise<void>;
}

export const ExtensionUI = defineService<ExtensionUI>("ultron.extension-ui");
