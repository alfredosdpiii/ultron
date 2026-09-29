import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
	areAutomaticVersionChecksDisabled,
	checkForNewVersion,
	comparePackageVersions,
	formatVersionCheckError,
	getLatestRelease,
	getLatestVersion,
	isNewerPackageVersion,
	LATEST_VERSION_URL,
} from "../src/utils/version-check.ts";
import { allowNetwork } from "./test-network-env.ts";

/** The shape of https://registry.npmjs.org/<name>/latest: the full manifest of the `latest` dist-tag. */
function registryManifest(version: string): Response {
	return Response.json({ name: "ultron-agent", version, dist: { tarball: "https://example.test/x.tgz" } });
}

beforeEach(() => {
	allowNetwork();
	vi.stubEnv("PI_SKIP_VERSION_CHECK", undefined);
	vi.stubEnv("ULTRON_SKIP_VERSION_CHECK", undefined);
});

afterEach(() => {
	vi.unstubAllGlobals();
	vi.unstubAllEnvs();
});

describe("version checks", () => {
	it("compares package versions", () => {
		expect(comparePackageVersions("0.70.6", "0.70.5")).toBeGreaterThan(0);
		expect(comparePackageVersions("0.70.5", "0.70.5")).toBe(0);
		expect(comparePackageVersions("0.70.4", "0.70.5")).toBeLessThan(0);
		expect(comparePackageVersions("5.0.0-beta.20", "5.0.0-beta.9")).toBeGreaterThan(0);
		expect(isNewerPackageVersion("0.70.5", "0.70.5")).toBe(false);
		expect(isNewerPackageVersion("0.70.6", "0.70.5")).toBe(true);
	});

	it("returns only newer versions", async () => {
		const fetchMock = vi.fn(async () => registryManifest("1.2.3"));
		vi.stubGlobal("fetch", fetchMock);

		await expect(checkForNewVersion("1.2.3")).resolves.toBeUndefined();
		await expect(checkForNewVersion("1.2.2")).resolves.toEqual({ version: "1.2.3", packageName: "ultron-agent" });
	});

	it("asks the npm registry for the latest ultron-agent release", async () => {
		const fetchMock = vi.fn(async () => registryManifest("1.2.4"));
		vi.stubGlobal("fetch", fetchMock);

		expect(LATEST_VERSION_URL).toBe("https://registry.npmjs.org/ultron-agent/latest");
		await expect(getLatestVersion("1.2.3")).resolves.toBe("1.2.4");
		expect(fetchMock).toHaveBeenCalledWith(
			"https://registry.npmjs.org/ultron-agent/latest",
			expect.objectContaining({
				headers: expect.objectContaining({
					"User-Agent": expect.stringMatching(/^pi\/1\.2\.3 /),
					accept: "application/json",
				}),
				signal: expect.any(AbortSignal),
			}),
		);
	});

	it("never contacts pi.dev", async () => {
		const fetchMock = vi.fn(async () => registryManifest("1.2.4"));
		vi.stubGlobal("fetch", fetchMock);

		await getLatestRelease("1.2.3", { retry: true });
		await checkForNewVersion("1.2.3");
		expect(fetchMock).toHaveBeenCalledTimes(2);
		for (const [url] of fetchMock.mock.calls as unknown as [string][]) {
			expect(new URL(url).hostname).toBe("registry.npmjs.org");
		}
	});

	it("installs the published package whatever name the registry document carries", async () => {
		vi.stubGlobal(
			"fetch",
			vi.fn(async () => Response.json({ name: "something-else", version: "2.0.0" })),
		);

		await expect(getLatestRelease("1.2.3")).resolves.toEqual({ version: "2.0.0", packageName: "ultron-agent" });
	});

	it("ignores registry errors and malformed versions", async () => {
		vi.stubGlobal(
			"fetch",
			vi.fn(async () => Response.json({ error: "Not found" }, { status: 404 })),
		);
		await expect(getLatestRelease("1.2.3")).resolves.toBeUndefined();

		vi.stubGlobal(
			"fetch",
			vi.fn(async () => Response.json({ version: "not-a-version" })),
		);
		await expect(getLatestRelease("1.2.3")).resolves.toBeUndefined();
		await expect(checkForNewVersion("1.2.3")).resolves.toBeUndefined();
	});

	it("retries a transient version request when explicitly requested", async () => {
		const fetchMock = vi
			.fn()
			.mockRejectedValueOnce(new Error("fetch failed"))
			.mockRejectedValueOnce(new Error("fetch failed"))
			.mockResolvedValueOnce(registryManifest("1.2.4"));
		vi.stubGlobal("fetch", fetchMock);

		await expect(getLatestRelease("1.2.3", { retry: true })).resolves.toEqual({
			version: "1.2.4",
			packageName: "ultron-agent",
		});
		expect(fetchMock).toHaveBeenCalledTimes(3);
	});

	it("keeps automatic version checks to one request", async () => {
		const fetchMock = vi.fn().mockRejectedValue(new Error("fetch failed"));
		vi.stubGlobal("fetch", fetchMock);

		await expect(checkForNewVersion("1.2.3")).resolves.toBeUndefined();
		expect(fetchMock).toHaveBeenCalledOnce();
	});

	it("formats nested network error details", () => {
		const error = new Error("fetch failed", {
			cause: new AggregateError([
				Object.assign(new Error("connect timeout"), { code: "ETIMEDOUT" }),
				Object.assign(new Error("network unreachable"), { code: "ENETUNREACH" }),
			]),
		});

		expect(formatVersionCheckError(error)).toBe("fetch failed (ETIMEDOUT, ENETUNREACH)");
	});

	it.each([
		["PI_SKIP_VERSION_CHECK", "1"],
		["ULTRON_SKIP_VERSION_CHECK", "1"],
		["ULTRON_SKIP_VERSION_CHECK", "true"],
		["PI_OFFLINE", "1"],
	])("skips automatic registry calls when %s=%s", async (name, value) => {
		vi.stubEnv(name, value);
		const fetchMock = vi.fn();
		vi.stubGlobal("fetch", fetchMock);

		expect(areAutomaticVersionChecksDisabled()).toBe(true);
		await expect(checkForNewVersion("1.2.3")).resolves.toBeUndefined();
		expect(fetchMock).not.toHaveBeenCalled();
	});

	it("treats a falsy opt-out value as enabled", () => {
		expect(areAutomaticVersionChecksDisabled({ ULTRON_SKIP_VERSION_CHECK: "0" })).toBe(false);
		expect(areAutomaticVersionChecksDisabled({ PI_SKIP_VERSION_CHECK: "false" })).toBe(false);
		expect(areAutomaticVersionChecksDisabled({})).toBe(false);
	});

	it("never calls the registry in offline mode, even when asked directly", async () => {
		vi.stubEnv("PI_OFFLINE", "1");
		const fetchMock = vi.fn();
		vi.stubGlobal("fetch", fetchMock);

		await expect(getLatestRelease("1.2.3", { retry: true })).resolves.toBeUndefined();
		expect(fetchMock).not.toHaveBeenCalled();
	});

	it("allows direct registry calls when automatic version checks are disabled", async () => {
		vi.stubEnv("PI_SKIP_VERSION_CHECK", "1");
		vi.stubEnv("ULTRON_SKIP_VERSION_CHECK", "1");
		const fetchMock = vi.fn(async () => registryManifest("1.2.4"));
		vi.stubGlobal("fetch", fetchMock);

		await expect(getLatestVersion("1.2.3")).resolves.toBe("1.2.4");
		expect(fetchMock).toHaveBeenCalledOnce();
	});
});
