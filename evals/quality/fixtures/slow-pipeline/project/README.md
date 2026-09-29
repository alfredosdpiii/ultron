# order-sync

A client that uploads order records to the staging order service (`client/`: settings in `client/config.py`,
batching in `client/uploader.py`) and the ledger that computes account balances from order events (`ledger/`).

- Unit tests: `python3 -m unittest discover -s tests/unit` (fast).
- `ops/` holds the release checks; they talk to (simulated) remote systems and are slow. Each prints progress and
  ends with one summary line; every run is logged under `.ops/`. Do not edit anything under `ops/`.
  - `./ops/provision.sh` (about 35 s): provisions a fresh staging tenant and prints its id and batch limit
    (`provision: tenant t-... ready, max_batch=N [run ...]`).
  - `./ops/loadtest.sh` (about 2.5 minutes): load test of `client/` against the tenant named in `client/config.py`.
    Ends with a line starting with `loadtest:`.
  - `./ops/snapshot.sh` (about 2 minutes): takes a snapshot of the production order events and balances and prints
    its id (`snapshot: s-... captured, ...`).
  - `./ops/replay.sh <snapshot-id>` (about 45 s): replays a snapshot through `ledger/` and compares every account's
    closing balance with production's. Ends with a line starting with `replay:`.
