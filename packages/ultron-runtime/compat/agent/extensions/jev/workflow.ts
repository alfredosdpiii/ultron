export type WorkflowNode<TInput = unknown> = {
  id: string;
  dependsOn?: string[];
  run: (input: TInput, results: ReadonlyMap<string, unknown>, signal?: AbortSignal) => Promise<unknown> | unknown;
};

export type WorkflowOptions = {
  signal?: AbortSignal;
  onEvent?: (event: { type: "start" | "finish"; node: string }) => void;
};

export async function runWorkflowGraph<TInput>(
  nodes: readonly WorkflowNode<TInput>[],
  input: TInput,
  options: WorkflowOptions = {},
): Promise<ReadonlyMap<string, unknown>> {
  const byId = new Map(nodes.map((node) => [node.id, node]));
  if (byId.size !== nodes.length) throw new Error("Workflow graph contains duplicate node IDs");
  for (const node of nodes) {
    for (const dependency of node.dependsOn ?? []) {
      if (!byId.has(dependency)) throw new Error(`Workflow node ${node.id} depends on unknown node ${dependency}`);
    }
  }

  const results = new Map<string, unknown>();
  const pending = new Set(byId.keys());
  while (pending.size > 0) {
    if (options.signal?.aborted) throw new DOMException("Workflow aborted", "AbortError");
    const ready = [...pending].filter((id) => (byId.get(id)?.dependsOn ?? []).every((dependency) => results.has(dependency)));
    if (ready.length === 0) throw new Error("Workflow graph contains a cycle");
    ready.forEach((id) => options.onEvent?.({ type: "start", node: id }));
    const completed = await Promise.all(
      ready.map(async (id) => [id, await byId.get(id)!.run(input, results, options.signal)] as const),
    );
    for (const [id, result] of completed) {
      results.set(id, result);
      pending.delete(id);
      options.onEvent?.({ type: "finish", node: id });
    }
  }
  return results;
}
