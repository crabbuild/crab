import { describe, it, expect } from "vitest";
import { getPendingSaveCount } from "../src/hooks/useRemoteAutoSave";

describe("useRemoteAutoSave", () => {
  describe("getPendingSaveCount", () => {
    it("returns 0 when no saves are pending", () => {
      // The module-level pendingSaves map starts empty.
      expect(getPendingSaveCount()).toBe(0);
    });
  });
});
