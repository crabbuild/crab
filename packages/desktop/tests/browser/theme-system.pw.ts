import { test, expect } from "@playwright/test";
import AxeBuilder from "@axe-core/playwright";
import palettes from "../../theme-tokens.json";

test.beforeEach(async ({ page }) => {
  await page.goto("/tests/fixtures/theme-system.html");
});

for (const mode of ["light", "dark", "hc-light", "hc-dark"] as const) {
  test(`${mode}: theme colors, bundled fonts, and accessible controls`, async ({
    page,
  }) => {
    await page.getByLabel("Theme", { exact: true }).click();
    await page
      .getByRole("option", { name: mode, exact: true }).click();
    await expect(page.locator("html")).toHaveClass(new RegExp(`theme-${mode}`));
    await page.getByRole("button", { name: "Show notification", exact: true }).click();
    await expect(page.locator("[data-sonner-toaster]")).toHaveAttribute("data-sonner-theme", mode.endsWith("light") ? "light" : "dark");
    await expect(page.locator("[data-sonner-toast]")).toHaveCSS("opacity", "1");
    const rgb = (value: string) => `rgb(${value.replaceAll(" ", ", ")})`;
    // Theme changes transition control colors. Assert the settled visual state.
    await expect.poll(() => page.getByRole("button", { name: "Save stage", exact: true }).evaluate(button => {
      const css = getComputedStyle(button);
      return [css.backgroundColor, css.color];
    })).toEqual([rgb(palettes[mode].primary), rgb(palettes[mode]["primary-foreground"])]);
    await page.evaluate(() => document.fonts.ready);
    expect(
      await page.evaluate(() => document.fonts.check('13px "Geist Variable"')),
    ).toBe(true);
    const result = await new AxeBuilder({ page })
      .withTags(["wcag2a", "wcag2aa"])
      .analyze();
    expect(
      result.violations.map(({ id, nodes }) => ({
        id,
        nodes: nodes.map((node) => node.failureSummary),
      })),
    ).toEqual([]);
    await page.screenshot({ path: test.info().outputPath(`${mode}.png`) });
  });
}

test("preserves field values, shadcn select values, input refs, and form submission", async ({
  page,
}) => {
  await page.getByRole("button", { name: "Focus stage", exact: true }).click();
  await expect(page.getByLabel("Stage name")).toBeFocused();
  await page.getByLabel("Stage name").fill("train");
  await page.getByLabel("Command").fill("python train.py");
  await page.getByLabel("Runner", { exact: true }).click();
  await page.getByRole("option", { name: "Python", exact: true }).click();
  await page.getByRole("button", { name: "Save stage", exact: true }).click();
  await expect(page.getByRole("status")).toHaveText(
    "train: python train.py (Python)",
  );
  await expect(
    page.getByRole("button", { name: "Unavailable", exact: true }),
  ).toBeDisabled();
});

test("workflow tabs support arrow keys and expose their panels", async ({
  page,
}) => {
  await page.getByRole("tab", { name: "Editor", exact: true }).focus();
  await page.keyboard.press("ArrowRight");
  await expect(page.getByRole("tab", { name: "Status 2" })).toBeFocused();
  await expect(page.getByRole("tabpanel")).toHaveText("Workflow status");
  await expect(page.locator('[role="tabpanel"]:visible')).toHaveCount(1);
  await page.keyboard.press("End");
  await expect(page.getByRole("tabpanel")).toHaveText("Workflow experiments");
});

test("confirmation traps focus, cancels on Escape, and restores the trigger", async ({
  page,
}) => {
  const trigger = page.getByRole("button", {
    name: "Review disk changes",
    exact: true,
  });
  await trigger.click();
  const dialog = page.getByRole("dialog", { name: "Hydrate files?" });
  await expect(dialog).toBeVisible();
  await expect(
    dialog.getByRole("button", { name: "Cancel", exact: true }),
  ).toBeFocused();
  await page.keyboard.press("Shift+Tab");
  await expect(
    dialog.getByRole("button", { name: "Hydrate", exact: true }),
  ).toBeFocused();
  await page.keyboard.press("Escape");
  await expect(dialog).toBeHidden();
  await expect(trigger).toBeFocused();
  await trigger.click();
  await dialog.getByRole("button", { name: "Hydrate", exact: true }).click();
  await expect(page.getByRole("status")).toHaveText("Disk change confirmed");
});

test("theme persists through reload and follows system preference", async ({
  page,
}) => {
  await page.getByLabel("Theme", { exact: true }).click();
  await page.getByRole("option", { name: "hc-light", exact: true }).click();
  await page.reload();
  await expect(page.locator("html")).toHaveClass(/theme-hc-light/);
  await page.getByRole("button", { name: "Show notification", exact: true }).click();
  await page.getByLabel("Theme", { exact: true }).click();
  await page.getByRole("option", { name: "system", exact: true }).click();
  await page.emulateMedia({ colorScheme: "dark" });
  await expect(page.locator("html")).toHaveClass(/theme-dark/);
  await expect(page.locator("[data-sonner-toaster]")).toHaveAttribute("data-sonner-theme", "dark");
  await page.emulateMedia({ colorScheme: "light" });
  await expect(page.locator("html")).toHaveClass(/theme-light/);
  await expect(page.locator("[data-sonner-toaster]")).toHaveAttribute("data-sonner-theme", "light");
});
