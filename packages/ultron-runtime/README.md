# Ultron runtime compatibility package

This directory contains the old local host services copied from the pre-fork runtime. It is a reference and compatibility package, not the Ultron authority.

The session worker owns native RLM and typed-agent execution in `packages/coding-agent/src/ultron/rlm/`. The copied modules remain here so their behavior and tests are available during the migration. They are not imported by the Ultron CLI and they do not read `~/.pi/agent`.

The source list and SHA-256 values are in `docs/source-manifest.json`. No Pi credentials, sessions, caches, or user configuration were copied into this package.
