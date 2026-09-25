#!/usr/bin/env node
import { consumeInternalProcessRole } from "./process.ts";
import { runSessionWorkerProcess } from "./session-worker.ts";

const role = consumeInternalProcessRole();
if (role !== "session-worker")
	throw new Error("Session worker entrypoint requires an internal session-worker invocation");
void runSessionWorkerProcess(process.argv.slice(2)).catch((error: unknown) => {
	console.error(error);
	process.exit(1);
});
