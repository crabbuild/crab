import AxeBuilder from "@axe-core/playwright";
import { expect, type Page } from "@playwright/test";

export async function expectNoAccessibilityViolations(page: Page) {
  // TooltipV2 opens on hover and fades in. Move the pointer away and wait
  // for the popover to close so axe never samples a half-transparent overlay.
  await page.mouse.move(0, 0);
  await expect(
    page.locator('[data-component="Tooltip"][popover]:popover-open'),
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
