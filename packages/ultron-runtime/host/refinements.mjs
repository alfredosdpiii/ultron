import { closeSync, fchmodSync, fsyncSync, mkdirSync, openSync, readFileSync, renameSync, unlinkSync, writeFileSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { randomUUID } from 'node:crypto';

const kinds = new Set(['observation', 'instruction', 'skill', 'agent']);
const protectedNames = new Set(['agents.md', 'security', 'policy']);
const own = (value, key) => Object.hasOwn(value, key);
const object = value => value !== null && typeof value === 'object' && !Array.isArray(value);

function identity(kind, target) {
  if (!kinds.has(kind)) throw new TypeError('Invalid refinement kind');
  // A namespace is mandatory and must match the kind. IDs never resolve to files.
  if (typeof target !== 'string' || !/^[a-z]+:[a-zA-Z0-9_-]+(?:\.[a-zA-Z0-9_-]+)*$/.test(target)) {
    throw new TypeError('Target must be an explicit namespace:name ID, not a path');
  }
  const [namespace, name] = target.split(':');
  if (namespace !== kind || protectedNames.has(name.toLowerCase())) throw new Error('Forbidden refinement target');
}

// Reject lossy JSON conversions, executable values, accessors, and cycles.
function jsonCopy(value) {
  const ancestors = new Set();
  function check(item) {
    if (item === null || typeof item === 'string' || typeof item === 'boolean') return;
    if (typeof item === 'number' && Number.isFinite(item)) return;
    if (typeof item !== 'object' || ancestors.has(item)) throw new TypeError('Expected finite, acyclic JSON data');
    const array = Array.isArray(item);
    if (!array && ![Object.prototype, null].includes(Object.getPrototypeOf(item))) throw new TypeError('Expected plain JSON data');
    ancestors.add(item);
    const keys = Reflect.ownKeys(item).filter(key => !(array && key === 'length'));
    if (array && keys.length !== item.length) throw new TypeError('Sparse arrays are not JSON data');
    for (const key of keys) {
      const descriptor = Object.getOwnPropertyDescriptor(item, key);
      if (typeof key !== 'string' || !descriptor.enumerable || !own(descriptor, 'value') ||
          array && (!/^(0|[1-9][0-9]*)$/.test(key) || Number(key) >= item.length)) {
        throw new TypeError('Expected plain JSON properties');
      }
      check(descriptor.value);
    }
    ancestors.delete(item);
  }
  check(value);
  return JSON.parse(JSON.stringify(value));
}

function nonempty(value) {
  if (value === null) return false;
  if (typeof value === 'string') return value.trim().length > 0;
  if (typeof value === 'object') return Object.values(value).some(nonempty);
  return true;
}

function fields(value, required, optional = []) {
  if (!object(value) || required.some(key => !own(value, key)) ||
      Object.keys(value).some(key => !required.includes(key) && !optional.includes(key))) {
    throw new TypeError('Malformed refinement fields');
  }
}

function proposal(input) {
  const value = jsonCopy(input);
  fields(value, ['kind', 'target', 'baseVersion', 'content'], ['evidence', 'scope']);
  const { kind, target, baseVersion, content, evidence = [], scope = 'session' } = value;
  identity(kind, target);
  if (!Number.isSafeInteger(baseVersion) || baseVersion < 0) throw new TypeError('baseVersion must be a nonnegative safe integer');
  if (!(typeof content === 'string' || content !== null && typeof content === 'object') || !nonempty(content)) {
    throw new TypeError('Content must be nonempty JSON text, an object, or an array');
  }
  if (!(evidence === null || typeof evidence === 'string' || typeof evidence === 'object')) throw new TypeError('Evidence must be JSON text, an object, or an array');
  if (typeof scope !== 'string' || !scope.trim()) throw new TypeError('Scope must be a nonempty string');
  return { kind, target, baseVersion, content, evidence, scope };
}

function lookup(records, id) {
  if (typeof id !== 'string' || !id) throw new TypeError('Invalid refinement ID');
  const record = records.find(record => record.id === id);
  if (!record) throw new Error(`Unknown refinement: ${id}`);
  return record;
}

const active = (records, kind, target) => records.find(record => record.kind === kind && record.target === target && record.state === 'active') ?? null;
function transition(record, state, event) {
  record.state = state;
  record.history.push({ state, at: event.at, cause: event.id });
}

// Replay uses the same state transitions as writes, but never runs schema callbacks.
function apply(records, event) {
  fields(event, ['action', 'id', 'at'], event.action === 'propose' ? ['proposal'] : []);
  if (typeof event.id !== 'string' || !event.id || typeof event.at !== 'string' ||
      !Number.isFinite(Date.parse(event.at))) throw new TypeError('Malformed refinement event');
  if (event.action === 'propose') {
    const data = proposal(event.proposal);
    if (records.some(record => record.id === event.id)) throw new Error('Duplicate refinement ID');
    const record = { id: event.id, ...data, version: null, previousId: null, state: 'proposed', history: [] };
    transition(record, 'proposed', event);
    records.push(record);
    return record;
  }
  const record = lookup(records, event.id);
  if (event.action === 'activate') {
    if (record.state !== 'proposed') throw new Error('Only a proposed refinement can be activated');
    identity(record.kind, record.target);
    if (!nonempty(record.evidence)) throw new Error('Activation requires nonempty evidence');
    const previous = active(records, record.kind, record.target);
    if (record.baseVersion !== (previous?.version ?? 0)) throw new Error('Stale base version');
    let version = 0;
    for (const item of records) {
      if (item.kind === record.kind && item.target === record.target) version = Math.max(version, item.version ?? 0);
    }
    if (!Number.isSafeInteger(version + 1)) throw new Error('Version exhausted');
    record.version = version + 1;
    record.previousId = previous?.id ?? null;
    if (previous) transition(previous, 'superseded', event);
    transition(record, 'active', event);
  } else if (event.action === 'reject') {
    if (record.state !== 'proposed') throw new Error('Only a proposed refinement can be rejected');
    transition(record, 'rejected', event);
  } else if (event.action === 'rollback') {
    if (active(records, record.kind, record.target)?.id !== record.id) throw new Error('Only the latest active refinement can be rolled back');
    transition(record, 'rolled_back', event);
    if (record.previousId !== null) transition(lookup(records, record.previousId), 'active', event);
  } else throw new Error('Unknown refinement action');
  return record;
}

/**
 * Synchronous, single-owner local ledger in directory/refinements.json.
 * No multiwriter safety: use one live instance per directory.
 *
 * Targets are <kind>:<name> IDs, e.g. skill:review or agent:security-reviewer.
 * File namespaces, paths, and the names AGENTS.md/security/policy are forbidden.
 * Scope is a metadata label, not a separate version namespace or a sandbox.
 *
 * Proposals have state 'proposed' and version null. baseVersion is required:
 * use current(kind, target)?.version ?? 0. Activation assigns a monotonically
 * increasing per-target version, with no approval prompt. current returns an
 * active record or null. Superseded, rejected, and rolled-back records remain
 * in list/get, including every state transition and the prior activation ID.
 * Only the current activation can be rolled back; repeated rollback can undo
 * its restored predecessors. Neither rejected nor rolled-back proposals retry.
 *
 * Content and evidence are inert JSON data. Missing/empty evidence can be
 * proposed but not activated. Evidence presence does not establish correctness.
 * Optional validate(kind, content) runs at activation with a defensive copy;
 * it must synchronously return true/undefined, return false, or throw. Reopening
 * does not rerun callbacks or execute definitions. Supply the validator again
 * on restart to validate future activations against your agent schema.
 */
export class RefinementStore {
  #directory;
  #path;
  #validate;
  #events = [];
  #records = [];
  #changing = false;
  #uncertain = false;

  constructor(directory, options = {}) {
    if (typeof directory !== 'string' || !directory.trim()) throw new TypeError('Directory must be a nonempty string');
    fields(options, [], ['validate']);
    if (options.validate !== undefined && typeof options.validate !== 'function') throw new TypeError('validate must be a function');
    this.#directory = resolve(directory);
    this.#path = join(this.#directory, 'refinements.json');
    this.#validate = options.validate;
    mkdirSync(this.#directory, { recursive: true, mode: 0o700 });
    let text;
    try { text = readFileSync(this.#path, 'utf8'); }
    catch (error) { if (error.code === 'ENOENT') return; throw error; }
    try {
      const document = JSON.parse(text);
      fields(document, ['formatVersion', 'events']);
      if (document.formatVersion !== 1 || !Array.isArray(document.events)) throw new Error('Unsupported ledger format');
      for (const event of document.events) apply(this.#records, event);
      this.#events = document.events;
    } catch (error) {
      throw new Error(`Invalid refinement ledger: ${error.message}`, { cause: error });
    }
  }

  #ready() {
    if (this.#uncertain) throw new Error('Ledger durability is uncertain; reopen the store before use');
  }

  list() { this.#ready(); return structuredClone(this.#records); }
  get(id) { this.#ready(); return structuredClone(lookup(this.#records, id)); }
  current(kind, target) {
    this.#ready(); identity(kind, target);
    return structuredClone(active(this.#records, kind, target));
  }
  propose(input) { return this.#commit({ action: 'propose', id: randomUUID(), proposal: proposal(input) }); }
  activate(id) { return this.#commit({ action: 'activate', id }); }
  reject(id) { return this.#commit({ action: 'reject', id }); }
  rollback(id) { return this.#commit({ action: 'rollback', id }); }

  #commit(fields) {
    this.#ready();
    if (this.#changing) throw new Error('Reentrant ledger mutation is not supported');
    this.#changing = true;
    try {
      const event = { ...fields, at: new Date().toISOString() };
      const records = structuredClone(this.#records);
      const result = apply(records, event);
      if (event.action === 'activate' && this.#validate) {
        const valid = this.#validate(result.kind, structuredClone(result.content));
        if (valid && typeof valid.then === 'function') {
          Promise.resolve(valid).catch(() => {});
          throw new TypeError('validate must be synchronous');
        }
        if (valid !== undefined && valid !== true) throw new Error('Refinement content validation failed');
      }
      const events = [...this.#events, event];
      this.#persist(events, records);
      return structuredClone(result);
    } finally { this.#changing = false; }
  }

  #persist(events, records) {
    const temporary = join(this.#directory, `.refinements-${randomUUID()}.tmp`);
    let file, directory, created = false, replaced = false;
    try {
      directory = openSync(this.#directory, 'r');
      file = openSync(temporary, 'wx', 0o600);
      created = true;
      fchmodSync(file, 0o600);
      writeFileSync(file, JSON.stringify({ formatVersion: 1, events }) + '\n', 'utf8');
      fsyncSync(file);
      closeSync(file); file = undefined;
      renameSync(temporary, this.#path);
      replaced = true;
      fsyncSync(directory);
      this.#events = events;
      this.#records = records;
    } catch (error) {
      // After replacement, a failed directory sync has an ambiguous durability
      // outcome. Do not let this instance serve or overwrite its old snapshot.
      if (replaced) this.#uncertain = true;
      throw error;
    } finally {
      try { if (file !== undefined) closeSync(file); }
      finally {
        try { if (directory !== undefined) closeSync(directory); }
        finally {
          if (created && !replaced) {
            try { unlinkSync(temporary); }
            catch (error) { if (error.code !== 'ENOENT') throw error; }
          }
        }
      }
    }
  }
}
