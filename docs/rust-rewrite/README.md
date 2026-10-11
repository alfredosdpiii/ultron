# Rust rewrite references

The canonical execution plan is [`../../rewriteplan.md`](../../rewriteplan.md).

- [`baseline.json`](baseline.json): pinned source revisions, package census, and A01–A56 reference evidence. Rust coverage is not implemented.
- [`decisions.tsv`](decisions.tsv): append-only decision history. Earlier links to sections in this directory's README refer to the plan now at the repository root.

The approved architecture is a Rust host and durable engine with a persistent Python REPL.
