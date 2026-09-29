import { compare, valid } from "semver";
import { NPM_PACKAGE_NAME } from "../config.ts";
import { fetchWithRetry } from "./management-http.ts";
import { getPiUserAgent } from "./pi-user-agent.ts";

/** The npm registry document for the `latest` dist-tag of the published package. */
export const LATEST_VERSION_URL = `https://registry.npmjs.org/${NPM_PACKAGE_NAME}/latest`;
const DEFAULT_VERSION_CHECK_TIMEOUT_MS = 10000;

export interface LatestRelease {
	version: string;
	/** The npm package to install for this release. */
	packageName: string;
}

function isTruthy(value: string | undefined): boolean {
	return value !== undefined && value.trim() !== "" && !/^(0|false|no)$/i.test(value.trim());
}

/** Automatic update checks are off in offline mode and when PI_SKIP_VERSION_CHECK or ULTRON_SKIP_VERSION_CHECK is set. */
export function areAutomaticVersionChecksDisabled(env: NodeJS.ProcessEnv = process.env): boolean {
	return Boolean(env.PI_OFFLINE) || isTruthy(env.PI_SKIP_VERSION_CHECK) || isTruthy(env.ULTRON_SKIP_VERSION_CHECK);
}

/** Include useful errno details hidden behind Node's generic "fetch failed" error. */
export function formatVersionCheckError(error: unknown): string {
	const rootMessage = error instanceof Error && error.message ? error.message : String(error);
	const cause = error instanceof Error ? error.cause : undefined;
	const causes = cause instanceof AggregateError ? cause.errors : cause === undefined ? [] : [cause];
	const codes = causes
		.map((value) =>
			typeof value === "object" && value !== null && "code" in value && typeof value.code === "string"
				? value.code
				: undefined,
		)
		.filter((code): code is string => code !== undefined);

	if (codes.length > 0) return `${rootMessage} (${[...new Set(codes)].join(", ")})`;
	const causeMessage = causes.find(
		(value): value is Error => value instanceof Error && Boolean(value.message),
	)?.message;
	return causeMessage ? `${rootMessage} (cause: ${causeMessage})` : rootMessage;
}

export function comparePackageVersions(leftVersion: string, rightVersion: string): number | undefined {
	const left = valid(leftVersion.trim());
	const right = valid(rightVersion.trim());
	if (!left || !right) {
		return undefined;
	}
	return compare(left, right);
}

export function isNewerPackageVersion(candidateVersion: string, currentVersion: string): boolean {
	const comparison = comparePackageVersions(candidateVersion, currentVersion);
	if (comparison !== undefined) {
		return comparison > 0;
	}
	return candidateVersion.trim() !== currentVersion.trim();
}

/** Ask the npm registry for the latest published version of Ultron. */
export async function getLatestRelease(
	currentVersion: string,
	options: { timeoutMs?: number; retry?: boolean } = {},
): Promise<LatestRelease | undefined> {
	if (process.env.PI_OFFLINE) return undefined;

	const response = await fetchWithRetry(
		LATEST_VERSION_URL,
		{
			headers: {
				"User-Agent": getPiUserAgent(currentVersion),
				accept: "application/json",
			},
		},
		{
			maxRetries: options.retry ? 2 : 0,
			timeoutMs: options.timeoutMs ?? DEFAULT_VERSION_CHECK_TIMEOUT_MS,
		},
	);
	if (!response.ok) return undefined;

	const data = (await response.json()) as { version?: unknown };
	if (typeof data.version !== "string" || !valid(data.version.trim())) {
		return undefined;
	}
	return { version: data.version.trim(), packageName: NPM_PACKAGE_NAME };
}

export async function getLatestVersion(
	currentVersion: string,
	options: { timeoutMs?: number; retry?: boolean } = {},
): Promise<string | undefined> {
	return (await getLatestRelease(currentVersion, options))?.version;
}

/** The startup update notification: a newer release, or undefined when there is none or checks are disabled. */
export async function checkForNewVersion(currentVersion: string): Promise<LatestRelease | undefined> {
	if (areAutomaticVersionChecksDisabled()) return undefined;

	try {
		const latestRelease = await getLatestRelease(currentVersion);
		if (latestRelease && isNewerPackageVersion(latestRelease.version, currentVersion)) {
			return latestRelease;
		}
		return undefined;
	} catch {
		return undefined;
	}
}
