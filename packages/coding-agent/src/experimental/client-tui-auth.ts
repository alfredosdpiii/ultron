/**
 * Pi's `/login` and `/logout` in the native TUI. The login flow runs in the client process, which owns the
 * terminal: it opens the browser, prints device codes and prompts for keys through Pi's login dialog. Credentials
 * go through Pi's `AuthStorage` to `<agentDir>/auth.json`; the Session worker is then told to reload them
 * (`SessionControl.reloadAuth`) so its next request uses them without a restart.
 */

import type { AuthEvent, AuthPrompt } from "@ultron/ai";
import { BACKGROUND_CONTEXT, withAbortSignal } from "@ultron/chord/context";
import { type Component, Container, type Focusable, isFocusable } from "@ultron/tui";
import type { ModelRuntime } from "../core/model-runtime.ts";
import { ExtensionSelectorComponent } from "../modes/interactive/components/extension-selector.ts";
import { LoginDialogComponent } from "../modes/interactive/components/login-dialog.ts";
import { type AuthSelectorProvider, OAuthSelectorComponent } from "../modes/interactive/components/oauth-selector.ts";
import type { PiCommandHost } from "./client-tui-pi-commands.ts";

/** What the login flow needs from Pi's `ModelRuntime` (the client's own, over the profile's `auth.json`). */
export type LoginRuntime = Pick<
	ModelRuntime,
	"getProviders" | "getProvider" | "getProviderAuthStatus" | "isUsingOAuth" | "login" | "logout" | "listCredentials"
>;

const CANCELLED = "Login cancelled";

/** An editor-slot component whose content (the login dialog, or a select prompt over it) can be swapped. */
class Slot extends Container implements Focusable {
	#current: Component;
	#focused = false;

	constructor(initial: Component) {
		super();
		this.#current = initial;
		this.addChild(initial);
	}

	get focused(): boolean {
		return this.#focused;
	}

