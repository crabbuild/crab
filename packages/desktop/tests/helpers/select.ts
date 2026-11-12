import type { Locator } from "@playwright/test";

// Application choices keep their existing values; only their rendered control
// changes from a native select to a portalled Radix listbox.
export async function chooseSelect(
  trigger: Locator,
  choice: string | { label: string },
) {
  await trigger.click();
  const options = trigger.page().getByRole("listbox");
  if (typeof choice === "string")
    await options
      .locator(`[role="option"][data-value=${JSON.stringify(":" + choice)}]`)
      .click();
  else
    await options
      .getByRole("option", { name: choice.label, exact: true })
      .click();
}
