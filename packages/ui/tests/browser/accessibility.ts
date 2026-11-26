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
  // Axe samples colours mid-transition otherwise; endless spinners never settle.
  await page.evaluate(() =>
    Promise.all(
      document
        .getAnimations()
        .filter(
          (animation) =>
            animation.playState === "running" &&
            animation.effect?.getComputedTiming().endTime !== Infinity,
        )
        .map((animation) => animation.finished.catch(() => undefined)),
    ),
  );
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

export async function selectTheme(
  page: Page,
  theme: "light" | "dark" | "auto",
) {
  const label =
    theme === "light" ? "Light" : theme === "dark" ? "Dark" : "System";
  const direct = page.locator(`[aria-label="${label}"]`);
  const userMenu = page.getByRole("button", {
    name: "Open user menu",
    exact: true,
  });
  // The session check shows its own theme buttons until the app replaces them.
  await expect(userMenu.or(direct).first()).toBeVisible();
  await expect(
    page.getByText("Checking your session…", { exact: true }),
  ).toHaveCount(0);
  if (await direct.isVisible()) {
    await direct.click();
  } else {
    await userMenu.click();
    await page.getByRole("menuitemradio", { name: label, exact: true }).click();
  }
  if (theme === "light" || theme === "dark") {
    await expect(page.locator("html")).toHaveCSS("color-scheme", theme);
  }
}

export async function selectDarkTheme(page: Page) {
  await selectTheme(page, "dark");
}
