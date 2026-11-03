import { describe, it, expect } from "vitest";
import {
  getEnabledBatchActions,
  computeSelectedSize,
} from "../src/hooks/useTransferSelection";
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

describe("getEnabledBatchActions", () => {
  it("returns all false for empty selection", () => {
    const transfers = new Map<string, TransferEntry>();
    const result = getEnabledBatchActions(new Set(), transfers);
    expect(result).toEqual({ canCancel: false, canRetry: false, canClear: false });
  });

  it("enables cancel for queued transfers", () => {
    const transfers = new Map<string, TransferEntry>([
      ["a", makeEntry({ id: "a", status: "queued" })],
    ]);
    const result = getEnabledBatchActions(new Set(["a"]), transfers);
    expect(result.canCancel).toBe(true);
    expect(result.canRetry).toBe(false);
    expect(result.canClear).toBe(false);
  });

  it("enables cancel for active transfers", () => {
    const transfers = new Map<string, TransferEntry>([
      ["a", makeEntry({ id: "a", status: "active" })],
    ]);
    const result = getEnabledBatchActions(new Set(["a"]), transfers);
    expect(result.canCancel).toBe(true);
  });

  it("enables retry and clear for failed transfers", () => {
    const transfers = new Map<string, TransferEntry>([
      ["a", makeEntry({ id: "a", status: "failed" })],
    ]);
    const result = getEnabledBatchActions(new Set(["a"]), transfers);
    expect(result.canCancel).toBe(false);
    expect(result.canRetry).toBe(true);
    expect(result.canClear).toBe(true);
  });

  it("enables clear for completed transfers", () => {
    const transfers = new Map<string, TransferEntry>([
      ["a", makeEntry({ id: "a", status: "completed" })],
    ]);
    const result = getEnabledBatchActions(new Set(["a"]), transfers);
    expect(result.canCancel).toBe(false);
    expect(result.canRetry).toBe(false);
    expect(result.canClear).toBe(true);
  });

  it("enables all actions for mixed statuses", () => {
    const transfers = new Map<string, TransferEntry>([
      ["a", makeEntry({ id: "a", status: "active" })],
      ["b", makeEntry({ id: "b", status: "failed" })],
      ["c", makeEntry({ id: "c", status: "completed" })],
    ]);
    const result = getEnabledBatchActions(new Set(["a", "b", "c"]), transfers);
    expect(result).toEqual({ canCancel: true, canRetry: true, canClear: true });
  });

  it("ignores selected IDs not in transfers map", () => {
    const transfers = new Map<string, TransferEntry>([
      ["a", makeEntry({ id: "a", status: "completed" })],
    ]);
    const result = getEnabledBatchActions(new Set(["a", "missing"]), transfers);
    expect(result.canClear).toBe(true);
    expect(result.canCancel).toBe(false);
    expect(result.canRetry).toBe(false);
  });

  it("enables cancel for scheduled transfers", () => {
    const transfers = new Map<string, TransferEntry>([
      ["a", makeEntry({ id: "a", status: "scheduled" })],
    ]);
    const result = getEnabledBatchActions(new Set(["a"]), transfers);
    expect(result.canCancel).toBe(true);
  });
});

describe("computeSelectedSize", () => {
  it("returns 0 for empty selection", () => {
    const transfers = new Map<string, TransferEntry>();
    expect(computeSelectedSize(new Set(), transfers)).toBe(0);
  });

  it("sums totalBytes of selected transfers", () => {
    const transfers = new Map<string, TransferEntry>([
      ["a", makeEntry({ id: "a", totalBytes: 1000 })],
      ["b", makeEntry({ id: "b", totalBytes: 2500 })],
      ["c", makeEntry({ id: "c", totalBytes: 500 })],
    ]);
    expect(computeSelectedSize(new Set(["a", "b"]), transfers)).toBe(3500);
  });

  it("ignores selected IDs not in transfers map", () => {
    const transfers = new Map<string, TransferEntry>([
      ["a", makeEntry({ id: "a", totalBytes: 1000 })],
    ]);
    expect(computeSelectedSize(new Set(["a", "missing"]), transfers)).toBe(1000);
  });
});
