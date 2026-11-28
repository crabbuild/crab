import { expect, test } from "@playwright/test";
import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import path from "node:path";
import { pathToFileURL } from "node:url";
import type { View } from "vega";
import type { render } from "../../../../crates/crab-workflow/assets/results-runtime.mjs";

type Rendered = Omit<Awaited<ReturnType<typeof render>>, "view"> & {
  view: View;
};
type Embed = (...args: Parameters<typeof render>) => Promise<Rendered>;
declare global {
  interface Window {
    vegaEmbed: Embed;
    resultsChartProbe: {
      views: { result: Rendered; elapsed: number }[];
    };
  }
}

const evidence = process.env.CRAB_RESULTS_EVIDENCE;
test.use({ channel: process.env.CI ? undefined : "chrome" });

test.beforeEach(async ({ page }) => {
  test.skip(
    !evidence || !existsSync(path.join(evidence, "chart-scale.html")),
    "Requires the owner-generated maximum-overlay qualification export",
  );
  mkdirSync(path.join(evidence!, "browser"), { recursive: true });
  await page.addInitScript(() => {
    window.resultsChartProbe = { views: [] };
    let embed: Embed;
    Object.defineProperty(window, "vegaEmbed", {
      configurable: true,
      get: () => embed,
      set: (render: Embed) => {
        embed = async (...args) => {
          const start = performance.now();
          const result = await render(...args);
          await new Promise(requestAnimationFrame);
          window.resultsChartProbe.views.push({
            result,
            elapsed: performance.now() - start,
          });
          return result;
        };
      },
    });
  });
});

test("large matrices retain exact counts, normalization and distinct class identities offline", async ({
  page,
}) => {
  const attempted: string[] = [];
  const errors: string[] = [];
  await page.route(/^https?:/, (route) => {
    attempted.push(route.request().url());
    return route.abort();
  });
  page.on("pageerror", (error) => errors.push(error.message));
  await page.goto(
    pathToFileURL(path.join(evidence!, "chart-large-matrices.html")).href,
  );
  await expect
    .poll(() => page.evaluate(() => window.resultsChartProbe.views.length))
    .toBe(2);
  await expect(
    page.getByText(
      "Counted all 400000 retained observations into 16 observed class pairs. Missing class combinations are shown as zero.",
      { exact: true },
    ),
  ).toHaveCount(2);
  await expect(page.locator("g.mark-rect.role-mark path")).toHaveCount(32);
  const values = await page.locator("#charts section").evaluateAll((sections) =>
    Object.fromEntries(
      sections.map((section) => {
        const labels = [
          ...section.querySelectorAll("g.mark-rect.role-mark path"),
        ].map((cell) => cell.getAttribute("aria-label") ?? "");
        return [
          section.querySelector("h2")?.textContent,
          {
            counts: labels
              .map((label) =>
                Number(label.match(/count: ([\d,]+)/)?.[1].replaceAll(",", "")),
              )
              .sort((a, b) => a - b),
            normalized: labels
              .flatMap(
                (label) => label.match(/normalized: ([\d.e+-]+)/)?.[1] ?? [],
              )
              .map(Number)
              .sort((a, b) => a - b),
          },
        ];
      }),
    ),
  );
  const counts = [
    ...Array.from({ length: 6 }, () => 12500),
    ...Array.from({ length: 4 }, () => 25000),
    ...Array.from({ length: 6 }, () => 37500),
  ];
  expect(values.confusion.counts).toEqual(counts);
  expect(values.confusion_normalized.counts).toEqual(counts);
  const expected = [
    ...Array.from({ length: 4 }, () => 0.25),
    ...Array.from({ length: 2 }, () => 1 / 3),
    ...Array.from({ length: 2 }, () => 0.4),
    ...Array.from({ length: 2 }, () => 0.6),
    ...Array.from({ length: 2 }, () => 2 / 3),
    ...Array.from({ length: 4 }, () => 0.75),
  ];
  expect(values.confusion_normalized.normalized).toEqual(expected);
  await page.screenshot({
    path: path.join(evidence!, "browser/chart-large-matrices.png"),
    fullPage: true,
  });
  await page.setViewportSize({ width: 390, height: 844 });
  await expect
    .poll(() => page.evaluate(() => document.documentElement.scrollWidth))
    .toBeLessThanOrEqual(390);
  await page.screenshot({
    path: path.join(evidence!, "browser/chart-large-matrices-narrow.png"),
    fullPage: true,
  });
  await page.setViewportSize({ width: 1280, height: 820 });
  await page.goto(
    pathToFileURL(path.join(evidence!, "chart-matrix-identities.html")).href,
  );
  await expect(page.locator("g.mark-rect.role-mark path")).toHaveCount(16);
  const identities = await page
    .locator("g.mark-rect.role-mark path")
    .evaluateAll((cells) =>
      cells.map((cell) => cell.getAttribute("aria-label") ?? ""),
    );
  expect(identities.filter((label) => /count: 1$/.test(label))).toHaveLength(4);
  expect(identities.filter((label) => /count: 0$/.test(label))).toHaveLength(
    12,
  );
  for (const value of ["(missing)", '"null"', '"a|b"', '"b|c"']) {
    expect(identities.some((label) => label.includes(value))).toBe(true);
  }
  await page.screenshot({
    path: path.join(evidence!, "browser/chart-matrix-identities.png"),
    fullPage: true,
  });
  await page.setViewportSize({ width: 390, height: 844 });
  await expect
    .poll(() => page.evaluate(() => document.documentElement.scrollWidth))
    .toBeLessThanOrEqual(390);
  await page.screenshot({
    path: path.join(evidence!, "browser/chart-matrix-identities-narrow.png"),
    fullPage: true,
  });
  await page.setViewportSize({ width: 1280, height: 820 });
  await page.goto(
    pathToFileURL(path.join(evidence!, "chart-matrix-labels.html")).href,
  );
  await expect(page.locator("g.mark-rect.role-mark path")).toHaveCount(4);
  const labelCells = await page
    .locator("g.mark-rect.role-mark path")
    .evaluateAll((cells) =>
      cells.map((cell) => cell.getAttribute("aria-label") ?? ""),
    );
  expect(
    labelCells.every(
      (label) =>
        label.includes("X (count):") && label.includes("Y (normalized):"),
    ),
  ).toBe(true);
  expect(labelCells.filter((label) => /count: 1$/.test(label))).toHaveLength(2);
  const paintedText = await page.locator("#charts svg text").allTextContents();
  expect(paintedText).toContain('"datum.value"');
  expect(paintedText).toContain('"datum.label"');
  expect(
    paintedText.some((text) => text.startsWith("datum.value datum.label")),
  ).toBe(true);
  await page.screenshot({
    path: path.join(evidence!, "browser/chart-matrix-labels.png"),
    fullPage: true,
  });
  await page.setViewportSize({ width: 390, height: 844 });
  await expect
    .poll(() => page.evaluate(() => document.documentElement.scrollWidth))
    .toBeLessThanOrEqual(390);
  await page.screenshot({
    path: path.join(evidence!, "browser/chart-matrix-labels-narrow.png"),
    fullPage: true,
  });
  writeFileSync(
    path.join(evidence!, "browser/chart-large-matrices.json"),
    JSON.stringify(
      { values, identities, labelCells, paintedText, attempted, errors },
      null,
      2,
    ),
  );
  expect(attempted).toEqual([]);
  expect(errors).toEqual([]);
});

