import { expect, type Locator } from "@playwright/test";

export async function openSecondaryFilter(trigger: Locator) {
  const more = trigger.page().getByRole("button", { name: "More filters" });
  await expect(trigger.or(more).first()).toBeVisible();
  if (!(await trigger.isVisible())) await more.click();
  await trigger.click();
}
