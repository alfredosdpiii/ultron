# Ultron rollout and rollback

Ultron installs as its own command (`ultron`) with its own profile (`~/.ultron/agent`). Stock Pi (`pi`, profile `~/.pi/agent`) is never modified or redirected, so rolling back is always "stop using `ultron`".

## Before relying on Ultron day to day

1. **Gate.** `npm run gate` must pass: check, all A01-A46 acceptance rows, and a current quality comparison (`npm run eval:quality`) within the frozen thresholds.
2. **Back up the profile.** `ultron migrate backup` writes a timestamped, hash-manifested copy of `~/.ultron/agent` (not sessions) to `~/.ultron/backups/` with owner-only permissions. Credentials are copied, never printed.
3. **Record versions.** Note `ultron --version`, the git commit of this repository (`git rev-parse HEAD`), and `pi --version` for the preserved stock Pi.
4. **Bring sessions over explicitly.** `ultron migrate import-pi <pi-session.jsonl>` imports a Pi session; the Pi file is never changed. Importing the same session twice is refused.
5. **Register each extension and skill once.** Ultron loads from `~/.ultron/agent` plus project `.pi/` resources; do not also copy the same package into both places.

## Cutover

1. Quiesce managed work: `/agents` shows running tasks and background jobs; stop what should not continue (`agents.cancel`, `background.stop`) or let it finish. Paused schedules and goals stay paused.
2. Reconcile spend and effects: `agents.status` usage shows reserved and settled calls; confirm no call is left `unknown` without a follow-up.
3. Checkpoint: RLM kernel state is snapshotted automatically on eviction; nothing else needs a manual checkpoint.
4. Start using `ultron`. Keep `pi` installed.

## Rollback

1. Stop `ultron` sessions (idle workers exit on their own; busy workers finish their work first).
2. Restore the previous profile if it was changed: `ultron migrate restore <backup-dir>` verifies every file hash before overwriting.
3. Go back to stock `pi` with its own profile. To carry a conversation back: `ultron migrate export-pi <native-session.jsonl> <out.jsonl>` writes a Pi session file.
4. Ultron's task journals, artifacts, experiments, and refinement history stay on disk as read-only evidence. Do not feed Ultron RLM snapshots to anything else, and do not try to rewind external effects.
5. Refinements are rolled back by version pointer (`refinements.rollback`), never by deleting history. Memory already written to Hindsight needs an explicit `memory.forget` or correction if it should not persist.

## Updating Ultron

Pull, run `npm run gate`, then `npm run build:offline`; the linked `ultron` command picks up the new bundle. Re-run `npm run eval:quality` when runtime behavior changes, and `npm run acceptance:lock` only after reviewing changes to acceptance evidence.
