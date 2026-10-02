import { getProviderEnvValue } from "../../utils/provider-env.ts";

const LOOPBACK = "127.0.0.1";

/**
 * The interface a login's callback server listens on: loopback, unless `ULTRON_OAUTH_CALLBACK_HOST` (or Pi's
 * `PI_OAUTH_CALLBACK_HOST`) names another one.
 *
 * Inside a container the browser runs on the host, where `localhost:<port>` is not the container's loopback.
 * Publishing the port (`docker run -p 127.0.0.1:1455:1455`) only reaches a server that listens on the container's
 * network interface, so that setup needs `ULTRON_OAUTH_CALLBACK_HOST=0.0.0.0`. It is opt-in because it exposes
 * the callback endpoint to whatever can reach that interface.
 */
export function oauthCallbackHost(): string {
	return (
		getProviderEnvValue("ULTRON_OAUTH_CALLBACK_HOST") || getProviderEnvValue("PI_OAUTH_CALLBACK_HOST") || LOOPBACK
	);
}

/** The host to put in a redirect URL for a server listening on `host`: a wildcard address is reached over loopback. */
export function oauthRedirectHost(host: string): string {
	return host === "0.0.0.0" || host === "::" || host === "[::]" ? LOOPBACK : host;
}
