export class WorkflowService {
  constructor(tasks) { this.tasks = tasks; }
  validate(nodes) {
    if (!Array.isArray(nodes)) throw new TypeError('nodes must be an array');
    const names = new Set();
    for (const node of nodes) {
      if (!node.id || names.has(node.id)) throw new Error('Duplicate or missing node ID');
      names.add(node.id);
      if (!this.tasks.definitions.has(node.definition)) throw new Error(`Unknown definition: ${node.definition}`);
      if (node.inputFrom && !(node.dependsOn ?? []).includes(node.inputFrom)) throw new Error('inputFrom must name a dependency');
      if (!node.inputFrom && !this.tasks.definitions.get(node.definition).input(node.input)) throw new Error(`Invalid workflow input: ${node.id}`);
    }
    const pending = new Set(names), done = new Set();
    for (const node of nodes) for (const dep of node.dependsOn ?? []) if (!names.has(dep)) throw new Error(`Unknown dependency: ${dep}`);
    while (pending.size) {
      const ready = nodes.filter(n => pending.has(n.id) && (n.dependsOn ?? []).every(d => done.has(d)));
      if (!ready.length) throw new Error('Workflow contains a cycle');
      for (const node of ready) { done.add(node.id); pending.delete(node.id); }
    }
  }
  async run(nodes, options = {}) {
    this.validate(nodes);
    const controller = new AbortController();
    const abort = () => controller.abort();
    options.signal?.addEventListener('abort', abort, { once: true });
    if (options.signal?.aborted) abort();
    const results = new Map(), pending = new Set(nodes.map(n => n.id));
    try {
      while (pending.size) {
        controller.signal.throwIfAborted();
        const ready = nodes.filter(n => pending.has(n.id) && (n.dependsOn ?? []).every(d => results.has(d)));
        const outcomes = await Promise.all(ready.map(async node => {
          if ((node.dependsOn ?? []).some(d => results.get(d).status !== 'succeeded')) return [node.id, { status: 'skipped', reason: 'Dependency did not succeed' }];
          const input = node.inputFrom ? results.get(node.inputFrom)?.value : node.input;
          if (node.inputFrom && !(node.dependsOn ?? []).includes(node.inputFrom)) throw new Error('inputFrom must name a dependency');
          return [node.id, await this.tasks.invoke(node.definition, input, { ...options, signal: controller.signal, key: undefined })];
        }));
        for (const [id, result] of outcomes) { pending.delete(id); results.set(id, result); }
      }
      return Object.fromEntries(results);
    } catch (error) { controller.abort(); throw error; }
    finally { options.signal?.removeEventListener('abort', abort); }
  }
}
