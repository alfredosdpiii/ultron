### Session pickup

**You own the resume point. Read the prior trail, don't redo it.**

1. Locate the trail: a resume note (`playbooks/pause-safely.md`), a decision trail, a pushed branch, or a prior Ultron
   session (find and parse it with `../recall/scripts/sessions.py`, which knows both session layouts; only this project's). Load a transcript as a handle
   (`h = await rlm.load(path=...)`), read its last messages first, then `h.search(...)` back for decision points. Keep
   the reduced timeline in a variable and print only that (`the guard-the-context-window principle (`../principle-guard-the-context-window/SKILL.md`)`).
2. Reconstruct operational state: branch and worktree, what landed (`git log`, `git diff` against the base), open
   plan items, decisions made. The trail is authoritative input; resist re-deriving it.
3. Diff done against pending and name the resume point. Do not re-run the prior repro or redo finished work. A "verify
   from scratch" pass treats an authoritative trail as untrustworthy.
4. Route the rest to the matching playbook with a verdict: continue the execution, ship a finished recommendation,
   ratify or override a prior conclusion, or postmortem a failed run. The routed playbook owns the rest.
5. Verify the inherited claims against the original goal on the real artifact (`the prove-it-works principle (`../principle-prove-it-works/SKILL.md`)`). A prior
   self-report is not proof.

**Reply:** where the prior agent stopped, what you inherited versus redid (ideally nothing), the resume point, the
outcome.
