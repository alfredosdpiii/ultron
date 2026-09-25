import { createHash, randomUUID } from 'node:crypto';
import Ajv from 'ajv';
import { authorize, resolveControls } from './policy.mjs';

function canonical(value) {
  if (Array.isArray(value)) return value.map(canonical);
  if (value && typeof value === 'object') return Object.fromEntries(Object.keys(value).sort().map(k => [k, canonical(value[k])]));
  return value;
}
const hash = value => createHash('sha256').update(JSON.stringify(canonical(value))).digest('hex');

export class TaskService {
  constructor({ store, adapters = {}, controls = {}, policyServices = {}, onDefinition = () => {} }) {
    this.store = store;
    this.onDefinition = onDefinition;
    this.adapters = adapters;
    this.controls = resolveControls(controls);
    this.policyServices = policyServices;
    this.definitions = new Map();
    this.live = new Map();
    this.ajv = new Ajv({ strict: true, allErrors: true });
  }
  register(definition) {
    const def = structuredClone(definition);
    if (!def.id || !def.version || !['deterministic', 'predict', 'rlm'].includes(def.strategy)) throw new Error('Invalid agent definition');
    const key = `${def.id}@${def.version}`;
    if (this.definitions.has(key)) {
      if (this.definitions.get(key).hash === hash(def)) return key;
      throw new Error(`Definition already registered: ${key}`);
    }
    if (def.maxRepairs !== undefined && (!Number.isInteger(def.maxRepairs) || def.maxRepairs < 0 || def.maxRepairs > 5)) throw new Error('maxRepairs must be between 0 and 5');
    const compiled = { def, hash: hash(def), input: this.ajv.compile(def.inputSchema), output: this.ajv.compile(def.outputSchema) };
    this.onDefinition(def);
    this.definitions.set(key, compiled);
    return key;
  }
  async spawn(definition, input, options = {}) {
    const item = this.definitions.get(definition);
    if (!item) throw new Error(`Unknown definition: ${definition}`);
    const data = structuredClone(input);
    if (!item.input(data)) throw new Error(`Invalid input: ${this.ajv.errorsText(item.input.errors)}`);
    if (options.signal?.aborted) throw new Error('Task cancelled before admission');
    const request = { definition, definitionHash: item.hash, input: data, parentId: options.parentId ?? null, instanceId: options.instanceId ?? null, branch: options.branch ?? 'root', model: options.model ?? item.def.model ?? null, executionProfile: options.executionProfile ?? 'trusted-local', capabilities: item.def.capabilities ?? [], grants: options.grants ?? [] };
    const adapter = this.adapters[item.def.strategy];
    if (!adapter) throw new Error(`Strategy unavailable: ${item.def.strategy}`);
    await authorize(this.controls, request, this.policyServices);
    const entry = this.store.admit(options.key ?? randomUUID(), hash(request), request);
    if (entry.created) {
      const controller = new AbortController();
      const abort = () => this.cancel(entry.id);
      options.signal?.addEventListener('abort', abort, { once: true });
      const state = { controller, promise: null };
      this.live.set(entry.id, state);
      state.promise = this.execute(entry.id, item, adapter, request, controller.signal).finally(() => {
        options.signal?.removeEventListener('abort', abort);
        this.live.delete(entry.id);
      });
      if (options.signal?.aborted) abort();
    }
    return this.handle(entry.id);
  }
  listDefinitions() { return [...this.definitions.entries()].map(([key, item]) => ({ key, ...structuredClone(item.def), hash: item.hash })); }
  list() { return this.store.list(); }
  handle(id) {
    return Object.freeze({ id, inspect: () => this.store.get(id), result: async () => {
      const pending = this.live.get(id)?.promise;
      if (pending && ['admitted', 'running'].includes(this.store.get(id).state)) {
        await Promise.race([pending, new Promise(resolve => {
          const signal = this.live.get(id)?.controller.signal;
          if (!signal || signal.aborted) return resolve();
          signal.addEventListener('abort', resolve, { once: true });
          pending.finally(() => { signal.removeEventListener('abort', resolve); resolve(); });
        })]);
      }
      const task = this.store.get(id);
      if (task.result === null) throw new Error('Task has no result; owner recovery required');
      return task.result;
    }, cancel: () => this.cancel(id) });
  }
  async execute(id, item, adapter, request, signal) {
    this.store.transition(id, 'admitted', 'running');
    let reservation;
    try {
      if (this.controls.budgetEnforcement && (!Number.isFinite(item.def.estimatedCost) || !Number.isFinite(item.def.budget))) throw new Error('Budget enforcement requires estimate and budget');
      let result, repair = null;
      for (let attempt = 0; attempt <= (item.def.maxRepairs ?? 0); attempt++) {
        signal.throwIfAborted();
        reservation = this.store.reserve('root', item.def.estimatedCost ?? 0, this.controls.budgetEnforcement ? item.def.budget : null);
        try {
          result = await adapter({ request, taskId: id, attempt, repair, definition: structuredClone(item.def), signal, invoke: (def, value, options = {}) => this.invoke(def, value, { ...options, parentId: id, branch: request.branch, signal }) });
          this.store.settle(reservation, result.cost ?? null);
        } catch (error) {
          this.store.settle(reservation, error.cost ?? null);
          if (error.name !== 'OutputValidationError' || attempt === (item.def.maxRepairs ?? 0)) throw error;
          repair = { error: error.message }; continue;
        }
        if (signal.aborted) return;
        if (item.output(result.value)) break;
        repair = { error: this.ajv.errorsText(item.output.errors) };
        if (attempt === (item.def.maxRepairs ?? 0)) throw new Error(`Invalid output: ${repair.error}`);
      }
      const verified = this.controls.completionGates && this.policyServices.verify
        ? await this.policyServices.verify({ request, value: result.value }) === true : false;
      if (this.controls.completionGates && !verified) throw new Error('Completion gate has not passed');
      this.store.transition(id, 'running', 'succeeded', { status: 'succeeded', value: result.value, verification: verified ? 'verified' : 'unverified' });
    } catch (error) {
      if (reservation) this.store.settle(reservation, null);
      this.store.transition(id, 'running', signal.aborted ? 'cancelled' : 'failed', { status: signal.aborted ? 'cancelled' : 'failed', error: String(error.message ?? error) });
    }
  }
  cancel(id) {
    const task = this.store.get(id);
    if (!['admitted', 'running'].includes(task.state)) return false;
    const changed = this.store.transition(id, task.state, 'cancelled', { status: 'cancelled' });
    this.live.get(id)?.controller.abort();
    for (const child of this.store.list()) if (child.request.parentId === id) this.cancel(child.id);
    return changed;
  }
  async invoke(definition, input, options = {}) { return (await this.spawn(definition, input, options)).result(); }
  async shutdown() {
    const pending = [...this.live.values()].map(x => x.promise);
    for (const id of this.live.keys()) this.cancel(id);
    await Promise.race([Promise.allSettled(pending), new Promise(resolve => { const timer = setTimeout(resolve, 3000); timer.unref?.(); })]);
  }
}
