import { expect, test } from "@playwright/test";
import { existsSync, readFileSync, mkdirSync } from "node:fs";
import path from "node:path";
import { pathToFileURL } from "node:url";
import type { Report, ResultsRead } from "@/lib/results-contracts";

const evidence = process.env.CRAB_RESULTS_EVIDENCE;
test.use({ channel: process.env.CI ? undefined : "chrome" });

test("authored plot failures remain visible while real metrics and healthy plots survive", async ({
  page,
}) => {
  const file = evidence && path.join(evidence, "authored-failures.html");
  test.skip(
    !file || !existsSync(file),
    "Requires the real CLI authored-plot export",
  );
  const payload: { id: string; report: Report } = JSON.parse(
    readFileSync(file!, "utf8").match(
      /<script id="results-data" type="application\/json">([\s\S]*?)<\/script>/,
    )![1],
  );
  let activePayload = payload;
  mkdirSync(path.join(evidence!, "browser"), { recursive: true });
  const errors: string[] = [];
  const attempted: string[] = [];
  page.on("pageerror", (error) => errors.push(error.message));
  const denyNetwork = (route: import("@playwright/test").Route) => {
    attempted.push(route.request().url());
    return route.abort();
  };
  await page.route(/^https?:/, denyNetwork);
  await page.goto(pathToFileURL(file!).href);
  const offline = page.locator("#charts > section");
  const healthy = offline.filter({
    has: page.getByRole("heading", { name: "healthy", exact: true }),
  });
  const event = offline.filter({
    has: page.getByRole("heading", { name: "event", exact: true }),
  });
  await expect(healthy.locator("g.mark-line path")).toHaveCount(1);
  await expect(event.locator("g.mark-line path")).toHaveCount(1);
  await expect(offline.getByRole("alert")).toHaveCount(2);
  await expect(
    offline
      .filter({
        has: page.getByRole("heading", { name: "initial", exact: true }),
      })
      .getByRole("alert"),
  ).toContainText(
    "Could not render this plot. Inspect its template and source data.",
  );
  await event.getByRole("checkbox").click();
  await expect(event.getByRole("alert")).toContainText(
    "Could not render this plot.",
  );
  await expect(event.locator("svg")).toHaveCount(0);
  await expect(
    event.getByRole("button", { name: "Save SVG", exact: true }),
  ).toHaveCount(0);
  await expect(healthy.locator("g.mark-line path")).toHaveCount(1);
  await expect(page.locator("#comparison")).toContainText("0.25");
  await expect(page.locator("#identity")).toContainText(payload.id);
  for (const width of [1280, 390]) {
    await page.setViewportSize({ width, height: 900 });
    await expect
      .poll(() => page.evaluate(() => document.documentElement.scrollWidth))
      .toBeLessThanOrEqual(width);
    await page.screenshot({
      path: path.join(evidence!, `browser/authored-offline-${width}.png`),
      fullPage: true,
    });
  }
  expect(attempted).toEqual([]);
  await page.goto(
    pathToFileURL(path.join(evidence!, "authored-metrics.html")).href,
  );
  const exportedPlots = page.locator("#plots section");
  const exportedHealthy = exportedPlots.filter({
    has: page.getByRole("heading", { name: "healthy", exact: true }),
  });
  const exportedEvent = exportedPlots.filter({
    has: page.getByRole("heading", { name: "event", exact: true }),
  });
  await expect(exportedPlots.getByRole("alert")).toHaveCount(2);
  await expect(exportedHealthy.locator("g.mark-line path")).toHaveCount(1);
  await exportedEvent.getByRole("checkbox").click();
  await expect(exportedPlots.getByRole("alert")).toHaveCount(3);
  await expect(exportedEvent.locator("svg")).toHaveCount(0);
  await expect(exportedHealthy.locator("g.mark-line path")).toHaveCount(1);
  expect(attempted).toEqual([]);
  await page.unroute(/^https?:/, denyNetwork);

  const unexpected: string[] = [];
  await page.route("**/api/**", (route) => {
    const url = new URL(route.request().url());
    if (url.pathname === "/api/session")
      return route.fulfill({
        json: { authenticated: true, mode: "local", user: null, csrf: null },
      });
    if (url.pathname === "/api/repos")
      return route.fulfill({
        json: {
          repositories: [
            {
              id: "00000000-0000-4000-8000-000000000099",
              owner: "team",
              name: "project",
              description: "",
              access: "read",
              can_admin: false,
              archived: false,
              archive_version: 0,
              protection_version: 0,
              protected_branches: [],
            },
          ],
        },
      });
    if (url.pathname === "/api/repos/team/project/results") {
      const request: ResultsRead = JSON.parse(url.searchParams.get("request")!);
      if (request.kind === "reports")
        return route.fulfill({
          json: { kind: "reports", data: [activePayload.id] },
        });
      if (request.kind === "report")
        return route.fulfill({
          json: {
            kind: "report",
            data: { ...activePayload, charts: activePayload.report.charts },
          },
        });
      if (request.kind === "list")
        return route.fulfill({
          json: {
            kind: "page",
            data: {
              generation: "a".repeat(64),
              total: 0,
              columns: [],
              display_columns: [],
              rows: [],
              next_cursor: null,
            },
          },
        });
    }
    unexpected.push(url.pathname);
    return route.fulfill({
      status: 404,
      json: { error: { message: "Unexpected authored-plot fixture request" } },
    });
  });
  await page.goto(`/team/project/results?report=${payload.id}`);
  const workspace = page.getByLabel("Results workspace", { exact: true });
  const browserEvent = workspace.locator('section[aria-label="event"]');
  const browserHealthy = workspace.locator('section[aria-label="healthy"]');
  await expect(workspace.getByRole("alert")).toHaveCount(2);
  await expect(browserEvent.locator("g.mark-line path")).toHaveCount(1);
  await browserEvent.getByRole("checkbox").click();
  await expect(workspace.getByRole("alert")).toHaveCount(3);
  await expect(browserEvent.locator("svg")).toHaveCount(0);
  await expect(browserHealthy.locator("g.mark-line path")).toHaveCount(1);
  await expect(workspace).toContainText("0.25");
  await expect(page.getByLabel("Open immutable report")).toHaveValue(
    payload.id,
  );
  for (const width of [1280, 390]) {
    await page.setViewportSize({ width, height: 900 });
    await expect
      .poll(() => page.evaluate(() => document.documentElement.scrollWidth))
      .toBeLessThanOrEqual(width);
    await page.screenshot({
      path: path.join(evidence!, `browser/authored-results-${width}.png`),
      fullPage: true,
    });
  }
  await page.reload();
  await expect(workspace.getByRole("alert")).toHaveCount(2);
  await expect(browserEvent.locator("g.mark-line path")).toHaveCount(1);
  await expect(browserHealthy.locator("g.mark-line path")).toHaveCount(1);
  expect(errors).toEqual([]);
  expect(unexpected).toEqual([]);

  const partialFile = path.join(evidence!, "partial-template.html");
  const partial: typeof payload = JSON.parse(
    readFileSync(partialFile, "utf8").match(
      /<script id="results-data" type="application\/json">([\s\S]*?)<\/script>/,
    )![1],
  );
  expect(partial.report.charts[0].state).toBe("not_captured");
  await page.goto("about:blank");
  await page.route(/^https?:/, denyNetwork);
  await page.goto(pathToFileURL(partialFile).href);
  const partialCharts = page.locator("#charts > section");
  await expect(partialCharts).toContainText(
    "Plot evidence was not captured for this run.",
  );
  await expect(partialCharts.locator("g.mark-line path")).toHaveCount(1);
  await expect(page.locator("#comparison")).toContainText("0.1");
  await expect(page.locator("#comparison")).toContainText("0.2");
  await expect(page.locator("#identity")).toContainText(partial.id);
  for (const width of [1280, 390]) {
    await page.setViewportSize({ width, height: 900 });
    await expect
      .poll(() => page.evaluate(() => document.documentElement.scrollWidth))
      .toBeLessThanOrEqual(width);
    await page.screenshot({
      path: path.join(
        evidence!,
        `browser/partial-template-offline-${width}.png`,
      ),
      fullPage: true,
    });
  }
  expect(attempted).toEqual([]);
  await page.unroute(/^https?:/, denyNetwork);
  activePayload = partial;
  await page.goto(`/team/project/results?report=${partial.id}`);
  await expect(workspace).toContainText(
    "Plot evidence was not captured for this run.",
  );
  await expect(workspace.locator("g.mark-line path")).toHaveCount(1);
  await expect(workspace).toContainText("0.1");
  await expect(workspace).toContainText("0.2");
  await expect(page.getByLabel("Open immutable report")).toHaveValue(
    partial.id,
  );
  await expect(workspace.getByRole("alert")).toHaveCount(0);
  for (const width of [1280, 390]) {
    await page.setViewportSize({ width, height: 900 });
    await expect
      .poll(() => page.evaluate(() => document.documentElement.scrollWidth))
      .toBeLessThanOrEqual(width);
    await page.screenshot({
      path: path.join(
        evidence!,
        `browser/partial-template-results-${width}.png`,
      ),
      fullPage: true,
    });
  }
  await page.reload();
  await expect(workspace.locator("g.mark-line path")).toHaveCount(1);
  await expect(workspace).toContainText(
    "Plot evidence was not captured for this run.",
  );
  expect(errors).toEqual([]);
  expect(unexpected).toEqual([]);
});
