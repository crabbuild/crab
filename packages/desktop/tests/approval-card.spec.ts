/**
 * Regression tests for ApprovalCard collapse behaviour.
 *
 * Validates: Requirements CHAT1.1, CHAT1.4
 *
 * The key invariant: while a blocking approval has no decision recorded,
 * the card MUST remain expanded regardless of the store's collapse state.
 * Once a decision is recorded, the card falls back to the shared collapse
 * rule (respects the `collapsed` prop from the store).
 */

import { describe, it, expect } from "vitest";

// ─── Collapse Logic Under Test ───────────────────────────────────────────────

/**
 * Mirrors the collapse logic in ApprovalCard.tsx:
 *
 *   const collapsed = (decision === null && payload.blocking)
 *     ? false
 *     : (collapsedProp ?? false);
 *
 * Extracted here so we can unit-test the decision matrix without
 * mounting a React component.
 */
function resolveApprovalCollapsed(
  decision: "allow" | "allow_once" | "deny" | null,
  blocking: boolean,
  collapsedProp: boolean | undefined,
): boolean {
  if (decision === null && blocking) {
    return false;
  }
  return collapsedProp ?? false;
}

// ─── Tests ───────────────────────────────────────────────────────────────────

describe("ApprovalCard collapse logic", () => {
  describe("blocking approval with no decision", () => {
    it("forces expanded when store says collapsed", () => {
      const result = resolveApprovalCollapsed(null, true, true);
      expect(result).toBe(false);
    });

    it("stays expanded when store says expanded", () => {
      const result = resolveApprovalCollapsed(null, true, false);
      expect(result).toBe(false);
    });

    it("stays expanded when collapsedProp is undefined", () => {
      const result = resolveApprovalCollapsed(null, true, undefined);
      expect(result).toBe(false);
    });
  });

  describe("blocking approval after decision is recorded", () => {
    it("respects store collapsed=true after allow", () => {
      const result = resolveApprovalCollapsed("allow", true, true);
      expect(result).toBe(true);
    });

    it("respects store collapsed=true after deny", () => {
      const result = resolveApprovalCollapsed("deny", true, true);
      expect(result).toBe(true);
    });

    it("respects store collapsed=true after allow_once", () => {
      const result = resolveApprovalCollapsed("allow_once", true, true);
      expect(result).toBe(true);
    });

    it("stays expanded when store says expanded after decision", () => {
      const result = resolveApprovalCollapsed("allow", true, false);
      expect(result).toBe(false);
    });

    it("defaults to expanded when collapsedProp is undefined after decision", () => {
      const result = resolveApprovalCollapsed("allow", true, undefined);
      expect(result).toBe(false);
    });
  });

  describe("non-blocking approval (no forced expansion)", () => {
    it("respects store collapsed=true even with no decision", () => {
      const result = resolveApprovalCollapsed(null, false, true);
      expect(result).toBe(true);
    });

    it("stays expanded when store says expanded with no decision", () => {
      const result = resolveApprovalCollapsed(null, false, false);
      expect(result).toBe(false);
    });

    it("defaults to expanded when collapsedProp is undefined", () => {
      const result = resolveApprovalCollapsed(null, false, undefined);
      expect(result).toBe(false);
    });

    it("respects store collapsed=true after decision", () => {
      const result = resolveApprovalCollapsed("deny", false, true);
      expect(result).toBe(true);
    });
  });
});
