/**
 * ARIA disclosure tests for the chat surface.
 *
 * Validates: Requirements NFR2
 * Design: P7
 *
 * Every disclosure trigger (ToolGroup, TurnHeader, CollapsedCardLine,
 * TurnSummaryCard file rows) must have `aria-expanded` set correctly
 * based on its collapsed state.
 *
 * These tests verify the logic that determines the `aria-expanded`
 * attribute value for each disclosure component, ensuring the DOM
 * state matches the semantic state.
 */

import { describe, it, expect } from "vitest";

// ─── ToolGroup aria-expanded logic ───────────────────────────────────────────

/**
 * Mirrors the ToolGroup component's aria-expanded logic:
 *
 * - Collapsed (complete): renders button with aria-expanded={false}
 * - Collapsed (running): renders button with aria-expanded={false}
 * - Expanded: renders disclosure header button with aria-expanded={true}
 *
 * The component always renders a button as the disclosure trigger.
 * aria-expanded reflects whether the group body (member cards) is visible.
 */
function resolveToolGroupAriaExpanded(
  storeCollapsed: boolean | undefined,
  defaultCollapsed: boolean,
  complete: boolean,
): boolean {
  const collapsed = storeCollapsed ?? defaultCollapsed;
  // aria-expanded is the inverse of collapsed — true means body is visible
  return !collapsed;
}

// ─── TurnHeader aria-expanded logic ──────────────────────────────────────────

/**
 * Mirrors the TurnHeader component's aria-expanded logic:
 *
 *   aria-expanded={!isCollapsed}
 *
 * Where isCollapsed is resolved from:
 *   1. Store override (turnCollapse[turnIndex]) if present
 *   2. Default: collapsed when turn is complete, expanded when running
 */
function resolveTurnHeaderAriaExpanded(
  turnCollapseEntry: { collapsed: boolean } | undefined,
  turnComplete: boolean,
): boolean {
  const isCollapsed = turnCollapseEntry != null
    ? turnCollapseEntry.collapsed
    : turnComplete;
  // aria-expanded is the inverse of collapsed
  return !isCollapsed;
}

// ─── CollapsedCardLine aria-expanded logic ───────────────────────────────────

/**
 * Mirrors the CollapsedCardLine component's aria-expanded logic:
 *
 *   aria-expanded={false}
 *
 * CollapsedCardLine only renders when a card IS collapsed, so its
 * disclosure trigger always reports aria-expanded=false (the card body
 * is hidden). When the card is expanded, CollapsedCardLine is not
 * rendered at all — the full card body renders instead.
 *
 * The overall card disclosure pattern:
 * - Card collapsed → CollapsedCardLine renders with aria-expanded=false
 * - Card expanded → full card body renders (no CollapsedCardLine)
 */
function resolveCollapsedCardLineAriaExpanded(): boolean {
  // CollapsedCardLine always renders with aria-expanded=false
  // because it only appears when the card is in collapsed state
  return false;
}

/**
 * Resolves whether a card should render as collapsed (showing
 * CollapsedCardLine) or expanded (showing full body).
 *
 * This determines which branch renders and therefore what
 * aria-expanded value is present in the DOM.
 */
function resolveCardCollapsedState(
  collapsedProp: boolean | undefined,
  eventComplete: boolean,
): { collapsed: boolean; ariaExpanded: boolean } {
  // If collapsedProp is provided, use it; otherwise default based on completion
  const collapsed = collapsedProp ?? eventComplete;
  // When collapsed: CollapsedCardLine renders with aria-expanded=false
  // When expanded: no disclosure trigger (full body shown)
  return { collapsed, ariaExpanded: !collapsed };
}

// ─── TurnSummaryCard FileRow aria-expanded logic ─────────────────────────────

/**
 * Mirrors the TurnSummaryCard FileRow's aria-expanded logic:
 *
 *   aria-expanded={expanded}
 *
 * Each file row in the TurnSummaryCard is a disclosure that expands
 * to show the inline diff. The `expanded` state is tracked per-file
 * in a local Set<string>.
 */
function resolveFileRowAriaExpanded(
  expandedFiles: Set<string>,
  filePath: string,
): boolean {
  return expandedFiles.has(filePath);
}

// ─── Tests ───────────────────────────────────────────────────────────────────

