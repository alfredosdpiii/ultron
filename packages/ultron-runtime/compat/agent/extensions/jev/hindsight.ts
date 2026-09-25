import { randomUUID } from "node:crypto";

export type HindsightMemory = {
  content: string;
  context?: string;
  document_id?: string;
  metadata?: Record<string, string>;
  tags?: string[];
  timestamp?: string;
  update_mode?: "replace" | "append";
};

export type HindsightRecall = {
  id?: string;
  text?: string;
  type?: string;
  context?: string;
  occurred_start?: string;
  occurred_end?: string;
  [key: string]: unknown;
};

type RecallResponse = { results?: HindsightRecall[] };
type OperationResponse = { operation_id?: string; status?: string; success?: boolean; async?: boolean; operation_ids?: string[] };

export type HindsightDocument = { id?: string; original_text?: string | null; tags?: string[]; [key: string]: unknown };

export class HindsightClient {
  readonly baseUrl: string;
  readonly bankId: string;

  constructor(
    baseUrl = process.env.PI_HINDSIGHT_URL ?? "http://localhost:8888",
    bankId = process.env.PI_HINDSIGHT_BANK ?? "omp",
  ) {
    this.baseUrl = baseUrl.replace(/\/$/, "");
    this.bankId = bankId;
  }

  private async request<T>(path: string, init: RequestInit = {}, signal?: AbortSignal): Promise<T> {
    const configuredTimeout = Number(process.env.PI_HINDSIGHT_TIMEOUT_MS ?? 15000);
    const timeoutMs = Number.isFinite(configuredTimeout) && configuredTimeout > 0 ? configuredTimeout : 15000;
    const response = await fetch(`${this.baseUrl}${path}`, {
      ...init,
      signal: signal ?? AbortSignal.timeout(timeoutMs),
      headers: { "content-type": "application/json", ...(init.headers ?? {}) },
    });
    if (!response.ok) {
      throw new Error(`Hindsight ${response.status} ${response.statusText}: ${path}`);
    }
    return (await response.json()) as T;
  }

  async health(signal?: AbortSignal): Promise<boolean> {
    try {
      await this.request("/health/ready", {}, signal);
      return true;
    } catch {
      return false;
    }
  }

  namespace(): string { return `${this.baseUrl}/v1/default/banks/${encodeURIComponent(this.bankId)}`; }

  async recallFiltered(request: { query: string; tags: string[]; tags_match?: string }, signal?: AbortSignal): Promise<HindsightRecall[]> {
    const response = await this.request<RecallResponse>(`${this.namespace()}/memories/recall`, {
      method: "POST",
      body: JSON.stringify({ ...request, types: ["world", "experience", "observation"], prefer_observations: true, budget: "mid", max_tokens: 4096, trace: false }),
    }, signal);
    return Array.isArray(response.results) ? response.results : [];
  }

  async document(documentId: string, signal?: AbortSignal): Promise<HindsightDocument> {
    return this.request<HindsightDocument>(`${this.namespace()}/documents/${encodeURIComponent(documentId)}`, {}, signal);
  }

  async deleteDocument(documentId: string, signal?: AbortSignal): Promise<{ success?: boolean; document_id?: string }> {
    return this.request(`${this.namespace()}/documents/${encodeURIComponent(documentId)}`, { method: "DELETE" }, signal);
  }

  async operation(operationId: string, signal?: AbortSignal): Promise<OperationResponse> {
    return this.request<OperationResponse>(`${this.namespace()}/operations/${encodeURIComponent(operationId)}`, {}, signal);
  }

  async retainDocument(request: Record<string, unknown>, signal?: AbortSignal): Promise<OperationResponse> {
    return this.request<OperationResponse>(`${this.namespace()}/memories`, { method: "POST", body: JSON.stringify(request) }, signal);
  }

  async recall(query: string, signal?: AbortSignal): Promise<HindsightRecall[]> {
    const response = await this.request<RecallResponse>(
      `/v1/default/banks/${encodeURIComponent(this.bankId)}/memories/recall`,
      {
        method: "POST",
        body: JSON.stringify({
          query,
          types: ["world", "experience", "observation"],
          prefer_observations: true,
          budget: "mid",
          max_tokens: 4096,
          trace: false,
        }),
      },
      signal,
    );
    return Array.isArray(response.results) ? response.results : [];
  }

  async retain(memory: HindsightMemory, signal?: AbortSignal): Promise<unknown> {
    return this.request(
      `/v1/default/banks/${encodeURIComponent(this.bankId)}/memories`,
      {
        method: "POST",
        body: JSON.stringify({
          async: true,
          operation_id: randomUUID(),
          items: [memory],
        }),
      },
      signal,
    );
  }
}
