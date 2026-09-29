import type { ProviderStreams } from "../types.ts";
import { lazyApi } from "./lazy.ts";

/**
 * The implementation reaches Node built-ins only through `process.getBuiltinModule`, so bundling it for a
 * browser is harmless; it fails at request time there with a clear error.
 */
export const claudeCodeCliApi = (): ProviderStreams => lazyApi(() => import("./claude-code-cli.ts"));
