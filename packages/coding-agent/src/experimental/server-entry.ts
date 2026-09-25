#!/usr/bin/env node
import { consumeInternalProcessRole } from "./process.ts";
import { runServerProcess } from "./server.ts";

const role = consumeInternalProcessRole();
if (role !== "server") throw new Error("Server entrypoint requires an internal server invocation");
void runServerProcess(process.argv.slice(2)).catch((error: unknown) => {
	console.error(error);
	process.exit(1);
});
