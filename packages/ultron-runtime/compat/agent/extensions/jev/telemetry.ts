import { createHash } from "node:crypto";
import { appendFileSync, mkdirSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";

export function hashText(value: string): string {
  return createHash("sha256").update(value).digest("hex").slice(0, 16);
}

export function traceEvent(type: string, data: Record<string, unknown> = {}): void {
  try {
    const now = new Date();
    const directory = process.env.PI_JEV_TRACE_DIR ?? join(homedir(), ".pi", "traces");
    mkdirSync(directory, { recursive: true });
    const path = join(directory, `pi-jev-${now.toISOString().slice(0, 10)}.jsonl`);
    appendFileSync(path, `${JSON.stringify({ timestamp: now.toISOString(), type, ...data })}\n`, "utf8");
  } catch {
    // Telemetry must never change agent behavior.
  }
}
