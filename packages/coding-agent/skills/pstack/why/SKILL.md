---
name: why
description: "Use for 'why does X work this way', 'why we picked Y', design rationale, regressions, postmortems, or data-backed thresholds. Searches git history and every configured MCP evidence source (issue tracker, docs, team chat, observability, error tracking, analytics warehouse), then returns a cited, confidence-tiered read on decisions and tradeoffs. Use how for runtime behavior."
disable-model-invocation: true
license: MIT (see ../LICENSE)
metadata:
  source: github.com/cursor/plugins/pstack/skills/why
  author: Lauren Tan
  modified: ported to Ultron's REPL
---

# Why

Investigate the motivation and intent behind code.

Companion to `../how/SKILL.md`. `how` answers what the code does and how it works. `why` answers what forces led to its shape.

You gather the evidence yourself, in your REPL: git and `gh` through `bash`, other sources through the `mcp` gateway. No subagents: the work is reading and searching, not independent multi-step changes. One `rlm.infer` frame writes the synthesis; you check it and present it.

## Operating posture

Be a careful, cautious, precise investigator. Separate what you know from what you infer. Read `references/epistemics.md` (in this skill's directory) before writing anything: it defines the confidence tiers and phrasing, and the synthesis frame gets it verbatim.

Keep everything in `state["why"]` (anchor, coverage map, evidence) so it survives a kernel restart.

## Step 1. Target and question

The **target** is usually a chunk of code, a pattern, a feature or a named decision. The **question** is a design rationale, a tradeoff, a motivating edge case, an external constraint, dead code, or a broad history sweep.

If the target is vague, make your best guess from the conversation (recent edits, what was just discussed), state it in one line, and proceed. The user can redirect.

## Step 2. Code anchor and source-control history

Source control is always available, so this step is also the source-control investigation. Follow `references/sources/code-archaeology.md`. Build the anchor in one cell:

```python
target, start, end = "src/core/retry.ts", 40, 80
log, blame = await asyncio.gather(
    bash(f'''git log --follow --format='%h %ad %an %s' --date=short -- {target} | head -30'''),
    bash(f'''git blame -L {start},{end} --date=short {target}'''),
)
print(log)
print(blame)
import re
prs = sorted(set(re.findall(r"\(#(\d+)\)", str(log))))
views = await asyncio.gather(*(
    bash(f'''gh pr view {n} --json number,title,body,author,mergedAt,labels,closingIssuesReferences,comments,reviews''')
    for n in prs[:8]
))
state["why"] = {"target": target, "lines": [start, end], "prs": prs,
                "evidence": {"source control": [str(v) for v in views if v.ok]}}
```

Pickaxe (`git log -S`) for the exact constant or string the question is about. Record ticket IDs and links you see in commit messages and PR bodies: they seed the other sources.

## Step 3. Discover the other sources

Map each configured MCP server to one evidence category: issue / ticket tracker, long-form documents, real-time team chat, infrastructure observability, error / exception tracking, product analytics warehouse. Only servers that exist count; never assume one.

```python
try:
    servers = [s for s in await mcp.servers() if not s.get("disabled")]
except Exception as error:  # no MCP gateway in this session
    servers = []
    print("MCP unavailable:", error)
for s in servers:
    print(s.get("name"), s.get("status"), s.get("toolCount"))
```

Classify by server name, its tool names (`await mcp.tools(name)`) and, if unclear, `await mcp.instructions(name)`. A server that fits two categories goes in the one matching its primary evidence. Record the map, including the categories with no server:

```python
state["why"]["coverage"] = {
    "source control": "git + gh",
    "issue tracker": "linear",       # example; None when no server fits
    "documents": None,
    "team chat": None,
    "observability": None,
    "error tracking": None,
    "analytics warehouse": None,
}
```

Aim for a complete coverage map, not a minimal one. A category with a server is searched; a null result is a finding. Skip a configured category only when it is provably irrelevant (error tracking for a build-time script with no runtime path), and write the reason down. A category with no server is a gap, not a choice.

If the target looks defensive (null checks, retries, timeouts, rate limits, feature flags, egress guards, OOM handlers), add the incident angle from `references/sources/incident-postmortem.md` to every source you search.

## Step 4. Search the sources together

`references/source-playbook.md` indexes one playbook per category. For each available source, read its playbook, find the exact tool names (`await mcp.search(...)`, `await mcp.describe(tool)`), plan its queries from the anchor, then run all sources at once:

```python
async def search_source(category, server, calls):
    found = []
    for tool, args in calls:  # within one source, later queries often follow earlier hits
        try:
            found.append({"tool": tool, "args": args, "text": str(await mcp.call(tool, args, server=server))})
        except Exception as error:  # auth required, not connected: a gap, not a finding
            found.append({"tool": tool, "args": args, "error": str(error)})
    return category, found

plan = {  # category -> (server, [(tool, args), ...]), from the playbooks and mcp.describe
    "issue tracker": ("linear", [("list_issues", {"query": "retry backoff"})]),
}
results = await asyncio.gather(*(search_source(c, s, calls) for c, (s, calls) in plan.items()))
for category, found in results:
    state["why"]["evidence"][category] = found
    print(category, [(f["tool"], len(f.get("text", "")), f.get("error")) for f in found])
```

Then go deeper where it paid off: fetch the full ticket, the whole thread, the full postmortem. Stay inside a source when following its links; note cross-source references and chase them in that source's turn.

Raw results can be large. Narrow with code (`h = await rlm.load(text=...)`, `h.search`) and, for long documents a search line cannot settle, extract evidence with frames using `references/investigator-prompt.md` as the task:

```python
skill_dir = "..."  # this skill's directory: "References are relative to <dir>" above
extract = str(await read(f"{skill_dir}/references/investigator-prompt.md")).replace("{QUESTION}", "Why does the retry loop cap at 5?")
docs = [f["text"] for _, found in results for f in found if len(f.get("text", "")) > 4000]
views = [v for d in docs for v in (await rlm.load(text=d)).chunks(16000)]
EVIDENCE = {"direct": list[str], "circumstantial": list[str], "contradictions": list[str], "gaps": list[str], "leads": list[str]}
extracted = await rlm.map(extract, views, contract=EVIDENCE)
```

Track what you searched, not only what you found: the queries are part of the evidence.

## Step 5. Synthesize in one frame

One `rlm.infer` writes the answer from `references/synthesizer-prompt.md`, with the epistemics framework and the gathered evidence as context. Prefer a strong model when one is configured:

```python
import json
ms = await rlm.find_models("opus")
model = f"{ms[0]['provider']}/{ms[0]['id']}" if ms else None
epistemics = str(await read(f"{skill_dir}/references/epistemics.md"))
task = str(await read(f"{skill_dir}/references/synthesizer-prompt.md")).replace("{QUESTION}", "Why does the retry loop cap at 5?")
answer = await rlm.infer(task, context=[epistemics, json.dumps(state["why"], indent=1)], contract=str, model=model)
if not answer:
    print("synthesis incomplete:", answer.status)
```

The frame has no tools, so you do the quality check it cannot: open every cited PR, commit, ticket or message you are unsure of and confirm it exists and says what the answer claims. Fix or downgrade claims that fail.

## Step 6. Present

Present the answer. You may lightly edit for clarity or add context from the conversation; **do not rewrite the confidence language**. Keep Sources Consulted as one line per category, including the empty, skipped and unavailable ones with the reason.

If the question is a precursor to changing this code, end with a Preserve / Change / Avoid / Risk constraint set for planning the change.

## Common failure modes

- **Recency bias.** The most recent commit is not authoritative. The current shape is often an accretion of earlier decisions. Trace back.
- **Confirming the user's guess.** A hypothesis in the question is a candidate, not a conclusion.
- **Code as evidence of its own intent.** It is mechanics, not motivation.

## Reference files

- `references/epistemics.md`: confidence tiers and phrasing. The synthesis frame and you follow it.
- `references/investigator-prompt.md`: evidence-extraction task for frames over long source documents.
- `references/source-playbook.md`: index of the per-category playbooks in `references/sources/`.
- `references/synthesizer-prompt.md`: the synthesis frame's task, including the output format.
