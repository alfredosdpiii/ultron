export type ApprovalRecord = {
  id: string;
  status: "pending" | "approved" | "rejected";
  tool: string;
  inputHash: string;
  severity: string;
  timestamp: string;
};

type EntryWriter = { appendEntry: (customType: string, data?: unknown) => void };

type SessionReader = { sessionManager: { getEntries: () => Array<{ type?: string; customType?: string; data?: unknown }> } };

export function recordApproval(writer: EntryWriter, record: ApprovalRecord): void {
  writer.appendEntry("jev-approval", record);
}

export function pendingApprovals(ctx: SessionReader): ApprovalRecord[] {
  const states = new Map<string, ApprovalRecord>();
  for (const entry of ctx.sessionManager.getEntries()) {
    if (entry.type !== "custom" || entry.customType !== "jev-approval" || !entry.data || typeof entry.data !== "object") continue;
    const record = entry.data as ApprovalRecord;
    if (record.id && record.status) states.set(record.id, record);
  }
  return [...states.values()].filter((record) => record.status === "pending");
}