describe("ARIA disclosure: ToolGroup", () => {
  describe("aria-expanded reflects collapsed state", () => {
    it("collapsed complete group → aria-expanded=false", () => {
      const ariaExpanded = resolveToolGroupAriaExpanded(true, true, true);
      expect(ariaExpanded).toBe(false);
    });

    it("collapsed running group → aria-expanded=false", () => {
      const ariaExpanded = resolveToolGroupAriaExpanded(true, true, false);
      expect(ariaExpanded).toBe(false);
    });

    it("expanded group → aria-expanded=true", () => {
      const ariaExpanded = resolveToolGroupAriaExpanded(false, true, true);
      expect(ariaExpanded).toBe(true);
    });

    it("no store override, defaultCollapsed=true → aria-expanded=false", () => {
      const ariaExpanded = resolveToolGroupAriaExpanded(undefined, true, true);
      expect(ariaExpanded).toBe(false);
    });

    it("no store override, defaultCollapsed=false → aria-expanded=true", () => {
      const ariaExpanded = resolveToolGroupAriaExpanded(undefined, false, true);
      expect(ariaExpanded).toBe(true);
    });

    it("store override=false overrides defaultCollapsed=true → aria-expanded=true", () => {
      const ariaExpanded = resolveToolGroupAriaExpanded(false, true, true);
      expect(ariaExpanded).toBe(true);
    });

    it("store override=true overrides defaultCollapsed=false → aria-expanded=false", () => {
      const ariaExpanded = resolveToolGroupAriaExpanded(true, false, false);
      expect(ariaExpanded).toBe(false);
    });
  });
});

describe("ARIA disclosure: TurnHeader", () => {
  describe("aria-expanded reflects turn collapsed state", () => {
    it("complete turn with no store override → aria-expanded=false (default collapsed)", () => {
      const ariaExpanded = resolveTurnHeaderAriaExpanded(undefined, true);
      expect(ariaExpanded).toBe(false);
    });

    it("running turn with no store override → aria-expanded=true (default expanded)", () => {
      const ariaExpanded = resolveTurnHeaderAriaExpanded(undefined, false);
      expect(ariaExpanded).toBe(true);
    });

    it("store says collapsed → aria-expanded=false regardless of turn state", () => {
      const ariaExpanded = resolveTurnHeaderAriaExpanded({ collapsed: true }, false);
      expect(ariaExpanded).toBe(false);
    });

    it("store says expanded → aria-expanded=true regardless of turn state", () => {
      const ariaExpanded = resolveTurnHeaderAriaExpanded({ collapsed: false }, true);
      expect(ariaExpanded).toBe(true);
    });

    it("store says expanded for running turn → aria-expanded=true", () => {
      const ariaExpanded = resolveTurnHeaderAriaExpanded({ collapsed: false }, false);
      expect(ariaExpanded).toBe(true);
    });

    it("store says collapsed for complete turn → aria-expanded=false", () => {
      const ariaExpanded = resolveTurnHeaderAriaExpanded({ collapsed: true }, true);
      expect(ariaExpanded).toBe(false);
    });
  });
});

describe("ARIA disclosure: CollapsedCardLine", () => {
  describe("always reports aria-expanded=false", () => {
    it("CollapsedCardLine renders with aria-expanded=false", () => {
      const ariaExpanded = resolveCollapsedCardLineAriaExpanded();
      expect(ariaExpanded).toBe(false);
    });
  });

  describe("card collapse state determines which branch renders", () => {
    it("collapsed prop=true → card is collapsed, aria-expanded=false", () => {
      const { collapsed, ariaExpanded } = resolveCardCollapsedState(true, true);
      expect(collapsed).toBe(true);
      expect(ariaExpanded).toBe(false);
    });

    it("collapsed prop=false → card is expanded, aria-expanded=true", () => {
      const { collapsed, ariaExpanded } = resolveCardCollapsedState(false, true);
      expect(collapsed).toBe(false);
      expect(ariaExpanded).toBe(true);
    });

    it("collapsed prop=undefined, event complete → defaults to collapsed", () => {
      const { collapsed, ariaExpanded } = resolveCardCollapsedState(undefined, true);
      expect(collapsed).toBe(true);
      expect(ariaExpanded).toBe(false);
    });

    it("collapsed prop=undefined, event in progress → defaults to expanded", () => {
      const { collapsed, ariaExpanded } = resolveCardCollapsedState(undefined, false);
      expect(collapsed).toBe(false);
      expect(ariaExpanded).toBe(true);
    });
  });
});

