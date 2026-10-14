// Offline guard for the fixture generators that run the TypeScript product: no socket may reach a host other than
// loopback. Load it first (`import "./offline-guard.mjs"` in-process, or `NODE_OPTIONS=--import <url>` for spawned
// node processes, which their own children inherit). A blocked connection throws and is appended to
// ULTRON_GUARD_LOG (when set), so a generator can assert afterwards that nothing tried to leave the machine.
//
// With ULTRON_FETCH_STUBS (a JSON file `{ "<METHOD> <url prefix>": [response, ...] }`, each response
// `{ status, headers?, body }` taken in order, the last one repeating), `fetch` to a non-loopback URL is answered from
// the stubs instead and logged to ULTRON_FETCH_LOG as JSON lines `{ method, url, headers, body }`; a request without
// a stub fails and is recorded as a violation.
import dns from "node:dns";
import { appendFileSync, readFileSync } from "node:fs";
import net from "node:net";
import tls from "node:tls";

const guardLog = process.env.ULTRON_GUARD_LOG;
/** This process's blocked attempts (spawned processes report through ULTRON_GUARD_LOG). */
export const violations = [];
const violation = (what) => {
	violations.push(what);
	if (guardLog) appendFileSync(guardLog, `${JSON.stringify({ pid: process.pid, what })}\n`);
	return new Error(`offline guard: ${what}`);
};

/** Fail the generator when anything (here, or in a process that logged to `logFile`) tried to leave loopback. */
export function assertOffline(logFile) {
	let logged = [];
	if (logFile) {
		try {
			logged = readFileSync(logFile, "utf8").split("\n").filter(Boolean);
		} catch {}
	}
	if (violations.length > 0 || logged.length > 0) {
		throw new Error(`offline guard: blocked non-loopback traffic:\n${[...violations, ...logged].join("\n")}`);
	}
}

/** The `NODE_OPTIONS` value that loads this guard into spawned node processes. */
export const guardNodeOptions = `--import ${new URL(import.meta.url).href}`;
const loopback = (host) =>
	host === undefined ||
	host === null ||
	host === "" ||
	host === "localhost" ||
	host === "::1" ||
	host === "[::1]" ||
	/^127\./.test(host) ||
	host === "0.0.0.0";

// A unix socket path (`path: null` on an https request's options is no socket).
const socketPath = (path) => (typeof path === "string" && path !== "" ? path : undefined);

// Name resolution: only loopback names resolve (a socket to a non-loopback name never gets an address).
const lookup = dns.lookup;
dns.lookup = function guardedLookup(hostname, ...rest) {
	if (!loopback(hostname)) {
		const callback = rest[rest.length - 1];
		const error = violation(`lookup of ${hostname}`);
		if (typeof callback === "function") process.nextTick(() => callback(error));
		return {};
	}
	return lookup.call(this, hostname, ...rest);
};
const promisesLookup = dns.promises.lookup;
dns.promises.lookup = (hostname, ...rest) =>
	loopback(hostname) ? promisesLookup(hostname, ...rest) : Promise.reject(violation(`lookup of ${hostname}`));

// Connections, however they are opened (IP literals skip the lookup).
const hostOf = (args) => {
	let options = args[0];
	if (Array.isArray(options)) options = options[0];
	if (options !== null && typeof options === "object")
		return { host: options.host ?? options.hostname, path: socketPath(options.path) };
	if (typeof options === "number" || (typeof options === "string" && /^\d+$/.test(options)))
		return { host: typeof args[1] === "string" ? args[1] : undefined };
	if (typeof options === "string") return { path: options };
	return {};
};
for (const [module, name] of [
	[net, "connect"],
	[net, "createConnection"],
	[tls, "connect"],
]) {
	const original = module[name];
	module[name] = function guarded(...args) {
		const { host, path } = hostOf(args);
		if (path === undefined && !loopback(host)) throw violation(`connection to ${host}`);
		return original.apply(this, args);
	};
}
const connect = net.Socket.prototype.connect;
net.Socket.prototype.connect = function guardedConnect(...args) {
	let options = args[0];
	if (Array.isArray(options)) options = options[0];
	let host;
	let path;
	if (options !== null && typeof options === "object") {
		host = options.host ?? options.hostname;
		path = socketPath(options.path);
	} else if (typeof options === "number" || (typeof options === "string" && /^\d+$/.test(options))) {
		host = typeof args[1] === "string" ? args[1] : undefined;
	} else if (typeof options === "string") {
		path = options;
	}
	if (path === undefined && !loopback(host)) throw violation(`connection to ${host}`);
	return connect.apply(this, args);
};

const stubsPath = process.env.ULTRON_FETCH_STUBS;
if (stubsPath) {
	const stubs = JSON.parse(readFileSync(stubsPath, "utf8"));
	const used = new Map();
	const fetchLog = process.env.ULTRON_FETCH_LOG;
	// The product may install its own fetch later (undici's, http-dispatcher.ts): keep it for loopback requests and
	// keep answering every other request from the stubs.
	let realFetch = globalThis.fetch;
	const stubbed = async (input, init = {}) => {
		const url = typeof input === "string" ? input : input instanceof URL ? input.href : input.url;
		const parsed = new URL(url);
		if (loopback(parsed.hostname)) return realFetch(input, init);
		const method = (init.method ?? (typeof input === "object" && "method" in input ? input.method : "GET")).toUpperCase();
		const headers = Object.fromEntries(new Headers(init.headers ?? {}).entries());
		const body = typeof init.body === "string" ? init.body : init.body instanceof URLSearchParams ? init.body.toString() : null;
		if (fetchLog) appendFileSync(fetchLog, `${JSON.stringify({ method, url, headers, body })}\n`);
		const key = Object.keys(stubs).find((k) => `${method} ${url}`.startsWith(k));
		if (key === undefined) throw violation(`unstubbed ${method} ${url}`);
		const index = used.get(key) ?? 0;
		used.set(key, index + 1);
		const responses = stubs[key];
		const response = responses[Math.min(index, responses.length - 1)];
		const text = typeof response.body === "string" ? response.body : JSON.stringify(response.body);
		return new Response(text, {
			status: response.status ?? 200,
			headers: { "content-type": "application/json", ...(response.headers ?? {}) },
		});
	};
	Object.defineProperty(globalThis, "fetch", {
		configurable: true,
		enumerable: true,
		get: () => stubbed,
		set: (value) => {
			if (value !== stubbed) realFetch = value;
		},
	});
}