	set focused(value: boolean) {
		this.#focused = value;
		if (isFocusable(this.#current)) this.#current.focused = value;
	}

	show(component: Component): void {
		if (isFocusable(this.#current)) this.#current.focused = false;
		this.#current = component;
		this.clear();
		this.addChild(component);
		if (isFocusable(component)) component.focused = this.#focused;
	}

	handleInput(data: string): void {
		this.#current.handleInput?.(data);
	}
}

export function loginProviderOptions(runtime: LoginRuntime, authType?: "oauth" | "api_key"): AuthSelectorProvider[] {
	const options: AuthSelectorProvider[] = [];
	for (const provider of runtime.getProviders()) {
		const authStatus = runtime.getProviderAuthStatus(provider.id);
		const status = authStatus.configured
			? {
					type: runtime.isUsingOAuth(provider.id) ? ("oauth" as const) : ("api_key" as const),
					source: authStatus.label ?? authStatus.source,
				}
			: undefined;
		if ((!authType || authType === "oauth") && provider.auth.oauth) {
			options.push({ id: provider.id, name: provider.name, authType: "oauth", method: provider.auth.oauth, status });
		}
		if ((!authType || authType === "api_key") && provider.auth.apiKey) {
			options.push({
				id: provider.id,
				name: provider.name,
				authType: "api_key",
				method: provider.auth.apiKey,
				status,
			});
		}
	}
	return options.sort((left, right) => left.name.localeCompare(right.name));
}

async function runtimeOrError(host: PiCommandHost): Promise<LoginRuntime | undefined> {
	try {
		return await host.loginRuntime();
	} catch (error) {
		host.showStatus(`Error: Could not load providers: ${error instanceof Error ? error.message : String(error)}`);
		return undefined;
	}
}

/** Pi's `/login [provider]`. */
export async function handleLogin(host: PiCommandHost, providerRef: string): Promise<void> {
	const runtime = await runtimeOrError(host);
	if (runtime === undefined) return;
	const ref = providerRef.trim().toLowerCase();
	if (!ref) {
		await showAuthTypeSelector(host, runtime);
		return;
	}
	const matches = loginProviderOptions(runtime).filter(
		(provider) => provider.id.toLowerCase() === ref || provider.name.toLowerCase() === ref,
	);
	if (matches.length === 1) {
		await startProviderLogin(host, runtime, matches[0]!);
		return;
	}
	if (matches.length > 1 && new Set(matches.map((provider) => provider.id)).size === 1) {
		await showAuthTypeSelector(host, runtime, matches);
		return;
	}
	showProviderSelector(host, runtime, undefined, providerRef.trim());
}

async function showAuthTypeSelector(
	host: PiCommandHost,
	runtime: LoginRuntime,
	providerOptions?: AuthSelectorProvider[],
): Promise<void> {
	const oauthProvider = providerOptions?.find((provider) => provider.authType === "oauth");
	const oauthLoginLabel =
		oauthProvider?.method && "loginLabel" in oauthProvider.method ? oauthProvider.method.loginLabel : undefined;
	const subscriptionLabel = oauthLoginLabel ?? "Sign in with an account";
	const apiKeyLabel = "Sign in with an API key";
	const available = providerOptions
		? new Set(providerOptions.map((provider) => provider.authType))
		: new Set<AuthSelectorProvider["authType"]>(["oauth", "api_key"]);
	const labels = [
		...(available.has("oauth") ? [subscriptionLabel] : []),
		...(available.has("api_key") ? [apiKeyLabel] : []),
	];
	if (labels.length === 0) {
		host.showStatus("No login methods available.");
		return;
	}
	if (providerOptions && labels.length === 1) {
		await startProviderLogin(host, runtime, providerOptions[0]!);
		return;
	}
	const title = providerOptions?.[0]
		? `Select authentication method for ${providerOptions[0].name}:`
		: "Select authentication method:";
	const choice = await host.select(title, labels);
	if (choice === undefined) return;
	const authType = choice === subscriptionLabel ? "oauth" : "api_key";
	if (providerOptions) {
		const option = providerOptions.find((provider) => provider.authType === authType);
		if (option) await startProviderLogin(host, runtime, option);
		return;
	}
	showProviderSelector(host, runtime, authType);
}

function showProviderSelector(
	host: PiCommandHost,
	runtime: LoginRuntime,
	authType?: AuthSelectorProvider["authType"],
	initialSearch?: string,
): void {
	const options = loginProviderOptions(runtime, authType);
	if (options.length === 0) {
		host.showStatus(
			authType === "oauth"
				? "No subscription providers available."
				: authType === "api_key"
					? "No API key providers available."
					: "No login providers available.",
		);
		return;
	}
	let close = (): void => {};
	const selector = new OAuthSelectorComponent(
		"login",
		options,
		(providerId, selectedAuthType) => {
			close();
			const option = options.find(
				(provider) => provider.id === providerId && provider.authType === selectedAuthType,
			);
			if (option) void startProviderLogin(host, runtime, option);
		},
		() => {
			close();
			if (authType) void showAuthTypeSelector(host, runtime);
			else host.requestRender();
		},
		initialSearch,
	);
	close = host.showComponent(selector, selector);
}

async function startProviderLogin(
	host: PiCommandHost,
	runtime: LoginRuntime,
	option: AuthSelectorProvider,
): Promise<void> {
	if (option.authType === "api_key" && !option.method?.login) {
		// Ambient-only auth (env vars, cloud profiles): Pi explains where it is configured.
		let close = (): void => {};
		const dialog = new LoginDialogComponent(
			host.ui,
			option.id,
			() => {
				close();
				host.requestRender();
			},
			option.name,
			`${option.name} setup`,
		);
		dialog.showInfo(`${option.method?.name ?? "Authentication"} is configured outside Ultron.`, [], true);
		close = host.showComponent(dialog, dialog);
		return;
	}
	await runLoginDialog(host, runtime, option);
}

/** What a login flow needs from its screen: the TUI and a slot to show the dialog in. */
export interface LoginDialogHost {
	readonly ui: PiCommandHost["ui"];
	showComponent: PiCommandHost["showComponent"];
	requestRender(): void;
}

export type LoginOutcome =
	| { readonly ok: true }
	| {
			readonly ok: false;
			readonly cancelled: boolean;
			readonly error: string;
			/** Cancelled by the screen (another dialog took the login dialog's place), not by the user. */
			readonly interrupted?: boolean;
	  };

/**
 * Pi's login dialog around `ModelRuntime.login`: browser/device-code flows for subscriptions, a masked prompt for
 * API keys. Credentials are saved by the runtime (to its `auth.json`); resolves once the dialog has closed.
 */
export async function runProviderLogin(
	host: LoginDialogHost,
	runtime: LoginRuntime,
	option: AuthSelectorProvider,
): Promise<LoginOutcome> {
	const dialog = new LoginDialogComponent(host.ui, option.id, () => {}, option.name);
	const slot = new Slot(dialog);
	let closed = false;
	let interrupted = false;
	const close = host.showComponent(slot, slot, {
		// Replaced by another dialog: stop the login, as Esc does.
		cancel: () => {
			interrupted = !dialog.signal.aborted;
			dialog.handleInput("\u001b");
		},
	});
	const finish = (): void => {
		if (closed) return;
		closed = true;
		close();
	};
	const prompt = (request: AuthPrompt): Promise<string> => {
		let response: Promise<string>;
		if (request.type === "select") {
			response = new Promise((resolve, reject) => {
				const selector = new ExtensionSelectorComponent(
					request.message,
					request.options.map((choice) => choice.label),
					(label) => {
						slot.show(dialog);
						const id = request.options.find((choice) => choice.label === label)?.id;
						if (id) resolve(id);
						else reject(new Error(CANCELLED));
						host.requestRender();
					},
					() => {
						slot.show(dialog);
						reject(new Error(CANCELLED));
						host.requestRender();
					},
				);
				slot.show(selector);
				host.requestRender();
			});
		} else if (request.type === "manual_code") {
			response = dialog.showManualInput(request.message);
		} else {
			response = dialog.showPrompt(request.message, request.placeholder, { secret: request.type === "secret" });
		}
		const signal = request.signal;
		if (!signal) return response;
		if (signal.aborted) return Promise.reject(new Error(CANCELLED));
		return Promise.race([
			response,
			new Promise<string>((_resolve, reject) =>
				signal.addEventListener("abort", () => reject(new Error(CANCELLED)), { once: true }),
			),
		]);
	};
	const notify = (event: AuthEvent): void => {
		if (event.type === "auth_url") dialog.showAuth(event.url, event.instructions);
		else if (event.type === "device_code") {
			dialog.showDeviceCode(event);
			dialog.showWaiting("Waiting for authentication...");
		} else if (event.type === "info") dialog.showInfo(event.message, event.links);
		else dialog.showProgress(event.message);
		host.requestRender();
	};
	try {
		await runtime.login(option.id, option.authType, { signal: dialog.signal, prompt, notify });
	} catch (error) {
		finish();
		const message = error instanceof Error ? error.message : String(error);
		const cancelled = message === CANCELLED || dialog.signal.aborted;
		return { ok: false, cancelled, error: message, ...(cancelled && interrupted ? { interrupted } : {}) };
	}
	finish();
	return { ok: true };
}

/** Pi's login dialog around `ModelRuntime.login`, then the worker reloads the saved credentials. */
async function runLoginDialog(host: PiCommandHost, runtime: LoginRuntime, option: AuthSelectorProvider): Promise<void> {
	const outcome = await runProviderLogin(host, runtime, option);
	if (!outcome.ok) {
		if (outcome.interrupted) {
			// Esc closes silently; a login ended by something else must not look like nothing happened.
			host.showStatus(`Login to ${option.name} stopped: another dialog opened. Run /login again.`);
		} else if (!outcome.cancelled) {
			host.showStatus(
				option.authType === "oauth"
					? `Error: Failed to login to ${option.name}: ${outcome.error}`
					: `Error: Failed to save API key for ${option.name}: ${outcome.error}`,
			);
		}
		return;
	}
	const action = option.authType === "oauth" ? `Logged in to ${option.name}` : `Saved API key for ${option.name}`;
	host.showStatus(`${action}. Credentials saved to ${host.authPath}. Reloading the Session's credentials…`);
	await reloadWorkerAuth(host, option.id, `${action}. Credentials saved to ${host.authPath}`, () =>
		modelHint(host, option),
	);
}

/**
 * Logging in does not change the Session's model. Say so when it is another provider's, so a login made to get
 * away from a model that does not work is not mistaken for one that failed.
 */
function modelHint(host: PiCommandHost, option: AuthSelectorProvider): string {
	const current = host.currentModel();
	if (current === undefined) return `. Use /model to select one of ${option.name}'s models`;
	if (current.provider === option.id) return "";
	return `. The Session still uses ${current.provider}/${current.modelId}; /model switches to ${option.name}'s models`;
}

/** Tell the worker to re-read `auth.json`, then refresh the replicated model catalog (as Pi does after login). */
async function reloadWorkerAuth(
	host: PiCommandHost,
	providerId: string,
	done: string,
	hint: () => string = () => "",
): Promise<void> {
	const control = host.control();
	if (control === undefined) {
		host.showStatus(`${done}. No Session is attached to reload them.`);
		return;
	}
	try {
		await control.reloadAuth(providerId, BACKGROUND_CONTEXT);
	} catch (error) {
		host.showStatus(
			`${done}, but the Session could not reload credentials: ${error instanceof Error ? error.message : String(error)}. Restart the Session to use them.`,
		);
		return;
	}
	const models = host.models();
	if (models !== undefined) {
		const abort = new AbortController();
		const timeout = setTimeout(() => abort.abort(), 15_000);
		try {
			await models.refresh(withAbortSignal(abort.signal, BACKGROUND_CONTEXT));
		} catch {
			host.showStatus(`${done}, but its model catalog could not be refreshed; using cached models.`);
			return;
		} finally {
			clearTimeout(timeout);
		}
	}
	host.showStatus(`${done}${hint()}`);
}

/** Pi's `/logout`: remove a stored credential, then the worker reloads. */
export async function handleLogout(host: PiCommandHost): Promise<void> {
	const runtime = await runtimeOrError(host);
	if (runtime === undefined) return;
	let options: AuthSelectorProvider[];
	try {
		options = (await runtime.listCredentials({ signal: AbortSignal.timeout(15_000) }))
			.map(({ providerId, type }) => ({
				id: providerId,
				name: runtime.getProvider(providerId)?.name ?? providerId,
				authType: type,
				status: { type, source: "stored credential" },
			}))
			.sort((left, right) => left.name.localeCompare(right.name));
	} catch (error) {
		host.showStatus(
			`Error: Could not read stored credentials: ${error instanceof Error ? error.message : String(error)}`,
		);
		return;
	}
	if (options.length === 0) {
		host.showStatus(
			"No stored credentials to remove. /logout only removes credentials saved by /login; environment variables and models.json config are unchanged.",
		);
		return;
	}
	let close = (): void => {};
	const selector = new OAuthSelectorComponent(
		"logout",
		options,
		(providerId) => {
			close();
			const option = options.find((provider) => provider.id === providerId);
			if (option === undefined) return;
			void (async () => {
				try {
					await runtime.logout(option.id, { signal: AbortSignal.timeout(15_000) });
				} catch (error) {
					host.showStatus(`Error: Logout failed: ${error instanceof Error ? error.message : String(error)}`);
					return;
				}
				await reloadWorkerAuth(
					host,
					option.id,
					option.authType === "oauth"
						? `Logged out of ${option.name}`
						: `Removed stored API key for ${option.name}. Environment variables and models.json config are unchanged`,
				);
			})();
		},
		() => {
			close();
			host.requestRender();
		},
	);
	close = host.showComponent(selector, selector);
}
