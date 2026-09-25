import { mkdtempSync, rmSync } from "node:fs";
import { join } from "node:path";
import { afterAll } from "vitest";

const created: string[] = [];

/**
 * A private ULTRON_SERVER_DIR directly under /tmp, short enough for the Unix socket paths inside it. It is removed
 * once the test file is done, after every suite's own teardown has stopped the clients that used it.
 */
export function tempServerDir(prefix: string): string {
	const directory = mkdtempSync(join("/tmp", prefix));
	created.push(directory);
	return directory;
}

/** Remove the server directories created so far (for suites that clean up after each test). */
export function removeTempServerDirs(): void {
	for (const directory of created.splice(0)) rmSync(directory, { recursive: true, force: true });
}

afterAll(removeTempServerDirs);
