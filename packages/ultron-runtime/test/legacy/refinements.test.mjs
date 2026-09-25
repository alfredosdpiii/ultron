import test from 'node:test';
import assert from 'node:assert/strict';
import { chmodSync, existsSync, mkdtempSync, mkdirSync, openSync, closeSync, readFileSync, readdirSync, renameSync, rmSync, statSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { spawnSync } from 'node:child_process';
import { RefinementStore } from '../../host/refinements.mjs';

function fixture(t, options) {
  const directory = mkdtempSync(join(tmpdir(), 'pi-refinements-'));
  t.after(() => rmSync(directory, { recursive: true, force: true }));
  return { directory, path: join(directory, 'refinements.json'), store: new RefinementStore(directory, options) };
}
const input = (overrides = {}) => ({
  kind: 'skill', target: 'skill:review', baseVersion: 0,
  content: 'Check boundary cases before suggesting a fix.', evidence: [{ test: 'boundary regression', outcome: 'passed' }],
  ...overrides,
});

test('proposals are durable but inactive; activation needs no approval and survives a process restart', t => {
  const { directory, store } = fixture(t);
  assert.deepEqual(store.list(), []);
  assert.equal(store.current('skill', 'skill:review'), null);
  const proposed = store.propose(input());
  assert.equal(proposed.state, 'proposed');
  assert.equal(proposed.version, null);
  assert.equal(proposed.previousId, null);
  assert.equal(proposed.scope, 'session');
  assert.equal(store.current('skill', 'skill:review'), null);
  assert.deepEqual(new RefinementStore(directory).get(proposed.id), proposed);

  const activated = store.activate(proposed.id);
  assert.equal(activated.state, 'active');
  assert.equal(activated.version, 1);
  assert.deepEqual(activated.history.map(entry => entry.state), ['proposed', 'active']);
  assert.deepEqual(store.current('skill', 'skill:review'), activated);
  const child = spawnSync(process.execPath, ['--input-type=module', '-e', `
    import { RefinementStore } from ${JSON.stringify(new URL('../../host/refinements.mjs', import.meta.url).href)};
    const store = new RefinementStore(process.argv[1]);
    process.stdout.write(JSON.stringify({ records: store.list(), current: store.current('skill', 'skill:review') }));
  `, directory], { encoding: 'utf8', timeout: 10000 });
  assert.equal(child.error, undefined);
  assert.equal(child.status, 0, child.stderr);
  assert.deepEqual(JSON.parse(child.stdout), { records: [activated], current: activated });
});

test('a competing activation makes a proposal stale without losing it or changing current', t => {
  const { directory, path, store } = fixture(t);
  const a = store.propose(input());
  const b = store.propose(input({ content: 'An alternative revision.' }));
  store.activate(a.id);
  const snapshot = readFileSync(path, 'utf8');
  assert.throws(() => store.activate(b.id), /Stale base version/);
  assert.equal(readFileSync(path, 'utf8'), snapshot);
  assert.equal(store.get(b.id).state, 'proposed');
  assert.equal(store.get(b.id).version, null);
  assert.equal(store.current('skill', 'skill:review').id, a.id);
  const reopened = new RefinementStore(directory);
  assert.throws(() => reopened.activate(b.id), /Stale base version/);
  const future = reopened.propose(input({ baseVersion: 999 }));
  assert.throws(() => reopened.activate(future.id), /Stale base version/);
  assert.equal(reopened.current('skill', 'skill:review').id, a.id);
});

test('rollback restores only the current target predecessor, retaining history and never reusing versions', t => {
  const { directory, store } = fixture(t);
  const first = store.activate(store.propose(input()).id);
  const second = store.activate(store.propose(input({ baseVersion: first.version, content: 'Second revision.' })).id);
  const unrelated = store.activate(store.propose(input({ kind: 'agent', target: 'agent:review', content: { instructions: 'Review.' } })).id);
  assert.equal(second.version, 2);
  assert.equal(second.previousId, first.id);
  assert.equal(store.get(first.id).state, 'superseded');
  assert.throws(() => store.rollback(first.id), /latest active/);
  assert.equal(store.current('skill', 'skill:review').id, second.id);

  const reopened = new RefinementStore(directory);
  assert.equal(reopened.rollback(second.id).state, 'rolled_back');
  assert.equal(reopened.current('skill', 'skill:review').id, first.id);
  assert.equal(reopened.current('agent', 'agent:review').id, unrelated.id);
  assert.deepEqual(reopened.get(first.id).history.map(entry => entry.state), ['proposed', 'active', 'superseded', 'active']);
  assert.equal(reopened.get(first.id).history.at(-1).cause, second.id);
  assert.throws(() => reopened.rollback(second.id), /latest active/);
  assert.throws(() => reopened.activate(second.id), /Only a proposed/);

  const third = reopened.activate(reopened.propose(input({ baseVersion: 1, content: 'Third revision after rollback.' })).id);
  assert.equal(third.version, 3);
  assert.equal(third.previousId, first.id);
  assert.equal(new RefinementStore(directory).current('skill', 'skill:review').id, third.id);
  reopened.rollback(third.id);
  reopened.rollback(first.id);
  assert.equal(reopened.current('skill', 'skill:review'), null);
  const fourth = reopened.activate(reopened.propose(input({ content: 'Fourth revision after clearing the pointer.' })).id);
  assert.equal(fourth.version, 4);
  assert.equal(fourth.previousId, null);
  assert.equal(reopened.list().length, 5);
  assert.equal(reopened.get(second.id).content, second.content);
  assert.deepEqual(reopened.get(second.id).evidence, second.evidence);
  assert.deepEqual(new RefinementStore(directory).list(), reopened.list());
});

test('reject retains a proposal and evidence but cannot reject or reactivate activation history', t => {
  const { directory, store } = fixture(t);
  const active = store.activate(store.propose(input()).id);
  const candidate = store.propose(input({ baseVersion: 1, scope: 'global' }));
  const rejected = store.reject(candidate.id);
  assert.equal(rejected.state, 'rejected');
  assert.equal(rejected.version, null);
  assert.equal(rejected.scope, 'global');
  assert.deepEqual(rejected.evidence, candidate.evidence);
  assert.deepEqual(rejected.history.map(entry => entry.state), ['proposed', 'rejected']);
  assert.throws(() => store.reject(candidate.id), /Only a proposed/);
  assert.throws(() => store.activate(candidate.id), /Only a proposed/);
  assert.throws(() => store.rollback(candidate.id), /latest active/);
  assert.throws(() => store.reject(active.id), /Only a proposed/);
  assert.throws(() => store.activate(active.id), /Only a proposed/);
  assert.equal(store.current('skill', 'skill:review').id, active.id);
  assert.deepEqual(new RefinementStore(directory).get(candidate.id), rejected);
});

test('all allowed kinds activate independently; scopes are metadata, not a stale-version bypass', t => {
  const { store } = fixture(t);
  for (const kind of ['observation', 'instruction', 'skill', 'agent']) {
    const proposed = store.propose(input({ kind, target: `${kind}:review`, scope: 'project' }));
    assert.equal(store.current(kind, `${kind}:review`), null);
    assert.equal(store.activate(proposed.id).version, 1);
    assert.equal(store.current(kind, `${kind}:review`).scope, 'project');
  }
  const global = store.propose(input({ scope: 'global' }));
  assert.throws(() => store.activate(global.id), /Stale base version/);
  const session = store.propose(input({ baseVersion: 1 }));
  assert.equal(store.activate(session.id).version, 2);
});

test('explicit target IDs reject paths and protected namespaces/names without substring blocking', t => {
  const { store, directory } = fixture(t);
  const forbidden = [
    'AGENTS.md', 'security', 'policy', '/tmp/AGENTS.md', '../AGENTS.md', './review', 'C:\\AGENTS.md',
    'file:AGENTS.md', 'path:review', 'security:rules', 'policy:rules', 'system:policy',
    'instruction:review', 'skill:AGENTS.md', 'skill:AgEnTs.Md', 'skill:SECURITY', 'skill:policy',
    'skill:../review', 'skill:sub/review', 'skill:sub\\review', 'skill:%2e%2e%2freview',
    'skill:review:policy', 'skill:review\0', ' skill:review', 'skill:review ', 'skill:', 'skill:..',
  ];
  for (const target of forbidden) {
    assert.throws(() => store.propose(input({ target })), /target|namespace/i, target);
    assert.throws(() => store.current('skill', target), /target|namespace/i, target);
  }
  for (const kind of ['observation', 'instruction', 'skill', 'agent']) {
    for (const name of ['AGENTS.md', 'security', 'policy']) {
      assert.throws(() => store.propose(input({ kind, target: `${kind}:${name}` })), /Forbidden/);
    }
  }
  assert.deepEqual(store.list(), []);
  for (const name of ['security-reviewer', 'policy-review', 'agents-md-notes', 'review.v2']) {
    const record = store.propose(input({ target: `skill:${name}` }));
    assert.equal(store.activate(record.id).state, 'active');
  }
  assert.deepEqual(readdirSync(directory), ['refinements.json']);
});

test('empty evidence can be recorded but never activated, including after restart', t => {
  const { directory, store } = fixture(t);
  for (const evidence of [null, '', ' \n', [], {}, [null, ' ', {}], { note: ' ' }]) {
    const proposed = store.propose(input({ evidence }));
    assert.throws(() => store.activate(proposed.id), /nonempty evidence/);
    assert.equal(store.get(proposed.id).state, 'proposed');
    assert.throws(() => new RefinementStore(directory).activate(proposed.id), /nonempty evidence/);
  }
  const missing = input(); delete missing.evidence;
  assert.throws(() => store.activate(store.propose(missing).id), /nonempty evidence/);
  assert.equal(store.current('skill', 'skill:review'), null);
  for (const [index, evidence] of ['test:42', ['held-out run'], { test: 'regression', passed: false }].entries()) {
    const proposed = store.propose(input({ target: `skill:evidence-${index}`, evidence }));
    // The ledger records evidence, not a claim that a test passed or a lesson improved behavior.
    assert.equal(store.activate(proposed.id).state, 'active');
  }
});

test('malformed input and non-JSON data fail without changing durable state or executing accessors', t => {
  const { directory, path, store } = fixture(t);
  store.propose(input());
  const before = readFileSync(path, 'utf8');
  const cycle = {}; cycle.self = cycle;
  const sparse = new Array(2);
  let executed = false;
  const accessor = { get text() { executed = true; return 'do not execute'; } };
  const executable = { toJSON() { executed = true; return 'do not execute'; } };
  const malformed = [
    undefined, null, [], 'bad', {},
    ...['script', 'policy', 'security', 'AGENT', '', null, 4].map(kind => input({ kind })),
    ...[undefined, null, '', 5, {}, []].map(target => input({ target })),
    ...[undefined, null, -1, 0.5, '0', NaN, Infinity, Number.MAX_SAFE_INTEGER + 1].map(baseVersion => input({ baseVersion })),
    ...[undefined, null, '', ' ', {}, [], false, 7, NaN, Infinity, 1n, () => {}, Symbol('code'),
      new Date(), new Map(), cycle, sparse, accessor, executable, { nested: undefined }, { nested: () => {} }].map(content => input({ content })),
    ...[true, 1, undefined, { score: NaN }].map(evidence => input({ evidence })),
    ...[null, '', ' ', 1, {}, []].map(scope => input({ scope })),
    input({ state: 'active' }), input({ version: 99 }), input({ previousId: 'fake' }),
  ];
  for (const candidate of malformed) assert.throws(() => store.propose(candidate));
  assert.equal(executed, false);
  for (const id of [undefined, null, '', {}, 1, 'unknown']) {
    for (const method of ['get', 'activate', 'reject', 'rollback']) assert.throws(() => store[method](id), /refinement/i);
  }
  for (const invalid of [undefined, null, '', ' ', 5, {}]) assert.throws(() => new RefinementStore(invalid), /Directory/);
  for (const options of [null, [], { validate: true }, { approval: true }]) assert.throws(() => new RefinementStore(directory, options));
  assert.equal(readFileSync(path, 'utf8'), before);
  assert.deepEqual(new RefinementStore(directory).list(), store.list());
});

test('the injected validator gates activation, preserves proposals on failure, and cannot rewrite content', t => {
  const validate = (kind, content) => {
    if (kind !== 'agent') return;
    if (typeof content.instructions !== 'string') throw new Error('Invalid agent schema: instructions required');
    const valid = content.strategy === 'deterministic';
    content.instructions = 'validator mutation must not persist';
    return valid;
  };
  const { directory, path, store } = fixture(t, { validate });
  const invalid = store.propose(input({ kind: 'agent', target: 'agent:review', content: { strategy: 'deterministic' } }));
  const before = readFileSync(path, 'utf8');
  assert.throws(() => store.activate(invalid.id), /Invalid agent schema/);
  assert.equal(readFileSync(path, 'utf8'), before);
  assert.equal(store.get(invalid.id).state, 'proposed');
  const falseResult = store.propose(input({ kind: 'agent', target: 'agent:review', content: { strategy: 'unknown', instructions: 'Review.' } }));
  assert.throws(() => store.activate(falseResult.id), /validation failed/);
  assert.equal(store.current('agent', 'agent:review'), null);
  const good = store.propose(input({ kind: 'agent', target: 'agent:review', content: { strategy: 'deterministic', instructions: 'Review.' } }));
  const reopened = new RefinementStore(directory, { validate });
  assert.equal(reopened.activate(good.id).content.instructions, 'Review.');
  const readOnly = new RefinementStore(directory, { validate() { throw new Error('must not run during replay'); } });
  assert.equal(readOnly.current('agent', 'agent:review').id, good.id);
  assert.equal(readOnly.get(invalid.id).state, 'proposed');
});

test('async validators cannot silently activate a definition', async t => {
  const { directory, path, store } = fixture(t, { validate: async () => { throw new Error('async schema failure'); } });
  const proposed = store.propose(input());
  const before = readFileSync(path, 'utf8');
  assert.throws(() => store.activate(proposed.id), /synchronous/);
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(readFileSync(path, 'utf8'), before);
  assert.equal(new RefinementStore(directory).get(proposed.id).state, 'proposed');
});

test('validator reentrancy cannot overwrite a nested commit', t => {
  let store;
  ({ store } = fixture(t, { validate() { store.propose(input({ target: 'skill:nested' })); } }));
  const proposed = store.propose(input());
  assert.throws(() => store.activate(proposed.id), /Reentrant/);
  assert.deepEqual(store.list(), [proposed]);
  assert.equal(store.current('skill', 'skill:review'), null);
});

test('input and returned records are defensive copies, including nested history and evidence', t => {
  const { directory, store } = fixture(t);
  const data = input({ content: { instructions: ['Review.'] } });
  const proposed = store.propose(data);
  data.content.instructions.push('outside mutation');
  data.evidence[0].outcome = 'changed';
  proposed.state = 'active';
  proposed.history.length = 0;
  proposed.content.instructions[0] = 'changed';
  assert.equal(store.get(proposed.id).state, 'proposed');
  const activated = store.activate(proposed.id);
  activated.content.instructions.push('changed');
  const current = store.current('skill', 'skill:review');
  current.evidence[0].outcome = 'changed';
  current.version = 999;
  store.list()[0].history.length = 0;
  store.get(proposed.id).content.instructions.length = 0;
  assert.deepEqual(store.get(proposed.id).content, { instructions: ['Review.'] });
  assert.equal(store.get(proposed.id).evidence[0].outcome, 'passed');
  assert.equal(store.current('skill', 'skill:review').version, 1);
  assert.equal(store.get(proposed.id).history.length, 2);
  assert.deepEqual(new RefinementStore(directory).list(), store.list());
});

test('JSON replacement uses mode 0600 and leaves readers of the old file on a complete snapshot', t => {
  const { directory, path, store } = fixture(t);
  const proposed = store.propose(input());
  assert.equal(statSync(path).mode & 0o777, 0o600);
  const old = openSync(path, 'r');
  try {
    const before = readFileSync(path, 'utf8');
    chmodSync(path, 0o644);
    store.activate(proposed.id);
    assert.equal(statSync(path).mode & 0o777, 0o600);
    assert.equal(readFileSync(old, 'utf8'), before);
    assert.doesNotThrow(() => JSON.parse(readFileSync(path, 'utf8')));
    assert.notEqual(readFileSync(path, 'utf8'), before);
    assert.deepEqual(readdirSync(directory), ['refinements.json']);
  } finally { closeSync(old); }
  store.rollback(proposed.id);
  assert.equal(statSync(path).mode & 0o777, 0o600);
});

test('a failed file replacement leaves the proposal inactive and cleans up temporary files', t => {
  const { directory, path, store } = fixture(t);
  const proposed = store.propose(input());
  const before = readFileSync(path, 'utf8');
  const backup = join(directory, 'saved.json');
  renameSync(path, backup);
  mkdirSync(path);
  try {
    assert.throws(() => store.activate(proposed.id));
    assert.equal(store.current('skill', 'skill:review'), null);
    assert.deepEqual(store.get(proposed.id), proposed);
    assert.deepEqual(readdirSync(directory).sort(), ['refinements.json', 'saved.json']);
  } finally {
    rmSync(path, { recursive: true });
    renameSync(backup, path);
  }
  assert.equal(readFileSync(path, 'utf8'), before);
  assert.deepEqual(new RefinementStore(directory).get(proposed.id), proposed);
  assert.equal(store.activate(proposed.id).version, 1);
});

test('malformed or semantically corrupt ledgers fail closed and are never overwritten', t => {
  const { directory, path, store } = fixture(t);
  const proposed = store.propose(input());
  store.activate(proposed.id);
  const good = readFileSync(path, 'utf8');
  const document = JSON.parse(good);
  const alter = change => { const copy = structuredClone(document); change(copy); return JSON.stringify(copy); };
  const corrupt = [
    '', '{', '{}', 'null', '[]', good.slice(0, -5),
    alter(doc => { doc.formatVersion = 99; }),
    alter(doc => { doc.events = {}; }),
    alter(doc => { doc.events[0].proposal.target = 'policy:rules'; }),
    alter(doc => { doc.events[0].proposal.evidence = []; }),
    alter(doc => { doc.events[0].proposal.baseVersion = 5; }),
    alter(doc => { doc.events[1].action = 'invented'; }),
    alter(doc => { doc.events[1].id = 'unknown'; }),
    alter(doc => { doc.events[0].at = 'not a date'; }),
    alter(doc => { doc.events.push(doc.events[0]); }),
    alter(doc => { doc.events.push(doc.events[1]); }),
  ];
  for (const text of corrupt) {
    writeFileSync(path, text);
    assert.throws(() => new RefinementStore(directory), /Invalid refinement ledger/);
    assert.equal(readFileSync(path, 'utf8'), text);
  }
  writeFileSync(path, good);
  assert.equal(new RefinementStore(directory).current('skill', 'skill:review').id, proposed.id);
});

test('source-like content and evidence are inert data; abandoned temporary files are not replayed', t => {
  const { directory, store } = fixture(t);
  const marker = join(directory, 'must-not-exist');
  const content = `require('node:fs').writeFileSync(${JSON.stringify(marker)}, 'executed')`;
  const proposed = store.propose(input({ kind: 'agent', target: 'agent:source', content, evidence: [content] }));
  store.activate(proposed.id);
  writeFileSync(join(directory, '.refinements-abandoned.tmp'), '{incomplete');
  const reopened = new RefinementStore(directory);
  assert.equal(reopened.current('agent', 'agent:source').content, content);
  assert.equal(existsSync(marker), false);
  const next = reopened.propose(input({ kind: 'agent', target: 'agent:source', content, baseVersion: 1 }));
  reopened.activate(next.id);
  reopened.rollback(next.id);
  assert.equal(existsSync(marker), false);
});
