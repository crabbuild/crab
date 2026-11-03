export interface PendingRestoreSnapshot {
  data: string;
}

export type PendingRestoreSnapshotMap = Map<string, PendingRestoreSnapshot>;

export interface RestoreSnapshotTarget {
  write(data: string): void;
}

export function queueRestoreSnapshot(
  pending: PendingRestoreSnapshotMap,
  terminalId: string,
  data: string | null | undefined,
): void {
  if (!data) return;
  pending.set(terminalId, { data });
}

export function replayQueuedRestoreSnapshot(
  pending: PendingRestoreSnapshotMap,
  terminalId: string,
  target: RestoreSnapshotTarget,
): boolean {
  const snapshot = pending.get(terminalId);
  if (!snapshot) return false;

  pending.delete(terminalId);
  target.write(snapshot.data);
  return true;
}
