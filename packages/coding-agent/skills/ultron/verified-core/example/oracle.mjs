// Reads [[entries oldest-first as [k, v]], key] cases as JSON lines; prints the oracle's answer per case.
import R from "./replay.mjs";
import { createInterface } from "node:readline";
for await (const line of createInterface({ input: process.stdin })) {
  const [entries, key] = JSON.parse(line);
  let list = { $: "Nil" };
  for (const [k, v] of entries) list = { $: "Con", head: { $: "Put", key: k, val: v }, tail: list }; // newest first
  const out = R.get(list, key);
  console.log(JSON.stringify(out.$ === "Some" ? out.value : null));
}
