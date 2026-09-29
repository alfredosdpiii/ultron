import { describe, expect, it } from "vitest";
import { loadFacetBundler } from "../src/experimental/plugins/package.ts";

function moduleNotFound(specifier: string): Error {
	return Object.assign(new Error(`Cannot find package '${specifier}' imported from /x/bundle.js`), {
		code: "ERR_MODULE_NOT_FOUND",
	});
}

describe("experimental plugin bundler loading", () => {
	it("loads the chord bundler on demand", async () => {
		const bundler = await loadFacetBundler();
		expect(typeof bundler.bundleFacetPackage).toBe("function");
	});

	it("explains how to install esbuild when an npm install leaves it out", async () => {
		const loading = loadFacetBundler(async () => {
			throw moduleNotFound("esbuild");
		});
		await expect(loading).rejects.toThrow(/npm install -g esbuild/);
	});

	it("passes other load failures through unchanged", async () => {
		const failure = moduleNotFound("something-else");
		await expect(
			loadFacetBundler(async () => {
				throw failure;
			}),
		).rejects.toBe(failure);
	});
});
