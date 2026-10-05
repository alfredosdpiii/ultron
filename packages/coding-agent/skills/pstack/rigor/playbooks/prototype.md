### Prototype

**You own the design decision, not the code. The prototype is a throwaway instrument; the real build follows Feature.**

The one playbook where "smallest change" and the verification bar invert. Speed over polish, no planning, code quality
does not matter. The rigor is in picking the right design cheaply. Propose variations the user didn't ask for; throw an
approach away and try another.

1. Scope the decision: which layout, interaction or density, or for an empirical fork which behavior, timing or
   approach. No decision means no prototype; route to Feature.
2. When the design space is open, gather references: prior art, a moodboard of themes and layouts, and let the user
   pick directions. Skip when the direction is set.
3. Build in an isolated scratch dir outside production source. Visual: vanilla HTML/CSS/JS or the lightest stack,
   CDN deps, a dev server started as a job (`await bash("...", yield_after=0)`). Behavioral or timing: the smallest
   script that exercises the question. No framework, no tests, no abstractions.
4. Compare alternatives behind one switcher (buttons or a keypress), each variant labeled
   (`the exhaust-the-design-space principle (`../principle-exhaust-the-design-space/SKILL.md`)`, made cheap). Independent variants can be built by parallel children.
5. Observe on the matching surface. Visual: screenshot each variant through a browser MCP server and look at it with
   `await view_image(path)`, then drive the interaction. Behavioral: log the timing, print the output. The observation
   is the test.
6. Present alternatives, tradeoffs and a recommendation. Hand the chosen direction to Feature (or
   `../architect/SKILL.md` for the shape).

**Reply:** variants explored, the evidence (screenshots or observed output and timing), tradeoffs, your
recommendation, the scratch path. Say plainly that the prototype is throwaway.