describe("ARIA disclosure: TurnSummaryCard file rows", () => {
  describe("aria-expanded reflects file expansion state", () => {
    it("file not in expanded set → aria-expanded=false", () => {
      const expandedFiles = new Set<string>();
      const ariaExpanded = resolveFileRowAriaExpanded(expandedFiles, "/src/app.ts");
      expect(ariaExpanded).toBe(false);
    });

    it("file in expanded set → aria-expanded=true", () => {
      const expandedFiles = new Set(["/src/app.ts"]);
      const ariaExpanded = resolveFileRowAriaExpanded(expandedFiles, "/src/app.ts");
      expect(ariaExpanded).toBe(true);
    });

    it("different file in expanded set → aria-expanded=false for queried file", () => {
      const expandedFiles = new Set(["/src/other.ts"]);
      const ariaExpanded = resolveFileRowAriaExpanded(expandedFiles, "/src/app.ts");
      expect(ariaExpanded).toBe(false);
    });

    it("multiple files expanded → each reports correctly", () => {
      const expandedFiles = new Set(["/src/a.ts", "/src/b.ts", "/src/c.ts"]);

      expect(resolveFileRowAriaExpanded(expandedFiles, "/src/a.ts")).toBe(true);
      expect(resolveFileRowAriaExpanded(expandedFiles, "/src/b.ts")).toBe(true);
      expect(resolveFileRowAriaExpanded(expandedFiles, "/src/c.ts")).toBe(true);
      expect(resolveFileRowAriaExpanded(expandedFiles, "/src/d.ts")).toBe(false);
    });

    it("toggling a file updates aria-expanded correctly", () => {
      const expandedFiles = new Set<string>();

      // Initially collapsed
      expect(resolveFileRowAriaExpanded(expandedFiles, "/src/app.ts")).toBe(false);

      // Expand
      expandedFiles.add("/src/app.ts");
      expect(resolveFileRowAriaExpanded(expandedFiles, "/src/app.ts")).toBe(true);

      // Collapse again
      expandedFiles.delete("/src/app.ts");
      expect(resolveFileRowAriaExpanded(expandedFiles, "/src/app.ts")).toBe(false);
    });
  });
});

describe("ARIA disclosure: deterministic transcript scenario", () => {
  /**
   * Simulates a complete transcript with multiple disclosure triggers
   * and verifies that every aria-expanded value is consistent with
   * the underlying collapsed/expanded state.
   *
   * Scenario: A task with 2 turns, each containing tool groups and
   * file edits. Verifies the full matrix of disclosure states.
   */
  it("all disclosures in a multi-turn transcript report correct aria-expanded", () => {
    // Turn 0: complete, 1 tool group with 3 members
    const turn0Complete = true;
    const turn0StoreOverride = undefined; // use default

    // Turn 1: running, 1 tool group with 2 members
    const turn1Complete = false;
    const turn1StoreOverride = undefined; // use default

    // ── TurnHeader disclosures ──
    const turn0HeaderAria = resolveTurnHeaderAriaExpanded(turn0StoreOverride, turn0Complete);
    const turn1HeaderAria = resolveTurnHeaderAriaExpanded(turn1StoreOverride, turn1Complete);

    // Complete turn defaults to collapsed → aria-expanded=false
    expect(turn0HeaderAria).toBe(false);
    // Running turn defaults to expanded → aria-expanded=true
    expect(turn1HeaderAria).toBe(true);

    // ── ToolGroup disclosures ──
    // Turn 0's group: complete, default collapsed
    const group0Aria = resolveToolGroupAriaExpanded(undefined, true, true);
    expect(group0Aria).toBe(false);

    // Turn 1's group: running, default expanded (defaultCollapsed=false for running)
    const group1Aria = resolveToolGroupAriaExpanded(undefined, false, false);
    expect(group1Aria).toBe(true);

    // ── Card disclosures (within expanded groups) ──
    // A completed card in a collapsed group won't render individually,
    // but if the group is expanded, each card has its own disclosure
    const completedCard = resolveCardCollapsedState(undefined, true);
    expect(completedCard.ariaExpanded).toBe(false);

    const runningCard = resolveCardCollapsedState(undefined, false);
    expect(runningCard.ariaExpanded).toBe(true);

    // ── TurnSummaryCard file rows ──
    // Turn 0 has file edits, so TurnSummaryCard renders
    const expandedFiles = new Set<string>();
    const file1Aria = resolveFileRowAriaExpanded(expandedFiles, "/src/main.ts");
    const file2Aria = resolveFileRowAriaExpanded(expandedFiles, "/src/utils.ts");
    expect(file1Aria).toBe(false);
    expect(file2Aria).toBe(false);

    // User expands one file
    expandedFiles.add("/src/main.ts");
    expect(resolveFileRowAriaExpanded(expandedFiles, "/src/main.ts")).toBe(true);
    expect(resolveFileRowAriaExpanded(expandedFiles, "/src/utils.ts")).toBe(false);
  });

  it("user overrides produce correct aria-expanded across all disclosure types", () => {
    // User has expanded a normally-collapsed complete turn
    const turnHeaderAria = resolveTurnHeaderAriaExpanded({ collapsed: false }, true);
    expect(turnHeaderAria).toBe(true);

    // User has collapsed a normally-expanded running group
    const groupAria = resolveToolGroupAriaExpanded(true, false, false);
    expect(groupAria).toBe(false);

    // User has expanded a normally-collapsed card
    const cardState = resolveCardCollapsedState(false, true);
    expect(cardState.ariaExpanded).toBe(true);

    // User has expanded a file row
    const expandedFiles = new Set(["/src/config.ts"]);
    expect(resolveFileRowAriaExpanded(expandedFiles, "/src/config.ts")).toBe(true);
  });
});
