import { test, expect } from "@playwright/test";
import AxeBuilder from "@axe-core/playwright";

test.describe("Accessibility", () => {
  test("should have no WCAG 2.0 AA violations on the main view", async ({
    page,
  }) => {
    await page.goto("/");
    // Wait for the app to render meaningful content.
    await page.waitForLoadState("networkidle");

    const results = await new AxeBuilder({ page })
      .withTags(["wcag2a", "wcag2aa"])
      .analyze();

    // Report violation details for easier debugging.
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

    expect(violations, "axe-core AA violations found").toEqual([]);
  });
});
