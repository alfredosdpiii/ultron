import { appendFileSync, mkdirSync, readFileSync, existsSync } from 'node:fs';
import { dirname } from 'node:path';
import { randomUUID } from 'node:crypto';

const terminal = new Set(['succeeded', 'failed', 'cancelled', 'interrupted']);

/** Single-process extension task journal. Not a replacement for Pi session history. */
export class JournalStore {
  constructor(path) {
    this.path = path; this.tasks = new Map(); this.keys = new Map(); this.entries = []; this.usage = new Map();
    if (path) {
      mkdirSync(dirname(path), { recursive: true, mode: 0o700 });
      if (existsSync(path)) {
        const text = readFileSync(path, 'utf8');
        if (text && !text.endsWith('\n')) throw new Error('Incomplete task journal; refusing to append');
        for (const line of text.split('\n').filter(Boolean)) this.apply(JSON.parse(line));
      }
    }
  }
  apply(event) {
    if (event.seq !== this.entries.length + 1) throw new Error('Invalid journal sequence');
    if (event.kind === 'admitted') {
      const { id, key, fingerprint, request } = event;
      this.tasks.set(id, { id, key, fingerprint, request, state: 'admitted', result: null }); this.keys.set(key, id);
    } else if (event.kind === 'transition') {
      const task = this.tasks.get(event.id);
      if (!task || task.state !== event.expected) throw new Error('Invalid journal transition');
      task.state = event.next; task.result = event.result;
    } else if (event.kind === 'reserve') this.usage.set(event.id, { account: event.account, reserved: event.estimate, actual: null, state: 'reserved' });
    else if (event.kind === 'settle') Object.assign(this.usage.get(event.id), { actual: event.actual, state: event.actual === null ? 'unresolved' : 'settled' });
    else throw new Error('Unknown journal event');
    this.entries.push(event);
  }
  commit(fields) {
    const event = { ...fields, seq: this.entries.length + 1, timestamp: new Date().toISOString() };
    if (this.path) appendFileSync(this.path, JSON.stringify(event) + '\n', { mode: 0o600, flush: true });
    this.apply(event);
  }
  admit(key, fingerprint, request) {
    const old = this.keys.get(key);
    if (old) {
      if (this.tasks.get(old).fingerprint !== fingerprint) throw new Error('Idempotency key reused for a different task');
      return { ...this.get(old), created: false };
    }
    const id = randomUUID(); this.commit({ kind: 'admitted', id, key, fingerprint, request });
    return { ...this.get(id), created: true };
  }
  get(id) {
    const task = this.tasks.get(id); if (!task) throw new Error(`Unknown task: ${id}`);
    return structuredClone(task);
  }
  list() { return [...this.tasks.keys()].map(id => this.get(id)); }
  transition(id, expected, next, result = null) {
    if (terminal.has(expected) || !['admitted', 'running'].includes(expected) || !(terminal.has(next) || expected === 'admitted' && next === 'running')) throw new Error('Invalid lifecycle transition');
    if (this.get(id).state !== expected) return false;
    this.commit({ kind: 'transition', id, expected, next, result }); return true;
  }
  history(id) { return structuredClone(this.entries.filter(e => e.id === id)); }
  recover() {
    for (const task of this.list()) if (['admitted', 'running'].includes(task.state)) this.transition(task.id, task.state, 'interrupted', { status: 'interrupted', reason: 'Owner ended; task was not replayed' });
  }
  reserve(account, estimate, limit = null) {
    if (!Number.isFinite(estimate) || estimate < 0 || limit !== null && (!Number.isFinite(limit) || limit < 0)) throw new TypeError('Invalid usage reservation');
    if (limit !== null && this.total(account) + estimate > limit) throw new Error('Budget exhausted');
    const id = randomUUID(); this.commit({ kind: 'reserve', id, account, estimate }); return id;
  }
  settle(id, actual) {
    if (actual !== null && (!Number.isFinite(actual) || actual < 0)) throw new TypeError('Invalid usage');
    if (this.usage.get(id)?.state !== 'reserved') return false;
    this.commit({ kind: 'settle', id, actual }); return true;
  }
  total(account) { return [...this.usage.values()].filter(x => x.account === account).reduce((n, x) => n + (x.actual ?? x.reserved), 0); }
  accounting(account) { const rows = [...this.usage.values()].filter(x => x.account === account); return { total: this.total(account), unresolved: rows.filter(x => x.state !== 'settled').length }; }
  rebranch(visibleAnchors) {
    return this.list().filter(task => visibleAnchors.has(task.request.branch));
  }
  close() {}
}