test("DVC templates render all selected fields with typed anchors and literal columns", async ({
  page,
}) => {
  const attempted: string[] = [];
  const errors: string[] = [];
  await page.route(/^https?:/, (route) => {
    attempted.push(route.request().url());
    return route.abort();
  });
  page.on("pageerror", (error) => errors.push(error.message));
  await page.goto(
    pathToFileURL(path.join(evidence!, "chart-template-parity.html")).href,
  );
  await expect
    .poll(() => page.evaluate(() => window.resultsChartProbe.views.length))
    .toBe(6);
  const counts = await page.locator("#charts section").evaluateAll((sections) =>
    Object.fromEntries(
      sections.map((section) => [
        section.querySelector("h2")?.textContent,
        {
          lines: section.querySelectorAll("g.mark-line path").length,
          bars: section.querySelectorAll("g.mark-rect.role-mark path").length,
        },
      ]),
    ),
  );
  expect(counts).toEqual({
    "curve.csv": { lines: 4, bars: 0 },
    linear: { lines: 4, bars: 0 },
    smooth: { lines: 4, bars: 0 },
    literal: { lines: 2, bars: 0 },
    "literal-default": { lines: 2, bars: 0 },
    bars: { lines: 0, bars: 4 },
  });
  const literal = page.locator("#charts section").filter({
    has: page.getByRole("heading", { name: "literal", exact: true }),
  });
  const point = literal.locator("g.mark-symbol.role-mark path").first();
  await point.scrollIntoViewIfNeeded();
  const box = await point.boundingBox();
  expect(box).not.toBeNull();
  await page.mouse.move(box!.x + box!.width / 2, box!.y + box!.height / 2);
  await expect(
    page.getByText("Cursor epoch.step[0]: 0", { exact: true }),
  ).toBeVisible();
  const bars = page
    .locator("#charts section")
    .filter({ has: page.getByRole("heading", { name: "bars", exact: true }) });
  await expect(
    bars.locator("g.mark-rect.role-mark path").first(),
  ).toHaveAttribute("aria-label", /score: [12]; label: (cat|dog)/);
  await page.screenshot({
    path: path.join(evidence!, "browser/chart-template-parity.png"),
    fullPage: true,
  });
  await page.setViewportSize({ width: 390, height: 844 });
  await expect
    .poll(() => page.evaluate(() => document.documentElement.scrollWidth))
    .toBeLessThanOrEqual(390);
  await page.screenshot({
    path: path.join(evidence!, "browser/chart-template-parity-narrow.png"),
    fullPage: true,
  });
  const authored: {
    name: string;
    spec: unknown;
    interaction: Parameters<typeof render>[2]["interaction"];
  }[] = JSON.parse(
    readFileSync(path.join(evidence!, "chart-authored-hover.json"), "utf8"),
  );
  expect(authored.map(({ name }) => name)).toEqual([
    "tooltip",
    "configured",
    "selected",
  ]);
  for (const fixture of authored) {
    await page.evaluate(async ({ name, spec, interaction }) => {
      const section = document.createElement("section");
      section.setAttribute("aria-label", `${name} hover`);
      section.style.maxWidth = "320px";
      const host = section.appendChild(document.createElement("div"));
      document.body.appendChild(section);
      await window.vegaEmbed(host, spec, {
        interaction,
        onError(error) {
          throw error;
        },
      });
    }, fixture);
    const chart = page.getByRole("region", { name: `${fixture.name} hover` });
    if (fixture.name === "selected") {
      const points = chart.locator("g.mark-symbol.role-mark path");
      await expect(points).toHaveCount(4);
      await points.first().hover();
      await expect
        .poll(() =>
          page.evaluate(() =>
            window.resultsChartProbe.views
              .at(-1)!
              .result.view.signal("selected"),
          ),
        )
        .toMatchObject({ rev: ["Run A"] });
      await expect(
        chart.locator('g.mark-line path[opacity="0.2"]'),
      ).toHaveCount(1);
      continue;
    }
    const targets = chart.locator('g[class*="_voronoi"] path');
    await expect(targets).toHaveCount(4);
    await targets.first().hover();
    await expect(chart.locator(".crab-results-chart")).toHaveAttribute(
      "title",
      /score: 91/,
    );
    await expect(chart.locator(".crab-results-chart")).toHaveAttribute(
      "title",
      /rev: Run A/,
    );
  }
  writeFileSync(
    path.join(evidence!, "browser/chart-template-parity.json"),
    JSON.stringify({ counts, attempted, errors }, null, 2),
  );
  expect(errors).toEqual([]);
  expect(attempted).toEqual([]);
});

