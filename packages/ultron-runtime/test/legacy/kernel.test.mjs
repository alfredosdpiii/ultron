import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, writeFileSync, chmodSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { createJiti } from 'jiti';

const jiti = createJiti(import.meta.url);
const { RlmKernel } = await jiti.import('../../compat/agent/extensions/prime-rlm/kernel.ts');
const runtimePath = resolve(new URL('../../compat/agent/extensions/prime-rlm/runtime.py', import.meta.url).pathname);

function temp() { return mkdtempSync(join(tmpdir(), 'pi-kernel-')); }

test('kernel persists namespace and explicit snapshot restore', { timeout: 10000 }, async () => {
  const cwd = temp(); const snapshot = join(cwd, 'state.json');
  try {
    const first = new RlmKernel({ cwd, runtimePath, snapshotPath: snapshot }, () => ({}));
    assert.equal((await first.execute('value = 41\nvalue + 1')).result, '42');
    assert.equal((await first.snapshot()).status, 'ok');
    await first.shutdown();
    const second = new RlmKernel({ cwd, runtimePath }, () => ({}));
    assert.equal((await second.restore(snapshot)).status, 'ok');
    assert.equal((await second.execute('value + 1')).result, '42');
    await second.shutdown();
  } finally { rmSync(cwd, { recursive: true, force: true }); }
});

test('abort kills an infinite cell and permits a fresh kernel generation', { timeout: 10000 }, async () => {
  const cwd = temp();
  try {
    const kernel = new RlmKernel({ cwd, runtimePath }, () => ({}));
    const controller = new AbortController();
    const pending = kernel.execute('while True: pass', controller.signal);
    setTimeout(() => controller.abort(new Error('test abort')), 100);
    await assert.rejects(pending, /test abort|aborted|exited/i);
    const result = await kernel.execute('6 * 7');
    assert.equal(result.result, '42');
    await kernel.shutdown();
  } finally { rmSync(cwd, { recursive: true, force: true }); }
});

test('shutdown is idempotent and queued work does not restart the kernel', { timeout: 10000 }, async () => {
  const cwd = temp();
  try {
    const kernel = new RlmKernel({ cwd, runtimePath }, () => ({}));
    const first = kernel.execute('1 + 1');
    await kernel.shutdown();
    await assert.rejects(first, /shut down|exited|closed/i);
    await kernel.shutdown();
    await assert.rejects(kernel.execute('2 + 2'), /shut down/i);
  } finally { rmSync(cwd, { recursive: true, force: true }); }
});

test('startup with a missing runtime fails, and oversized raw frames are rejected', { timeout: 10000 }, async () => {
  const cwd = temp();
  try {
    const missing = new RlmKernel({ cwd, runtimePath: join(cwd, 'missing.py') }, () => ({}));
    await assert.rejects(missing.execute('1'), /exited|startup|stdout closed|ENOENT/i);
    await missing.shutdown();

    const noisy = join(cwd, 'noisy.py');
    writeFileSync(noisy, `import sys\nsys.stdout.write('{"event":"ready"}' + 'x' * (2*1024*1024) + '\\n')\nsys.stdout.flush()\n`);
    const framed = new RlmKernel({ cwd, runtimePath: noisy }, () => ({}));
    await assert.rejects(framed.execute('1'), /frame exceeds|exited|protocol/i);
    await framed.shutdown();
  } finally { rmSync(cwd, { recursive: true, force: true }); }
});
