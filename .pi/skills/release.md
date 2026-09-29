---
name: release
description: Prepare, verify, and publish an Ultron release (GitHub release tarball and the ultron-agent npm package).
---

# Releasing Ultron

Run commands from the repository root. All packages share one version.

1. **Verify** on the release commit:
   ```bash
   npm run build:offline && npm run check && ./test.sh && npm run test:scripts
   npm run test:acceptance && npm run test:acceptance:mutation
   ```
   Every acceptance row (A01-A56) must pass. Commit the refreshed `acceptance/report.*` and `acceptance/mutation.json`.

2. **Bump the version** and regenerate the locks:
   ```bash
   npm run version:patch
   node scripts/generate-coding-agent-shrinkwrap.mjs && node scripts/generate-coding-agent-install-lock.mjs
   git add -A packages package-lock.json
   PI_ALLOW_LOCKFILE_CHANGE=1 git commit -m "Release X.Y.Z: <summary>"
   ```

3. **Pack**: `npm run build:offline && node scripts/pack-release.mjs` writes `dist-release/ultron-X.Y.Z.tgz` (npm name `ultron-agent`, bin `ultron`).

4. **Scan before pushing**: the diff since `origin/main` and the unpacked tarball must contain no home-directory paths, API keys or tokens.
   ```bash
   node scripts/secret-scan.mjs --diff origin/main...HEAD
   node scripts/secret-scan.mjs --tarball dist-release/ultron-X.Y.Z.tgz
   npm run scan:secrets    # the whole tracked tree, as CI runs it
   ```
   Each exits 1 and prints `path:line:col kind preview` (values redacted) on a finding. `pack-release.mjs` already scans the staged package and refuses to write the tarball on a finding (`--skip-secret-scan` only to investigate). Remove a real value; a deliberate fake in a test fixture goes in `.secret-scan-allow` (path glob and rule ids). The rules are `packages/coding-agent/src/ultron/rlm/secret-patterns.json`, the same ones that mask secrets in REPL output.

5. **Push and publish**:
   ```bash
   git push origin main
   cd dist-release && cp ultron-X.Y.Z.tgz ultron.tgz && sha256sum ultron-X.Y.Z.tgz ultron.tgz > SHA256SUMS
   gh release create vX.Y.Z ultron-X.Y.Z.tgz ultron.tgz SHA256SUMS --target main --title "Ultron X.Y.Z" --notes "..."
   ```
   Then publish to npm from a logged-in terminal: `npm run publish:npm -- --no-build` (add `--otp <code>` for two-factor accounts, or run `npm publish ./dist-release/ultron-X.Y.Z.tgz --access public` to use the browser flow).

6. **Smoke test** the published package: `npm install -g --prefix "$(mktemp -d)" ultron-agent`, then run its `bin/ultron --version`.
