import { describe, it, expect } from "vitest";
import { groupAndSort, STATUS_GROUP_ORDER } from "../src/components/ssh/TransferPanel";
import type { TransferEntry } from "../electron/ssh-types";

function makeEntry(overrides: Partial<TransferEntry> & { id: string }): TransferEntry {
  return {
    profileId: "profile-1",
    direction: "upload",
    localPath: `/local/${overrides.id}.bin`,
    remotePath: `/remote/${overrides.id}.bin`,
    fileName: `${overrides.id}.bin`,
    status: "active",
    bytesTransferred: 0,
    totalBytes: 1000,
    speed: 100,
    eta: 10,
    retryCount: 0,
    ...overrides,
  };
}

describe("groupAndSort", () => {
  it("returns empty array for empty map", () => {
    const result = groupAndSort(new Map());
    expect(result).toEqual([]);
  });

  it("groups transfers by status", () => {
    const transfers = new Map<string, TransferEntry>([
      ["a", makeEntry({ id: "a", status: "active", startedAt: 100 })],
      ["b", makeEntry({ id: "b", status: "completed", startedAt: 200 })],
      ["c", makeEntry({ id: "c", status: "active", startedAt: 300 })],
      ["d", makeEntry({ id: "d", status: "failed", startedAt: 150 })],
    ]);

    const result = groupAndSort(transfers);

    const statusOrder = result.map((g) => g.status);
    expect(statusOrder).toEqual(["active", "completed", "failed"]);
    expect(result[0].entries).toHaveLength(2);
    expect(result[1].entries).toHaveLength(1);
    expect(result[2].entries).toHaveLength(1);
  });

  it("sorts within groups by startedAt descending", () => {
    const transfers = new Map<string, TransferEntry>([
      ["a", makeEntry({ id: "a", status: "active", startedAt: 100 })],
      ["b", makeEntry({ id: "b", status: "active", startedAt: 300 })],
      ["c", makeEntry({ id: "c", status: "active", startedAt: 200 })],
    ]);

    const result = groupAndSort(transfers);
    const ids = result[0].entries.map((e) => e.id);
    expect(ids).toEqual(["b", "c", "a"]);
  });

  it("entries without startedAt sort last", () => {
    const transfers = new Map<string, TransferEntry>([
      ["a", makeEntry({ id: "a", status: "queued" })],
      ["b", makeEntry({ id: "b", status: "queued", startedAt: 500 })],
    ]);

    const result = groupAndSort(transfers);
    const ids = result[0].entries.map((e) => e.id);
    expect(ids).toEqual(["b", "a"]);
  });

  it("follows STATUS_GROUP_ORDER for group ordering", () => {
    const transfers = new Map<string, TransferEntry>([
      ["a", makeEntry({ id: "a", status: "scheduled", startedAt: 100 })],
      ["b", makeEntry({ id: "b", status: "active", startedAt: 100 })],
      ["c", makeEntry({ id: "c", status: "failed", startedAt: 100 })],
      ["d", makeEntry({ id: "d", status: "queued", startedAt: 100 })],
      ["e", makeEntry({ id: "e", status: "completed", startedAt: 100 })],
    ]);

    const result = groupAndSort(transfers);
    const statusOrder = result.map((g) => g.status);
    expect(statusOrder).toEqual(["active", "queued", "completed", "failed", "scheduled"]);
  });

  it("omits empty groups from result", () => {
    const transfers = new Map<string, TransferEntry>([
      ["a", makeEntry({ id: "a", status: "completed", startedAt: 100 })],
    ]);

    const result = groupAndSort(transfers);
    expect(result).toHaveLength(1);
    expect(result[0].status).toBe("completed");
  });
});
