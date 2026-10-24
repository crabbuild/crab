import AxeBuilder from "@axe-core/playwright";
import { expect, type Page } from "@playwright/test";

export async function expectNoAccessibilityViolations(page: Page) {
  // Radix retains closing tooltips until their exit animation ends. Move the
  // pointer away and await removal so axe audits the settled page contrast.
  await page.mouse.move(0, 0);
  await expect(
    page.locator('[data-slot="tooltip-content"][data-state="closed"]'),
  ).toHaveCount(0);
  const result = await new AxeBuilder({ page })
    .withTags([
      "wcag2a",
      "wcag2aa",
      "wcag21a",
      "wcag21aa",
      "wcag22a",
      "wcag22aa",
    ])
    .analyze();
  expect(result.violations).toEqual([]);
}

export async function selectDarkTheme(page: Page) {
  await page.locator('[aria-label="Dark"]').click();
  await expect(page.locator("html")).toHaveCSS("color-scheme", "dark");
}
