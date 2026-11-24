import type { Locator } from "@playwright/test";

export async function chooseSelectOption(
  trigger: Locator,
  value: string | { label: string },
) {
  await trigger.click();
  const list = trigger.page().getByRole("listbox");
  const option =
    typeof value === "string"
      ? list
          .getByRole("option")
          .and(list.locator(`[data-value=${JSON.stringify(value)}]`))
      : list.getByRole("option", { name: value.label, exact: true });
  await option.click();
}
