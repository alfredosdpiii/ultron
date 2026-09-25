import { value } from "@ultron/agent-core";

/**
 * Written by the server into a Session it creates as an in-session fork (Pi's `/fork`, `/clone`, RPC `fork`/`clone`),
 * read and removed by the Session's first worker: its extensions then see Pi's `session_start` with reason "fork" and
 * the source Session file.
 */
export const forkedSessionStart = value<{ previousSessionFile: string }>("ultron.session.start", "fork");
