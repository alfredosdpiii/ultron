/** Loads `ultron autoreview` only when the command runs (see cli.ts). */
export const loadAutoreviewCommand = () => import("./cli.ts");
