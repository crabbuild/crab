import { test, expect, chromium } from "@playwright/test";
import { pathToFileURL } from "node:url";
import path from "node:path";

test("immutable Results HTML renders actual retained curves with networking denied", async () => {
  test.skip(
    !process.env.CRAB_RESULTS_EVIDENCE,
    "Requires the real queued Results qualification artifact",
  );
  const browser = await chromium.launch({
    channel: process.env.CI ? undefined : "chrome",
  });
  const context = await browser.newContext();
  const attempted: string[] = [];
  await context.route(/^https?:/, (route) => {
    attempted.push(route.request().url());
    return route.abort();
  });
  const page = await context.newPage();
  const errors: string[] = [];
  page.on("pageerror", (error) => errors.push(error.message));
  try {
    await page.goto(
      pathToFileURL(
        path.join(process.env.CRAB_RESULTS_EVIDENCE!, "report.html"),
      ).href,
    );
    await expect(page.locator("svg")).toHaveCount(2);
    await expect(page.locator("table")).toContainText("0.1");
    await expect(page.locator("table")).toContainText("0.2");
    const runs = await page
      .locator("#comparison tr:has(td) td:first-child")
      .allTextContents();
    expect(new Set(runs).size).toBe(2);
    for (const [index, run] of runs.entries()) {
      const metric = page
        .locator("#attachments p")
        .filter({ hasText: run })
        .filter({ hasText: "metrics.json" });
      await expect(metric).toHaveCount(1);
      await expect(
        metric.getByRole("link", { name: "Download", exact: true }),
      ).toBeVisible();
      await metric.getByRole("button", { name: "Preview", exact: true }).click();
      expect(
        JSON.parse((await page.locator("#preview pre").textContent()) ?? "null"),
      ).toMatchObject({ score: [0.1, 0.2][index] });
    }
    expect(await page.locator("svg path").count()).toBeGreaterThan(2);
    const ranges = page.getByRole("form", { name: /^X range / });
    await expect(ranges).toHaveCount(2);
    await ranges
      .first()
      .getByRole("button", { name: "Zoom in", exact: true })
      .click();
    for (const input of await page.getByLabel(/^X minimum /).all())
      await expect(input).toHaveValue("0.125");
    await ranges
      .first()
      .getByRole("button", { name: "Reset range", exact: true })
      .click();
    for (const input of await page.getByLabel(/^X minimum /).all())
      await expect(input).toHaveValue("0");
    for (const input of await page.getByLabel(/^X maximum /).all())
      await expect(input).toHaveValue("0.5");
    const zoom = ranges.first().getByRole("button", { name: "Zoom in", exact: true });
    await zoom.focus();
    for (const [color, background] of [
      ["rgb(245, 245, 245)", "rgb(15, 15, 15)"],
      ["rgb(23, 23, 23)", "rgb(255, 255, 255)"],
    ]) {
      await page.evaluate(({ color, background }) => {
        document.body.style.color = color;
        document.body.style.backgroundColor = background;
      }, { color, background });
      const paints = await page.locator(".crab-results-chart svg text").evaluateAll(labels =>
        labels.map(label => getComputedStyle(label).fill)
      );
      expect(paints.length).toBeGreaterThan(0);
      expect(new Set(paints)).toEqual(new Set([color]));
      await expect(zoom).toBeFocused();
      for (const input of await page.getByLabel(/^X maximum /).all())
        await expect(input).toHaveValue("0.5");
    }
    await page
      .locator('g[class*="_voronoi"] path')
      .first()
      .hover({ force: true });
    await expect(page.getByRole("status")).toContainText("Cursor step:");
    await expect(page.locator("g.mark-rule.role-mark line")).toHaveCount(2);
    expect(errors).toEqual([]);
    expect(attempted).toEqual([]);
    await page.screenshot({
      path: path.join(process.env.CRAB_RESULTS_EVIDENCE!, "offline-report.png"),
      fullPage: true,
    });
  } finally {
    await context.close();
    await browser.close();
  }
});
