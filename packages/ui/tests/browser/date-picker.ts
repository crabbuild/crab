import { expect, type Page } from "@playwright/test";

export async function pickDate(page: Page, label: string, value: string) {
  const [year, , day] = value.split("-").map(Number);
  const date = new Date(`${value}T12:00:00Z`);
  const shortMonth = date.toLocaleString("en-US", {
    month: "short",
    timeZone: "UTC",
  });
  const longMonth = date.toLocaleString("en-US", {
    month: "long",
    timeZone: "UTC",
  });
  await page.getByRole("button", { name: new RegExp(`^${label}:`) }).click();
  const calendar = page.getByRole("dialog", {
    name: `${label} calendar`,
    exact: true,
  });
  await calendar
    .getByRole("combobox", { name: "Choose year", exact: true })
    .click();
  await page.getByRole("option", { name: String(year), exact: true }).click();
  await calendar
    .getByRole("combobox", { name: "Choose month", exact: true })
    .click();
  await page.getByRole("option", { name: shortMonth, exact: true }).click();
  await calendar
    .getByRole("button", {
      name: new RegExp(`${longMonth} ${day}(?:st|nd|rd|th)?, ${year}`),
    })
    .click();
  await expect(calendar).toHaveCount(0);
}
