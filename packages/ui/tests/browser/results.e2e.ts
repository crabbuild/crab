import { expect, test } from "@playwright/test";
import { readFileSync, existsSync, mkdirSync, writeFileSync } from "node:fs";
import path from "node:path";
import type { Report, ResultsRead } from "@/lib/results-contracts";
import type { Preferences } from "../../../workflow-results/model";
import {
  expectNoAccessibilityViolations,
  contrastRatio,
  selectDarkTheme,
} from "./accessibility";

const evidence = process.env.CRAB_RESULTS_EVIDENCE;
const fixture = evidence && path.join(evidence, "report.json");
test.use({ channel: process.env.CI ? undefined : "chrome" });
test("Results renders retained run values and curves offline across themes and reloads", async ({
  page,
}) => {
  test.skip(
    !fixture || !existsSync(fixture),
    "Requires the real queued-run qualification report",
  );
  const report: Report = JSON.parse(readFileSync(fixture!, "utf8"));
  const html = readFileSync(path.join(evidence!, "report.html"), "utf8");
  const exported: { id: string; objects: Record<string, string> } = JSON.parse(
    html.match(
      /<script id="results-data" type="application\/json">([\s\S]*?)<\/script>/,
    )![1],
  );
  const reportId = exported.id;
  expect(report.renderer).toBeTruthy();
  await page.addInitScript(() => {
    const original = window.fetch;
    window.fetch = async (...args) => {
      const response = await original(...args);
      if (String(args[0]).includes("/results?")) {
        const text = response.text.bind(response);
        response.text = async () => {
          const value = await text();
          if (
            JSON.parse(value).kind === "report" &&
            !performance.getEntriesByName("charts-arrival").length
          )
            performance.mark("charts-arrival");
          return value;
        };
      }
      return response;
    };
    window.addEventListener(
      "pointerover",
      (event) => {
        if (
          event.target instanceof Element &&
          event.target.closest('g[class*="_voronoi"]') &&
          !performance.getEntriesByName("cursor-start").length
        )
          performance.mark("cursor-start");
      },
      true,
    );
    let initialized = false;
    let linked = false;
    const observer = new MutationObserver(() => {
      if (
        !initialized &&
        performance.getEntriesByName("charts-arrival").length &&
        document.querySelectorAll("g.mark-line path").length === 4
      ) {
        initialized = true;
        requestAnimationFrame(() =>
          performance.measure("charts-initial", "charts-arrival"),
        );
      }
      const labels = [...document.querySelectorAll("section p")]
        .map((label) => label.textContent ?? "")
        .filter((text) => text.startsWith("Cursor step:"));
      if (
        !linked &&
        performance.getEntriesByName("cursor-start").length &&
        labels.length === 2 &&
        labels[0] === labels[1] &&
        document.querySelectorAll("g.mark-rule.role-mark line").length >= 2
      ) {
        linked = true;
        requestAnimationFrame(() =>
          performance.measure("cursor-linked", "cursor-start"),
        );
      }
      if (initialized && linked) observer.disconnect();
    });
    observer.observe(document, {
      subtree: true,
      childList: true,
      attributes: true,
    });
  });
  const unexpected: string[] = [];
  const errors: string[] = [];
  let readFailure = "";
  let chartsUnavailable = false;
  page.on("pageerror", (error) => errors.push(error.message));
  await page.route("**/api/**", async (route) => {
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
              access: "write",
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
      if (request.kind === "list" && readFailure)
        return route.fulfill({
          status: 503,
          json: { error: { message: readFailure } },
        });
      if (request.kind === "reports")
        return route.fulfill({ json: { kind: "reports", data: [reportId] } });
      if (request.kind === "report")
        return route.fulfill({
          json: {
            kind: "report",
            data: { id: reportId, report, charts: report.charts },
          },
        });
      if (request.kind === "compare")
        return route.fulfill({
          json: {
            kind: "comparison",
            data: {
              comparison: report.comparison,
              charts: report.charts.map((chart) =>
                chartsUnavailable && chart.spec
                  ? {
                      ...chart,
                      state: "missing",
                      spec: null,
                      interaction: null,
                      points: 0,
                      displayed_points: 0,
                      reduction: null,
                      reason:
                        "Retained plot bytes are missing. Retrieve the run evidence, then refresh Results.",
                    }
                  : chart,
              ),
            },
          },
        });
      if (request.kind === "evidence") {
        const snapshot = report.comparison.rows.find(
          (row) => row.run.digest === request.args.run.digest,
        )?.run.snapshot;
        const attachment = snapshot?.attachments.find(
          (a) => a.path === request.args.path,
        );
        if (attachment?.hash && exported.objects[attachment.hash])
          return route.fulfill({
            json: {
              kind: "evidence",
              data: {
                attachment,
                text: null,
                reason: null,
                data_url: `data:image/svg+xml;base64,${exported.objects[attachment.hash]}`,
              },
            },
          });
      }
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
      json: { error: { message: "Unexpected Results request" } },
    });
  });
  await page.goto(`/team/project/results?report=${reportId}`);
  const workspace = page.getByLabel("Results workspace", { exact: true });
  await expect(workspace).toContainText("0.1");
  await expect(workspace).toContainText("0.2");
  await expect(workspace.locator("g.mark-line path")).toHaveCount(4);
  await expect(workspace.locator("figure img")).toHaveCount(2);
  await expect(workspace.getByRole("alert")).toHaveCount(0);
  await workspace
    .locator('g[class*="_voronoi"] path')
    .first()
    .hover({ force: true });
  await expect(workspace.getByText(/^Cursor step:/)).toHaveCount(2);
  const cursors = await workspace.getByText(/^Cursor step:/).allTextContents();
  expect(cursors[0]).toEqual(cursors[1]);
  await expect
    .poll(() =>
      page.evaluate(() => performance.getEntriesByName("cursor-linked").length),
    )
    .toBe(1);
  const timings = await page.evaluate(() => ({
    charts_ms: performance.getEntriesByName("charts-initial")[0]?.duration,
    linked_cursor_ms:
      performance.getEntriesByName("cursor-linked")[0]?.duration,
  }));
  mkdirSync(path.join(evidence!, "browser"), { recursive: true });
  writeFileSync(
    path.join(evidence!, "browser/chart-timings.json"),
    JSON.stringify(timings),
  );
  expect(timings.charts_ms).toBeLessThanOrEqual(500);
  expect(timings.linked_cursor_ms).toBeLessThanOrEqual(100);
  const ranges = workspace.getByRole("form", { name: /^X range / });
  await expect(ranges).toHaveCount(2);
  for (const input of await workspace.getByLabel(/^X maximum /).all())
    await expect(input).toHaveValue("0.5");
  await expect(ranges.first()).toContainText("Report inspection is temporary");
  const zoom = ranges
    .first()
    .getByRole("button", { name: "Zoom in", exact: true });
  await zoom.focus();
  await page.evaluate(async () => {
    const url = new URL(location.href);
    url.searchParams.set("inspection", "metadata");
    history.replaceState(null, "", url);
    dispatchEvent(new PopStateEvent("popstate"));
    await new Promise<void>((resolve) =>
      requestAnimationFrame(() => requestAnimationFrame(() => resolve())),
    );
  });
  await expect(zoom).toBeFocused();
  await page.keyboard.press("Enter");
  await expect(zoom).toBeFocused();
  await page.screenshot({
    path: path.join(evidence!, "browser/metadata-focus.png"),
  });
  for (const input of await workspace.getByLabel(/^X minimum /).all())
    await expect(input).toHaveValue("0.125");
  // This inspects the compiled Vega scale, not just the controls.
  for (const svg of await workspace.locator(".crab-results-chart svg").all()) {
    const ticks = svg.locator('g[aria-label^="X-axis"] .role-axis-label text');
    await expect
      .poll(async () => {
        const values = (await ticks.allTextContents()).map(Number);
        return (
          values.length > 0 && values.every((x) => x >= 0.125 && x <= 0.375)
        );
      })
      .toBeTruthy();
  }
  await page.keyboard.press("Enter");
  await expect(zoom).toBeFocused();
  for (const input of await workspace.getByLabel(/^X minimum /).all())
    await expect(input).toHaveValue("0.1875");
  await ranges
    .first()
    .getByLabel(/^X minimum /)
    .fill("0.1");
  await ranges
    .first()
    .getByLabel(/^X maximum /)
    .fill("0.4");
  const apply = ranges
    .first()
    .getByRole("button", { name: "Apply range", exact: true });
  await apply.focus();
  await page.keyboard.press("Enter");
  await expect(apply).toBeFocused();
  await ranges
    .first()
    .getByRole("button", { name: "Reset range", exact: true })
    .click();
  for (const input of await workspace.getByLabel(/^X minimum /).all())
    await expect(input).toHaveValue("0");
  for (const input of await workspace.getByLabel(/^X maximum /).all())
    await expect(input).toHaveValue("0.5");
  await expect(page.getByLabel("Open immutable report")).toHaveValue(reportId);
  await expectNoAccessibilityViolations(page);
  mkdirSync(path.join(evidence!, "browser"), { recursive: true });
  await page.screenshot({
    path: path.join(evidence!, "browser/light.png"),
    fullPage: true,
  });
  const themePaints: Array<{
    scheme: string;
    background: string;
    minimum: number;
  }> = [];
  const expectReadablePlotText = async () => {
    const { scheme, background } = await page.evaluate(() => ({
      scheme: getComputedStyle(document.documentElement).colorScheme,
      background: getComputedStyle(document.body).backgroundColor,
    }));
    const fills = await workspace
      .locator(".crab-results-chart svg text")
      .evaluateAll((labels) =>
        labels.map((label) => getComputedStyle(label).fill),
      );
    expect(fills.length).toBeGreaterThan(0);
    const minimum = Math.min(
      ...fills.map((fill) => contrastRatio(fill, background)),
    );
    themePaints.push({ scheme, background, minimum });
    expect(minimum).toBeGreaterThanOrEqual(4.5);
  };
  await expectReadablePlotText();
  await selectDarkTheme(page);
  await expectReadablePlotText();
  await page.locator('[aria-label="Light"]').click();
  await expect(page.locator("html")).toHaveCSS("color-scheme", "light");
  await expectReadablePlotText();
  await selectDarkTheme(page);
  await expectReadablePlotText();
  writeFileSync(
    path.join(evidence!, "browser/theme-contrast.json"),
    JSON.stringify(themePaints, null, 2),
  );
  await expectNoAccessibilityViolations(page);
  await page.screenshot({
    path: path.join(evidence!, "browser/dark.png"),
    fullPage: true,
  });
  await page.getByText(/^Dataset and split identity/).click();
  await expect(page.getByLabel("Dataset split")).toHaveAttribute(
    "readonly",
    "",
  );
  // Full-page screenshots resize Chromium and can reset its forced-color
  // painting while the media query stays active. Capture an actual viewport.
  await page.setViewportSize({ width: 1280, height: 2400 });
  await page.emulateMedia({ forcedColors: "active", contrast: "more" });
  const forcedPainting = () =>
    page.evaluate(() => {
      const probe = document.createElement("span");
      probe.style.color = "CanvasText";
      probe.style.backgroundColor = "Canvas";
      document.body.append(probe);
      const expected = getComputedStyle(probe);
      const actual = getComputedStyle(document.body);
      const matches =
        matchMedia("(forced-colors: active)").matches &&
        actual.color === expected.color &&
        actual.backgroundColor === expected.backgroundColor;
      probe.remove();
      return matches;
    });
  await expect.poll(forcedPainting).toBeTruthy();
  await expect(workspace.locator("g.mark-line path")).toHaveCount(4);
  await page.screenshot({
    path: path.join(evidence!, "browser/high-contrast.png"),
  });
  await expect.poll(forcedPainting).toBeTruthy();
  writeFileSync(
    path.join(evidence!, "browser/high-contrast.json"),
    JSON.stringify(
      await workspace.locator("g.mark-line path").evaluateAll((paths) =>
        paths.map((path) => {
          const style = getComputedStyle(path);
          return { stroke: style.stroke, dash: style.strokeDasharray };
        }),
      ),
    ),
  );
  writeFileSync(
    path.join(evidence!, "browser/high-contrast-text.json"),
    JSON.stringify(
      await page.locator("h1 a").evaluate((element) => {
        const style = getComputedStyle(element);
        const parent = getComputedStyle(document.body);
        return {
          forced: matchMedia("(forced-colors: active)").matches,
          color: style.color,
          text_fill: style.webkitTextFillColor,
          background: parent.backgroundColor,
        };
      }),
    ),
  );
  // Axe 4.13 reads -webkit-text-fill-color, which Chromium leaves at the
  // authored dark value under forced colors. Check the painted SVG fills.
  const background = await page.evaluate(
    () => getComputedStyle(document.body).backgroundColor,
  );
  const fills = await workspace
    .locator("svg text")
    .evaluateAll((labels) =>
      labels.map((label) => getComputedStyle(label).fill),
    );
  expect(fills.length).toBeGreaterThan(0);
  expect(
    Math.min(...fills.map((fill) => contrastRatio(fill, background))),
  ).toBeGreaterThanOrEqual(4.5);
  const dashed = workspace.locator(
    "[data-crab-dashed-series] g.mark-line path",
  );
  await expect(dashed).toHaveCount(4);
  const signatures = await dashed.evaluateAll((paths) =>
    paths.map((path) => {
      const style = getComputedStyle(path);
      return `${style.stroke}:${style.strokeDasharray}`;
    }),
  );
  expect(new Set(signatures).size).toBeGreaterThanOrEqual(2);
  await page.emulateMedia({ forcedColors: "none", contrast: "no-preference" });
  await page.setViewportSize({ width: 390, height: 844 });
  const identityFields = await workspace
    .getByLabel("Dataset hash")
    .boundingBox();
  const splitField = await workspace.getByLabel("Dataset split").boundingBox();
  expect(identityFields!.width).toBeGreaterThan(240);
  expect(identityFields!.y).toBeLessThan(splitField!.y);
  const curves = workspace
    .locator("section")
    .filter({ has: page.locator("g.mark-line path") });
  await expect
    .poll(async () =>
      curves.evaluateAll((charts) =>
        charts.every(
          (chart) =>
            chart.querySelector("svg")!.getBoundingClientRect().width <=
            chart.clientWidth + 1,
        ),
      ),
    )
    .toBeTruthy();
  for (const row of report.comparison.rows) {
    await expect(curves.first().locator(".role-legend")).toContainText(
      row.run.snapshot.experiment_id.slice(-8),
    );
  }
  const table = workspace.getByRole("region", { name: "Metric comparison" });
  await table.focus();
  await page.keyboard.press("ArrowRight");
  await expect
    .poll(() => table.evaluate((element) => element.scrollLeft))
    .toBeGreaterThan(0);
  await expect(
    workspace.getByText(
      "Execution actions are unavailable in an immutable report.",
      { exact: false },
    ),
  ).toBeVisible();
  await expectNoAccessibilityViolations(page);
  expect(
    await page.evaluate(() => document.documentElement.scrollWidth),
  ).toBeLessThanOrEqual(390);
  await page.screenshot({
    path: path.join(evidence!, "browser/narrow.png"),
    fullPage: true,
  });
  await page.getByText("Columns", { exact: true }).click();
  const picker = page
    .locator("details[open]")
    .filter({ has: page.getByText("Columns", { exact: true }) });
  const pickerBox = await picker.locator("div").boundingBox();
  expect(pickerBox!.x).toBeGreaterThanOrEqual(0);
  expect(pickerBox!.x + pickerBox!.width).toBeLessThanOrEqual(390);
  await page.screenshot({
    path: path.join(evidence!, "browser/narrow-columns.png"),
    fullPage: true,
  });
  await page.getByText("Columns", { exact: true }).click();
  const debuggerSession = await page.context().newCDPSession(page);
  await debuggerSession.send("HeapProfiler.collectGarbage");
  const before = await debuggerSession.send("Runtime.getHeapUsage");
  for (let index = 0; index < 5; index++) {
    await page.getByLabel("Open immutable report").selectOption("");
    await expect(workspace.locator("g.mark-line path")).toHaveCount(0);
    await page.getByLabel("Open immutable report").selectOption(reportId);
    await expect(workspace.locator("g.mark-line path")).toHaveCount(4);
  }
  await debuggerSession.send("HeapProfiler.collectGarbage");
  const after = await debuggerSession.send("Runtime.getHeapUsage");
  writeFileSync(
    path.join(evidence!, "browser/lifecycle.json"),
    JSON.stringify({
      switches: 10,
      heap_before: before.usedSize,
      heap_after: after.usedSize,
      delta: after.usedSize - before.usedSize,
    }),
  );
  expect(after.usedSize - before.usedSize).toBeLessThan(16 * 1024 * 1024);
  await page.reload();
  await expect(workspace.locator("g.mark-line path")).toHaveCount(4);
  readFailure = `Retained evidence ${reportId} is unavailable. Check repository storage and retry.`;
  await workspace
    .getByRole("button", { name: "Refresh Results", exact: true })
    .click();
  const failure = workspace.getByRole("alert");
  await expect(failure).toContainText(reportId);
  await expect(
    failure.getByRole("button", { name: "Refresh view", exact: true }),
  ).toBeInViewport({ ratio: 1 });
  await expect(page.getByLabel("Open immutable report")).toHaveValue(reportId);
  expect(
    await page.evaluate(() => document.documentElement.scrollWidth),
  ).toBeLessThanOrEqual(390);
  await page.screenshot({
    path: path.join(evidence!, "browser/narrow-failure.png"),
    fullPage: true,
  });
  readFailure = "";
  await failure
    .getByRole("button", { name: "Refresh view", exact: true })
    .click();
  await expect(workspace.getByRole("alert")).toHaveCount(0);
  await expect(workspace.locator("g.mark-line path")).toHaveCount(4);
  await page.evaluate(
    ({ runs, spec }) => {
      const key = Object.keys(localStorage).find((key) =>
        key.startsWith("crab.results.v1."),
      );
      if (!key) throw new Error("Missing working comparison preferences");
      const previous: Preferences = JSON.parse(localStorage.getItem(key)!);
      const next: Preferences = {
        ...previous,
        selected: runs,
        spec,
        report: "",
      };
      localStorage.setItem(key, JSON.stringify(next));
    },
    {
      runs: report.comparison.rows.map((row) => ({
        id: row.run.snapshot.experiment_id,
        digest: row.run.digest,
      })),
      spec: report.comparison.spec,
    },
  );
  await page.goto("/team/project/results");
  await expect(workspace.locator("g.mark-line path")).toHaveCount(4);
  chartsUnavailable = true;
  await workspace
    .getByRole("button", { name: "Refresh Results", exact: true })
    .click();
  await expect(workspace).toContainText("Retained plot bytes are missing");
  await expect(workspace.locator("g.mark-line path")).toHaveCount(0);
  await page.screenshot({
    path: path.join(evidence!, "browser/unavailable-curves.png"),
    fullPage: true,
  });
  chartsUnavailable = false;
  await workspace
    .getByRole("button", { name: "Refresh Results", exact: true })
    .click();
  await expect(workspace.locator("g.mark-line path")).toHaveCount(4);
  expect(errors).toEqual([]);
  expect(unexpected).toEqual([]);
});

