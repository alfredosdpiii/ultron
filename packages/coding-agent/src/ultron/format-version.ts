import { type Session, value } from "@ultron/agent-core";
import type { Context, JsonValue } from "@ultron/chord";

/**
 * Format versions of the session values Ultron writes (the unreal-agent invariant).
 *
 * Every durable document carries a format version. Reading one:
 * - without a version reads it as version 1 (every document written before versioning has the v1 shape);
 * - with a version this build supports reads it normally;
 * - with any other version fails explicitly with {@link UnsupportedFormatVersionError}, naming the namespace and
 *   the version. It is never treated as corrupt or empty, so an older Ultron never overwrites a newer document.
 *
 * Entries inside a document (tasks, calls, memory operations, refinement events, decisions) inherit the version of
 * the document that holds them: each document is replaced whole and atomically, so an entry can never be written
 * under a different format than its document.
 */
export const CURRENT_FORMAT_VERSION = 1;

export class UnsupportedFormatVersionError extends Error {
	readonly code = "UNSUPPORTED_FORMAT_VERSION";
	readonly namespace: string;
	readonly version: unknown;
	readonly supported: readonly number[];

	constructor(namespace: string, version: unknown, supported: readonly number[] = [CURRENT_FORMAT_VERSION]) {
		super(
			`Cannot resume: session value ${namespace} has format version ${JSON.stringify(version)}, but this Ultron ` +
				`reads only format version ${supported.join(", ")}. It was left unchanged; open the session with the ` +
				"Ultron version that wrote it, or upgrade.",
		);
		this.name = "UnsupportedFormatVersionError";
		this.namespace = namespace;
		this.version = version;
		this.supported = supported;
	}
}

export type FormatOptions = {
	/** Name of the version field; most documents use `version`. */
	field?: string;
	supported?: readonly number[];
};

/**
 * Checks a stored document's format version and returns it with the version field present (a document without
 * one is read as version 1). Values that are not objects are returned unchanged for the caller's own validation.
 */
export function readVersioned<T extends JsonValue | undefined>(
	namespace: string,
	saved: T,
	options: FormatOptions = {},
): T {
	if (saved === undefined || saved === null || typeof saved !== "object" || Array.isArray(saved)) return saved;
	const field = options.field ?? "version";
	const supported = options.supported ?? [CURRENT_FORMAT_VERSION];
	const document = saved as { [key: string]: JsonValue };
	if (!Object.hasOwn(document, field)) return { ...document, [field]: CURRENT_FORMAT_VERSION } as T;
	const version = document[field];
	if (typeof version !== "number" || !supported.includes(version))
		throw new UnsupportedFormatVersionError(namespace, version, supported);
	return saved;
}

/** Version field of each Ultron namespace, for the resume-time check. */
function moduleField(key: string): string {
	return key === "skills" ? "format" : "version";
}

/** Session values checked when a session resumes; artifacts are checked when read (they can be large). */
const SINGLE_VALUES: ReadonlyArray<{ namespace: string; key: string; field: string }> = [
	{ namespace: "ultron.tasks", key: "root", field: "version" },
	{ namespace: "ultron.definitions", key: "root", field: "version" },
	{ namespace: "ultron.usage", key: "root", field: "version" },
	{ namespace: "ultron.memory.state", key: "root", field: "version" },
	{ namespace: "ultron.jev.decisions", key: "root", field: "version" },
	{ namespace: "ultron.local", key: "ultron.refinements", field: "formatVersion" },
	{ namespace: "ultron.local", key: "ultron.experiments", field: "formatVersion" },
	{ namespace: "ultron.local", key: "ultron.artifact-refs", field: "formatVersion" },
];

/**
 * Resume-time check: every versioned session value Ultron owns must be readable by this build. Throws
 * {@link UnsupportedFormatVersionError} for the first value written in a format this build does not know, before
 * any of it is loaded or rewritten.
 */
export async function assertSessionFormatsReadable(
	session: Pick<Session, "getValue" | "scanValues">,
	context: Context,
): Promise<void> {
	for (const { namespace, key, field } of SINGLE_VALUES) {
		const stored = await session.getValue(value<JsonValue>(namespace, key), context);
		readVersioned(`${namespace}/${key}`, stored?.value, { field });
	}
	for (const stored of await session.scanValues(value<JsonValue>("ultron.module", ""), context))
		readVersioned(`ultron.module/${stored.address.key}`, stored.value, { field: moduleField(stored.address.key) });
}
