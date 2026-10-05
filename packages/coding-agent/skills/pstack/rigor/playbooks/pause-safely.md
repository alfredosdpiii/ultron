### Pause safely

**You own a clean stop. Leave a checkpoint a cold-start agent can resume from.** Explicit only: on "keep going",
"going to bed, keep going" or "don't stop", do not pause.

1. Stop at a safe boundary. Finish or back out of the current atomic step, start nothing new, and stop running
   children and jobs you own.
2. Take no irreversible action to pause. No PR and no push unless one was already out.
3. Make the work durable. Commit uncommitted edits as one `wip:` commit on the current branch; if the tree is broken,
   say so in one line of the commit body. Children's unmerged worktree branches stay; list them.
4. Write the resume note to a file outside the context (for example `/tmp/<slug>-resume.md`): intent, what you were
   doing, progress and what is verified, current state, next steps, key files, gotchas. If a decision trail exists,
   point at it instead of duplicating it. `state` survives a kernel restart but not a new session; the note is what a
   cold start reads.

**Reply:** where you are in the loop, what is on disk versus only in your head (paths, no diff dumps), the commits made
and whether the tree is clean, and the first action on resume. This is a pause, not a final report.
