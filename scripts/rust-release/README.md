# Ultron

Ultron is a terminal coding agent whose model has one tool: a persistent Python REPL.

This archive holds a self-contained `ultron` binary. It needs no Node.js. The model's REPL runs on Python 3:
`ULTRON_PYTHON` if set, else `/usr/bin/python3` on Linux, else `python3` on `PATH`.

## Install

    sha256sum -c ultron-<version>-<target>.tar.gz.sha256
    tar -xzf ultron-<version>-<target>.tar.gz
    ./ultron-<version>-<target>/install.sh              # into ~/.local/bin
    ./ultron-<version>-<target>/install.sh --prefix /opt/ultron

Then run `ultron setup` to pick a provider and model, or `ultron --help`.

## Files it writes

The first time a run needs them (a session, `--list-models`; never `--version` or `--help`), the binary installs its runtime files (the Python REPL runtime, bundled skills, docs and changelog)
into `~/.ultron/agent/runtime/<version>-<hash>/` (under `$ULTRON_CODING_AGENT_DIR` when that is set). Each
release gets its own directory; older ones are removed automatically once no running ultron process uses them.

## Licenses

MIT; see LICENSE. Third-party notices: NOTICE, THIRD_PARTY_LICENSES.txt and licenses/.