test("maximum overlays retain all series within render and interaction budgets", async ({
  page,
}) => {
  const attempted: string[] = [];
  const errors: string[] = [];
  await page.route(/^https?:/, (route) => {
    attempted.push(route.request().url());
    return route.abort();
  });
  page.on("pageerror", (error) => errors.push(error.message));
  await page.goto(pathToFileURL(path.join(evidence!, "chart-scale.html")).href);
  await expect(page.locator("g.mark-line path")).toHaveCount(20);
  await expect(page.locator("#charts")).toContainText(
    /Showing \d+ sampled points of 40000/,
  );
  await expect
    .poll(() => page.evaluate(() => window.resultsChartProbe.views.length))
    .toBe(1);
  const timings = await page.evaluate(async () => {
    const { result, elapsed } = window.resultsChartProbe.views[0];
    const zoomStart = performance.now();
    result.view.signal("crab_domain", [500, 1500]);
    await result.view.runAsync();
    await new Promise(requestAnimationFrame);
    const zoom = performance.now() - zoomStart;
    const cursorStart = performance.now();
    result.view.signal("crab_shared_x", 1000);
    await result.view.runAsync();
    await new Promise(requestAnimationFrame);
    return {
      render_ms: elapsed,
      zoom_ms: zoom,
      cursor_ms: performance.now() - cursorStart,
    };
  });
  writeFileSync(
    path.join(evidence!, "browser/chart-scale-timings.json"),
    JSON.stringify(timings, null, 2),
  );
  expect(timings.render_ms).toBeLessThanOrEqual(500);
  expect(timings.zoom_ms).toBeLessThanOrEqual(100);
  expect(timings.cursor_ms).toBeLessThanOrEqual(100);
  await expect(page.locator("g.mark-rule.role-mark line")).toHaveCount(1);
  await page.getByRole("button", { name: "Zoom in", exact: true }).click();
  await expect(page.getByLabel(/^X minimum /)).toHaveValue("499.75");
  await page.screenshot({
    path: path.join(evidence!, "browser/chart-scale.png"),
    fullPage: true,
  });
  await page.setViewportSize({ width: 390, height: 844 });
  await expect
    .poll(() => page.evaluate(() => document.documentElement.scrollWidth))
    .toBeLessThanOrEqual(390);
  await expect
    .poll(() =>
      page
        .locator("#charts svg")
        .evaluate((svg) => svg.getBoundingClientRect().width),
    )
    .toBeLessThanOrEqual(326);
  await page.screenshot({
    path: path.join(evidence!, "browser/chart-scale-narrow.png"),
    fullPage: true,
  });
  expect(errors).toEqual([]);
  expect(attempted).toEqual([]);
});