test("Results paints a bounded 200-row 32-column table within the interaction budget", async ({
  page,
}) => {
  test.skip(
    !evidence,
    "Requires a dedicated Results qualification output directory",
  );
  await page.addInitScript(() => {
    const original = window.fetch;
    window.fetch = async (...args) => {
      const response = await original(...args);
      if (String(args[0]).includes("/results?")) {
        const text = response.text.bind(response);
        response.text = async () => {
          const value = await text();
          if (JSON.parse(value).kind === "page")
            performance.mark("results-arrival");
          return value;
        };
      }
      return response;
    };
    const observe = () =>
      new MutationObserver(() => {
        if (
          document.querySelectorAll('input[aria-label^="Compare "]').length ===
            200 &&
          performance.getEntriesByName("results-arrival").length &&
          !performance.getEntriesByName("table-interactive").length
        ) {
          performance.measure("table-interactive", "results-arrival");
        }
      }).observe(document, { subtree: true, childList: true });
    observe();
  });
  const fields = Array.from({ length: 32 }, (_, index) => ({
    file: "scale.json",
    pointer: `/metric_${index}`,
    state: "present",
    value: index,
    reason: null,
  }));
  const columns = fields.map((field) => ({
    key: `metric:${JSON.stringify([field.file, field.pointer])}`,
    file: field.file,
    pointer: field.pointer,
    kind: "metric",
  }));
  const rows = Array.from({ length: 200 }, (_, index) => ({
    id: `00000000-0000-4000-8000-${String(index).padStart(12, "0")}`,
    digest: "a".repeat(64),
    name: `train ${index}`,
    origin: "captured",
    status: "success",
    base_commit: "b".repeat(40),
    started_at: "2026-11-18T00:00:00Z",
    ended_at: null,
    parameters: [],
    metrics: fields,
    capture_issues: [],
  }));
  await page.route("**/api/**", async (route) => {
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
    if (url.pathname.endsWith("/results")) {
      const request: ResultsRead = JSON.parse(url.searchParams.get("request")!);
      return route.fulfill({
        json:
          request.kind === "reports"
            ? { kind: "reports", data: [] }
            : {
                kind: "page",
                data: {
                  generation: "a".repeat(64),
                  total: 10000,
                  columns,
                  display_columns: columns.map((column) => column.key),
                  rows,
                  next_cursor: null,
                },
              },
      });
    }
    return route.fulfill({
      status: 404,
      json: { error: { message: "Unexpected scale fixture request" } },
    });
  });
  await page.goto("/team/project/results");
  await expect(page.getByRole("checkbox", { name: /^Compare / })).toHaveCount(
    200,
  );
  const elapsed = await page.evaluate(
    () => performance.getEntriesByName("table-interactive")[0]?.duration,
  );
  expect(elapsed).toBeDefined();
  mkdirSync(path.join(evidence!, "browser"), { recursive: true });
  writeFileSync(
    path.join(evidence!, "browser/table-scale.json"),
    JSON.stringify({
      runs: 10000,
      rows: 200,
      columns: 32,
      data_arrival_to_interactive_ms: elapsed,
      browser: await page.evaluate(() => navigator.userAgent),
    }),
  );
  expect(elapsed).toBeLessThanOrEqual(250);
});
