import AxeBuilder from "@axe-core/playwright";
import { expect, type Page } from "@playwright/test";

export function contrastRatio(a: string, b: string) {
  const luminance = (color: string) => {
    const channels = color
      .match(/[\d.]+/g)!
      .slice(0, 3)
      .map(Number)
      .map((value) => {
        const channel = value / 255;
        return channel <= 0.04045
          ? channel / 12.92
          : ((channel + 0.055) / 1.055) ** 2.4;
      });
    return channels[0] * 0.2126 + channels[1] * 0.7152 + channels[2] * 0.0722;
  };
  const light = luminance(a),
    dark = luminance(b);
  return (Math.max(light, dark) + 0.05) / (Math.min(light, dark) + 0.05);
}

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