test("offline plots preserve gaps and authored transformed and compound semantics", async ({
  page,
}) => {
  const attempted: string[] = [];
  const errors: string[] = [];
  await page.route(/^https?:/, (route) => {
    attempted.push(route.request().url());
    return route.abort();
  });
  page.on("pageerror", (error) => errors.push(error.message));
  await page.goto(
    pathToFileURL(path.join(evidence!, "chart-semantics.html")).href,
  );
  await expect
    .poll(() => page.evaluate(() => window.resultsChartProbe.views.length))
    .toBe(3);
  const gap = page.locator("#charts section").filter({
    has: page.getByRole("heading", { name: "gaps.csv", exact: true }),
  });
  const paths = await gap
    .locator("g.mark-line path")
    .evaluateAll((paths) => paths.map((path) => path.getAttribute("d") ?? ""));
  expect(paths).toHaveLength(1);
  expect(paths[0].match(/M/g)).toHaveLength(2);
  await expect(page.getByRole("form", { name: /^X range / })).toHaveCount(1);
  const observed = await page.evaluate(() => {
    const rows: Record<string, unknown>[] = [];
    const collect = (state: {
      data?: Record<string, unknown>;
      subcontext?: unknown[];
    }) => {
      for (const values of Object.values(state.data ?? {}))
        if (Array.isArray(values))
          rows.push(...(values as Record<string, unknown>[]));
      for (const child of state.subcontext ?? [])
        collect(child as typeof state);
    };
    for (const { result } of window.resultsChartProbe.views)
      collect(result.view.getState({ data: () => true, recurse: true }));
    return {
      scaled: [
        ...new Set(
          rows
            .filter((row) => typeof row.scaled_loss === "number")
            .map((row) => row.scaled_loss),
        ),
      ].sort((a, b) => Number(a) - Number(b)),
    };
  });
  expect(observed.scaled).toEqual([10, 20, 30, 40, 50]);
  const compound = page.locator("#charts section").filter({
    has: page.getByRole("heading", { name: "compound", exact: true }),
  });
  await expect(compound.locator('[aria-label$="Mean of loss: 3"]')).toHaveCount(
    1,
  );
  writeFileSync(
    path.join(evidence!, "browser/chart-semantics.json"),
    JSON.stringify(
      {
        gap_segments: paths[0].match(/M/g)?.length,
        scaled_values: observed.scaled,
        painted_mean: 3,
      },
      null,
      2,
    ),
  );
  await page.screenshot({
    path: path.join(evidence!, "browser/chart-semantics.png"),
    fullPage: true,
  });
  await page.setViewportSize({ width: 390, height: 844 });
  await expect
    .poll(() => page.evaluate(() => document.documentElement.scrollWidth))
    .toBeLessThanOrEqual(390);
  const manifest = page.getByText("Frozen evidence manifests", { exact: true });
  await manifest.click();
  await expect
    .poll(() => page.evaluate(() => document.documentElement.scrollWidth))
    .toBeLessThanOrEqual(390);
  await manifest.click();
  await page.screenshot({
    path: path.join(evidence!, "browser/chart-semantics-narrow.png"),
    fullPage: true,
  });
  await page.setViewportSize({ width: 1280, height: 820 });
  await page.goto(
    pathToFileURL(path.join(evidence!, "chart-builtins.html")).href,
  );
  await expect
    .poll(() => page.evaluate(() => window.resultsChartProbe.views.length))
    .toBe(4);
  const counts = await page.locator("#charts section").evaluateAll((sections) =>
    Object.fromEntries(
      sections.map((section) => [
        section.querySelector("h2")?.textContent,
        {
          lines: section.querySelectorAll("g.mark-line path").length,
          cells: section.querySelectorAll("g.mark-rect.role-mark path").length,
        },
      ]),
    ),
  );
  writeFileSync(
    path.join(evidence!, "browser/chart-builtin-counts.json"),
    JSON.stringify(counts, null, 2),
  );
  expect(counts).toEqual({
    "curve.csv": { lines: 2, cells: 0 },
    minimal: { lines: 2, cells: 0 },
    confusion: { lines: 0, cells: 8 },
    normalized: { lines: 0, cells: 8 },
  });
  const matrixValues = await page
    .locator("#charts section")
    .evaluateAll((sections) =>
      Object.fromEntries(
        sections
          .filter((section) =>
            ["confusion", "normalized"].includes(
              section.querySelector("h2")?.textContent ?? "",
            ),
          )
          .map((section) => [
            section.querySelector("h2")?.textContent,
            [...section.querySelectorAll("g.mark-rect.role-mark path")]
              .map((cell) =>
                Number(
                  cell
                    .getAttribute("aria-label")
                    ?.match(/(?:count|normalized): ([\d.e+-]+)/)?.[1],
                ),
              )
              .sort((a, b) => a - b),
          ]),
      ),
    );
  expect(matrixValues).toEqual({
    confusion: [0, 0, 1, 1, 2, 2, 3, 3],
    normalized: [0, 0, 0.25, 0.25, 0.75, 0.75, 1, 1],
  });
  const squareCells = await page
    .locator("g.mark-rect.role-mark path")
    .evaluateAll((cells) =>
      cells.every((cell) => {
        const box = cell.getBoundingClientRect();
        return Math.abs(box.width - box.height) <= 2;
      }),
    );
  expect(squareCells).toBe(true);
  const isolated = await page
    .locator("#charts section")
    .evaluateAll((sections) =>
      sections
        .filter((section) =>
          ["curve.csv", "minimal"].includes(
            section.querySelector("h2")?.textContent ?? "",
          ),
        )
        .every((section) => {
          const boxes = [...section.querySelectorAll("g.mark-line path")].map(
            (path) => path.getBoundingClientRect(),
          );
          return (
            boxes.length === 2 &&
            (boxes[0].bottom < boxes[1].top || boxes[1].bottom < boxes[0].top)
          );
        }),
    );
  expect(isolated).toBe(true);
  writeFileSync(
    path.join(evidence!, "browser/chart-builtin-values.json"),
    JSON.stringify({ matrixValues, isolated }, null, 2),
  );
  await page.screenshot({
    path: path.join(evidence!, "browser/chart-builtins.png"),
    fullPage: true,
  });
  await page.setViewportSize({ width: 390, height: 844 });
  await expect
    .poll(() => page.evaluate(() => document.documentElement.scrollWidth))
    .toBeLessThanOrEqual(390);
  const normalized = page.getByRole("region", {
    name: "normalized",
    exact: true,
  });
  await normalized.focus();
  await expect(normalized).toBeFocused();
  const scrollBefore = await normalized.evaluate(
    (section) => section.scrollLeft,
  );
  await page.keyboard.press("ArrowRight");
  await expect
    .poll(() => normalized.evaluate((section) => section.scrollLeft))
    .toBeGreaterThan(scrollBefore);
  await page.keyboard.press("ArrowLeft");
  await expect
    .poll(() => normalized.evaluate((section) => section.scrollLeft))
    .toBe(scrollBefore);
  await page.screenshot({
    path: path.join(evidence!, "browser/chart-builtins-narrow.png"),
    fullPage: true,
  });
  const authoredMarkExposed = await page.evaluate(async () => {
    const host = document.body.appendChild(document.createElement("div"));
    const result = await window.vegaEmbed(
      host,
      {
        $schema: "https://vega.github.io/schema/vega/v5.json",
        width: 100,
        height: 100,
        marks: [
          {
            name: "authored_brush",
            type: "rect",
            clip: true,
            encode: {
              enter: {
                x: { value: 0 },
                y: { value: 0 },
                width: { value: 50 },
                height: { value: 40 },
                description: { value: "Authored comparison value" },
              },
            },
          },
        ],
      },
      {
        onError: (error) => {
          throw error;
        },
      },
    );
    try {
      const mark = host.querySelector(
        '[aria-label="Authored comparison value"]',
      );
      return Boolean(mark && !mark.closest('[aria-hidden="true"]'));
    } finally {
      result.finalize();
      host.remove();
    }
  });
  expect(authoredMarkExposed).toBe(true);
  expect(errors).toEqual([]);
  expect(attempted).toEqual([]);
});
