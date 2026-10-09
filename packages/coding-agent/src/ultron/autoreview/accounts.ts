/**
 * The GitHub accounts `ultron autoreview` reviews as: every account `gh` is logged in to, on every host. A token
 * is read with `gh auth token`, kept in memory, registered with the secret-masking list, and handed to child
 * `gh`/`git` processes only through their environment.
 */
import { registerSecretValues } from "../rlm/output-secrets.ts";
import type { Runner } from "./runner.ts";

export interface Account {
	readonly login: string;
	readonly host: string;
	/** The account `gh` uses by default on this host. */
	readonly active?: boolean;
}

export function accountKey(account: Account): string {
	return `${account.host}/${account.login}`;
}

/** Accounts from `gh auth status --json hosts`, or from its text form on a `gh` that has no `--json`. */
export function parseAuthStatus(stdout: string, stderr = ""): Account[] {
	const accounts: Account[] = [];
	const add = (login: string, host: string, active = false) => {
		if (login && host && !accounts.some((item) => item.login === login && item.host === host))
			accounts.push({ login, host, ...(active ? { active: true } : {}) });
	};
	try {
		const parsed = JSON.parse(stdout) as {
			hosts?: Record<string, Array<{ login?: unknown; state?: unknown; active?: unknown }>>;
		};
		for (const [host, entries] of Object.entries(parsed.hosts ?? {}))
			for (const entry of Array.isArray(entries) ? entries : [])
				if (typeof entry.login === "string" && (entry.state === undefined || entry.state === "success"))
					add(entry.login, host, entry.active === true);
		return accounts;
	} catch {
		// An older gh: "✓ Logged in to github.com account octocat (keyring)".
	}
	for (const line of `${stdout}\n${stderr}`.split("\n")) {
		const match = /Logged in to (\S+) (?:account|as) (\S+)/.exec(line);
		if (match) add(match[2]!, match[1]!);
	}
	return accounts;
}

/** The logged-in accounts, restricted to `only` (logins, or `host/login`) when given. */
export async function listAccounts(runner: Runner, only?: readonly string[]): Promise<Account[]> {
	let result = await runner(["gh", "auth", "status", "--json", "hosts"]);
	if (result.code === 127) throw new Error("ultron autoreview needs the GitHub CLI (gh), which is not installed");
	if (result.code !== 0 || !result.stdout.trim().startsWith("{")) result = await runner(["gh", "auth", "status"]);
	const accounts = parseAuthStatus(result.stdout, result.stderr);
	if (only === undefined || only.length === 0) return accounts;
	const wanted = only.map((item) => item.toLowerCase());
	return accounts.filter(
		(account) =>
			wanted.includes(account.login.toLowerCase()) ||
			wanted.includes(`${account.host}/${account.login}`.toLowerCase()),
	);
}

/** The environment that authenticates a child `gh` or `git` as an account. */
export function authEnv(account: Account, token: string): Record<string, string> {
	// gh reads GH_TOKEN for github.com and *.ghe.com, and GH_ENTERPRISE_TOKEN for any other host.
	const hosted = account.host === "github.com" || account.host.endsWith(".ghe.com");
	return { [hosted ? "GH_TOKEN" : "GH_ENTERPRISE_TOKEN"]: token, GH_HOST: account.host };
}

/** Tokens by account, read once and held in memory only. */
export class TokenStore {
	readonly #runner: Runner;
	readonly #tokens = new Map<string, string>();

	constructor(runner: Runner) {
		this.#runner = runner;
	}

	async env(account: Account): Promise<Record<string, string>> {
		const key = accountKey(account);
		let token = this.#tokens.get(key);
		if (token === undefined) {
			const result = await this.#runner([
				"gh",
				"auth",
				"token",
				"--user",
				account.login,
				"--hostname",
				account.host,
			]);
			token = result.stdout.trim();
			if (result.code !== 0 || token === "")
				throw new Error(`gh has no token for ${account.login} on ${account.host}`);
			registerSecretValues([token]);
			this.#tokens.set(key, token);
		}
		return authEnv(account, token);
	}

	/** Forget a token (after an authentication failure, so the next call reads it again). */
	forget(account: Account): void {
		this.#tokens.delete(accountKey(account));
	}
}
