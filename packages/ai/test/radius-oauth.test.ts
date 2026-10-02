import { get as httpGet } from "node:http";
import { afterEach, describe, expect, it, vi } from "vitest";
import { createRadiusOAuth } from "../src/auth/oauth/radius.ts";
import type { AuthEvent, ProviderAuthInteraction } from "../src/auth/types.ts";

const GATEWAY = "https://radius.example";

function jsonResponse(body: unknown, status = 200): Response {
	return new Response(JSON.stringify(body), {
		status,
		headers: { "content-type": "application/json" },
	});
}

function requestUrl(input: unknown): string {
	if (typeof input === "string") return input;
	if (input instanceof URL) return input.toString();
	if (input instanceof Request) return input.url;
	throw new Error(`Unsupported request input: ${String(input)}`);
}

function interaction(loginMethod: "browser" | "device-code", events: AuthEvent[] = []): ProviderAuthInteraction {
	return {
		signal: new AbortController().signal,
		prompt: async () => loginMethod,
		notify: (event) => events.push(event),
	};
}

describe("Radius OAuth", () => {
	afterEach(() => {
		vi.restoreAllMocks();
		vi.unstubAllGlobals();
		vi.useRealTimers();
	});

	it("uses gateway endpoints directly for device login", async () => {
		vi.useFakeTimers();
		vi.setSystemTime(new Date("2026-07-24T00:00:00Z"));
		const events: AuthEvent[] = [];
		const urls: string[] = [];
		vi.stubGlobal(
			"fetch",
			vi.fn(async (input: unknown, init?: RequestInit) => {
				const url = requestUrl(input);
				urls.push(url);
				const form = new URLSearchParams(String(init?.body));
				if (url === `${GATEWAY}/v1/oauth/device`) {
					expect(form.get("client_id")).toBe("pi-gateway");
					expect(form.get("scope")).toBe("gateway offline_access");
					return jsonResponse({
						device_code: "device-code",
						user_code: "ABCD-1234",
						verification_uri: "https://radius-ui.example/pair",
						expires_in: 600,
						interval: 5,
					});
				}
				if (url === `${GATEWAY}/v1/oauth/token`) {
					expect(form.get("grant_type")).toBe("urn:ietf:params:oauth:grant-type:device_code");
					expect(form.get("client_id")).toBe("pi-gateway");
					expect(form.get("device_code")).toBe("device-code");
					return jsonResponse({
						access_token: "access-token",
						refresh_token: "refresh-token",
						expires_in: 3600,
						scope: "gateway offline_access",
					});
				}
				throw new Error(`Unexpected request: ${url}`);
			}),
		);

		const oauth = createRadiusOAuth({ name: "Radius", gateway: GATEWAY });
		await expect(oauth.login(interaction("device-code", events))).resolves.toEqual({
			type: "oauth",
			access: "access-token",
			refresh: "refresh-token",
			expires: Date.now() + 3600 * 1000 - 60_000,
			scope: "gateway offline_access",
		});
		expect(events).toEqual([
			{
				type: "device_code",
				userCode: "ABCD-1234",
				verificationUri: "https://radius-ui.example/pair",
				intervalSeconds: 5,
				expiresInSeconds: 600,
			},
		]);
		expect(urls).toEqual([`${GATEWAY}/v1/oauth/device`, `${GATEWAY}/v1/oauth/token`]);
	});

	it("refreshes directly through the gateway without discovery", async () => {
		const fetchMock = vi.fn(async (input: unknown, init?: RequestInit) => {
			expect(requestUrl(input)).toBe(`${GATEWAY}/v1/oauth/token`);
			const form = new URLSearchParams(String(init?.body));
			expect(form.get("grant_type")).toBe("refresh_token");
			expect(form.get("client_id")).toBe("pi-gateway");
			expect(form.get("refresh_token")).toBe("old-refresh");
			return jsonResponse({
				access_token: "new-access",
				refresh_token: "new-refresh",
				expires_in: 3600,
			});
		});
		vi.stubGlobal("fetch", fetchMock);

		const oauth = createRadiusOAuth({ name: "Radius", gateway: GATEWAY });
		await expect(
			oauth.refresh(
				{ type: "oauth", access: "old-access", refresh: "old-refresh", expires: 0 },
				new AbortController().signal,
			),
		).resolves.toMatchObject({ access: "new-access", refresh: "new-refresh" });
		expect(fetchMock).toHaveBeenCalledOnce();
	});

	it("discovers only the interactive browser authorization endpoint", async () => {
		const fetchMock = vi.fn(async (input: unknown) => {
			expect(requestUrl(input)).toBe(`${GATEWAY}/v1/oauth`);
			return jsonResponse({ issuer: "https://radius-ui.example" });
		});
		vi.stubGlobal("fetch", fetchMock);

		const oauth = createRadiusOAuth({ name: "Radius", gateway: GATEWAY });
		await expect(oauth.login(interaction("browser"))).rejects.toThrow(`Invalid Radius OAuth config from ${GATEWAY}`);
		expect(fetchMock).toHaveBeenCalledOnce();
	});

	function stubBrowserLoginGateway(): { tokenForms: URLSearchParams[] } {
		const tokenForms: URLSearchParams[] = [];
		vi.stubGlobal(
			"fetch",
			vi.fn(async (input: unknown, init?: RequestInit) => {
				const url = requestUrl(input);
				if (url === `${GATEWAY}/v1/oauth`)
					return jsonResponse({ authorizationEndpoint: "https://radius-ui.example/authorize" });
				if (url === `${GATEWAY}/v1/oauth/token`) {
					tokenForms.push(new URLSearchParams(String(init?.body)));
					return jsonResponse({ access_token: "access", refresh_token: "refresh", expires_in: 3600 });
				}
				throw new Error(`Unexpected request: ${url}`);
			}),
		);
		return { tokenForms };
	}

	/** A browser login whose paste prompt answers with `pasted(state)`; `undefined` leaves the prompt open. */
	function browserInteraction(
		events: AuthEvent[],
		pasted: (state: string) => string | undefined,
		signal: AbortSignal = new AbortController().signal,
	): ProviderAuthInteraction {
		return {
			signal,
			notify: (event) => events.push(event),
			prompt: async (prompt) => {
				if (prompt.type === "select") return "browser";
				if (prompt.type !== "manual_code") throw new Error(`Unexpected prompt: ${prompt.type}`);
				const authUrl = events.find((event) => event.type === "auth_url");
				if (authUrl?.type !== "auth_url") throw new Error("The sign-in URL was not shown before the prompt");
				const answer = pasted(new URL(authUrl.url).searchParams.get("state") ?? "");
				if (answer !== undefined) return answer;
				return new Promise<string>((_resolve, reject) => {
					prompt.signal?.addEventListener("abort", () => reject(new Error("Login cancelled")), { once: true });
				});
			},
		};
	}

	// The browser login had no way to finish when the browser cannot reach 127.0.0.1:1456 (a container, SSH).
	it.each([
		[
			"the full redirect URL",
			(state: string) => `http://127.0.0.1:1456/oauth/callback?code=pasted-code&state=${state}`,
		],
		[
			"the address without its scheme",
			(state: string) => `127.0.0.1:1456/oauth/callback?code=pasted-code&state=${state}`,
		],
		["the bare code", () => "pasted-code"],
	])("completes browser login from %s pasted into the prompt", async (_name, pasted) => {
		const { tokenForms } = stubBrowserLoginGateway();
		const events: AuthEvent[] = [];
		const oauth = createRadiusOAuth({ name: "Radius", gateway: GATEWAY });
		const credential = await oauth.login(browserInteraction(events, pasted));

		expect(credential).toMatchObject({ type: "oauth", access: "access", refresh: "refresh" });
		expect(tokenForms).toHaveLength(1);
		expect(tokenForms[0]?.get("grant_type")).toBe("authorization_code");
		expect(tokenForms[0]?.get("code")).toBe("pasted-code");
		expect(tokenForms[0]?.get("redirect_uri")).toBe("http://127.0.0.1:1456/oauth/callback");
	});

	it("still completes browser login through the callback server, and closes the paste prompt", async () => {
		const { tokenForms } = stubBrowserLoginGateway();
		const events: AuthEvent[] = [];
		let promptSignal: AbortSignal | undefined;
		const base = browserInteraction(events, () => undefined);
		const oauth = createRadiusOAuth({ name: "Radius", gateway: GATEWAY });
		const login = oauth.login({
			...base,
			prompt: (prompt) => {
				if (prompt.type === "manual_code") promptSignal = prompt.signal;
				return base.prompt(prompt);
			},
		});
		const authUrl = await vi.waitFor(() => {
			const event = events.find((candidate) => candidate.type === "auth_url");
			if (event?.type !== "auth_url") throw new Error("waiting for the sign-in URL");
			return new URL(event.url);
		});
		const status = await new Promise<number>((resolve, reject) => {
			httpGet(
				`http://127.0.0.1:1456/oauth/callback?code=browser-code&state=${authUrl.searchParams.get("state")}`,
				(response) => {
					response.resume();
					response.on("end", () => resolve(response.statusCode ?? 0));
				},
			).on("error", reject);
		});
		expect(status).toBe(200);
		expect(await login).toMatchObject({ type: "oauth", access: "access" });
		expect(tokenForms[0]?.get("code")).toBe("browser-code");
		expect(promptSignal?.aborted).toBe(true);
	});

	it("rejects a pasted redirect URL that belongs to another login, and cancels cleanly", async () => {
		stubBrowserLoginGateway();
		const oauth = createRadiusOAuth({ name: "Radius", gateway: GATEWAY });
		await expect(
			oauth.login(browserInteraction([], () => "http://127.0.0.1:1456/oauth/callback?code=x&state=someone-else")),
		).rejects.toThrow("OAuth state mismatch");

		const controller = new AbortController();
		const events: AuthEvent[] = [];
		const login = oauth.login(browserInteraction(events, () => undefined, controller.signal));
		await vi.waitFor(() => expect(events.some((event) => event.type === "auth_url")).toBe(true));
		controller.abort();
		await expect(login).rejects.toThrow("Login cancelled");
	});
});
