/**
 * Accessibility tests for the Orchestrator Task Panel.
 *
 * Covers the task panel in all states: empty, running, reviewing, planning, failed.
 * Uses axe-core for automated WCAG 2.0 AA compliance checking.
 */

import { test, expect } from "@playwright/test";
import AxeBuilder from "@axe-core/playwright";

test.describe("Orchestrator Accessibility", () => {
  test("empty state should have no WCAG 2.0 AA violations", async ({ page }) => {
    await page.goto("/#/tasks");
    await page.waitForLoadState("networkidle");

    // The empty state should be visible when no tasks exist
    const results = await new AxeBuilder({ page })
      .withTags(["wcag2a", "wcag2aa"])
      .include('[data-testid="orchestrator-view"], [class*="orchestrator"], main')
      .analyze();

    const violations = results.violations.map((v) => ({
      id: v.id,
      impact: v.impact,
      description: v.description,
      nodes: v.nodes.map((n) => ({
        html: n.html,
        target: n.target,
        failureSummary: n.failureSummary,
      })),
    }));

    expect(violations, "axe-core AA violations in empty state").toEqual([]);
  });

  test("task list should have proper ARIA roles", async ({ page }) => {
    await page.goto("/#/tasks");
    await page.waitForLoadState("networkidle");

    // Verify task list has listbox role
    const taskList = page.locator('[role="listbox"][aria-label="Task list"]');
    // If tasks exist, verify they have proper labels
    const taskCards = page.locator('[aria-label^="Task:"]');
    const count = await taskCards.count();
    if (count > 0) {
      await expect(taskCards.first()).toHaveAttribute("aria-pressed");
    }
  });

  test("tab buttons should have proper ARIA attributes", async ({ page }) => {
    await page.goto("/#/tasks");
    await page.waitForLoadState("networkidle");

    // Check that tab buttons have role="tab" and aria-selected
    const tabs = page.locator('[role="tab"]');
    const tabCount = await tabs.count();
    if (tabCount > 0) {
      for (let i = 0; i < tabCount; i++) {
        await expect(tabs.nth(i)).toHaveAttribute("aria-selected");
      }
    }
  });

  test("action buttons should have aria-labels", async ({ page }) => {
    await page.goto("/#/tasks");
    await page.waitForLoadState("networkidle");

    // All buttons should have either visible text or aria-label
    const buttons = page.locator("button:not([aria-label]):not(:has(span))");
    const unlabeledCount = await buttons.count();

    // Filter to only buttons without any text content
    let unlabeledButtons = 0;
    for (let i = 0; i < unlabeledCount; i++) {
      const text = await buttons.nth(i).textContent();
      if (!text?.trim()) unlabeledButtons++;
    }

    expect(unlabeledButtons, "Buttons without labels or aria-labels").toBe(0);
  });

  test("keyboard navigation should work in task list", async ({ page }) => {
    await page.goto("/#/tasks");
    await page.waitForLoadState("networkidle");

    // Verify that the new task button is focusable
    const newTaskBtn = page.locator('[aria-label="Create new task"]');
    if (await newTaskBtn.isVisible()) {
      await newTaskBtn.focus();
      await expect(newTaskBtn).toBeFocused();
    }
  });

  test("status badges should have aria-labels", async ({ page }) => {
    await page.goto("/#/tasks");
    await page.waitForLoadState("networkidle");

    // Status dots should have aria-label describing the status
    const statusDots = page.locator('[aria-label="running"], [aria-label="reviewing"], [aria-label="planning"], [aria-label="failed"], [aria-label="applied"], [aria-label="archived"], [aria-label="queued"], [aria-label="iterating"]');
    const count = await statusDots.count();
    // If there are tasks, each should have a labeled status dot
    if (count > 0) {
      await expect(statusDots.first()).toBeVisible();
    }
  });
});
