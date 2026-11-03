import { describe, expect, it } from "vitest";
import {
  queueRestoreSnapshot,
  replayQueuedRestoreSnapshot,
  type PendingRestoreSnapshotMap,
} from "../src/lib/terminal-restore-snapshots";

function createTarget() {
  const writes: string[] = [];
  return {
    writes,
    target: {
      write(data: string) {
        writes.push(data);
      },
    },
  };
}

describe("terminal restore snapshots", () => {
  it("keeps inactive tab snapshots queued until that tab mounts", () => {
    const pending: PendingRestoreSnapshotMap = new Map();
    queueRestoreSnapshot(pending, "term-d-1", "snapshot-1");
    queueRestoreSnapshot(pending, "term-d-2", "snapshot-2");
    queueRestoreSnapshot(pending, "term-d-3", "snapshot-3");

    const first = createTarget();
    expect(replayQueuedRestoreSnapshot(pending, "term-d-1", first.target)).toBe(true);
    expect(first.writes).toEqual(["snapshot-1"]);

    expect(pending.has("term-d-1")).toBe(false);
    expect(pending.has("term-d-2")).toBe(true);
    expect(pending.has("term-d-3")).toBe(true);

    const third = createTarget();
    expect(replayQueuedRestoreSnapshot(pending, "term-d-3", third.target)).toBe(true);
    expect(third.writes).toEqual(["snapshot-3"]);

    const second = createTarget();
    expect(replayQueuedRestoreSnapshot(pending, "term-d-2", second.target)).toBe(true);
    expect(second.writes).toEqual(["snapshot-2"]);

    expect(pending.size).toBe(0);
  });

  it("ignores empty snapshots and missing ids", () => {
    const pending: PendingRestoreSnapshotMap = new Map();
    queueRestoreSnapshot(pending, "term-d-1", "");
    queueRestoreSnapshot(pending, "term-d-2", null);

    const target = createTarget();
    expect(replayQueuedRestoreSnapshot(pending, "term-d-1", target.target)).toBe(false);
    expect(replayQueuedRestoreSnapshot(pending, "term-d-2", target.target)).toBe(false);
    expect(replayQueuedRestoreSnapshot(pending, "term-d-3", target.target)).toBe(false);
    expect(target.writes).toEqual([]);
    expect(pending.size).toBe(0);
  });
});
