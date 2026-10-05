### Visual parity (optional, web UIs)

**You own pixel-exact equivalence. The baseline is the spec; you do not touch it.** Equivalence is verified by image
diff, not by eye. Needs a way to screenshot the UI: a browser MCP server (`await mcp.servers()`) or the project's own
visual regression tooling (Playwright and the like, run through `bash`). Without one, say so and stop.

1. Establish the baseline before any migration: a harness that screenshots the current component across its states,
   plus the target when matching two implementations. No baseline, no parity claim.
2. Hold the anti-shortcut clauses: no harness changes, no baseline edits, no restructuring a component to pass a
   diff. If the baseline looks wrong, stop and ask.
3. Migrate one component at a time. Shared primitives migrate first, as a blocking phase. Then one child per
   component, each `worktree=True` (`the separate-before-serializing-shared-state principle (`../principle-separate-before-serializing-shared-state/SKILL.md`)`), merged with `rlm.merge`.
4. Verify each component against its baseline by image diff. A nonzero diff is a fail: investigate the pixel delta
   and iterate per component until it is zero.
5. Run `playbooks/opening-a-pr.md` per component or per safe batch.

**Reply:** components migrated, each diff result, the harness location, what's left.
