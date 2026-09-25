# Ultron architecture

Ultron is an RLM-first fork of Pi. The fork starts from upstream Pi main and keeps the upstream package graph intact while the runtime split is built and tested.

## Product rules

- The root RLM is the primary execution model.
- Typed agent methods and optional workflow nodes use one task service.
- Permission prompts, risk blocking, capability enforcement, budget enforcement, completion gates, refinement approval, and mandatory sandboxing are off by default.
- Schema validation, cancellation, bounded protocol/output handling, and truthful `unverified` results remain active.
- Pi session state remains authoritative until the Ultron session service proves a full replacement across branch, fork, resume, and restart.

## Planned runtime layers

```text
Ultron session host
  transcript, branches, task journal, definitions, artifacts, usage
        |
Root RLM
  persistent Python, bounded previews, programmatic delegation
        |
Typed agents and optional workflows
        |
Jev decisions, Hindsight memory, versioned refinements
```

The first fork commit changes product identity and config paths only. Runtime changes land in separate commits with focused tests. The installed `pi` and `prime-agent` commands are not changed by this repository. Ultron does not read `~/.pi/agent` after the one-time copy. Do not run Ultron and stock Pi against the same live session.

## Upstream boundary

Base commit: `a8ed497713ee712b2ba5f27c2ec269d68657465b` from `upstream/main`, recorded on 2026-09-23.

Internal `@earendil-works/pi-*` package names remain temporarily unchanged to avoid breaking the monorepo dependency graph. The coding-agent product package is `@bryandlp/ultron-coding-agent`, its command is `ultron`, and its default config directory is `~/.ultron`. Ultron uses its own copied `~/.ultron/agent` settings, credentials, extensions, skills, models, and sessions.
