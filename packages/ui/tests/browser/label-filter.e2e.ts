import { expect, test, type Page } from "@playwright/test";
import { mockPullRequest } from "./pulls/fixture";
import {
  expectNoAccessibilityViolations,
  selectDarkTheme,
} from "./accessibility";

const base = "is:issue is:open";
const bug = 'label:"Bug"';
const help = 'label:"Help wanted"';
async function fixture(page: Page) {
  await mockPullRequest(page);
  let fail = false;
  let hold = false;
  let release: (() => void) | undefined;
  const requests: string[] = [];
  const targets: Record<
    string,
    { bug: string; help: string; exclude: string }
  > = {
    [base]: {
      bug: `${base} ${bug}`,
      help: `${base} ${help}`,
      exclude: `${base} -${bug}`,
    },
    [`${base} ${bug}`]: {
      bug: base,
      help: `${base} ${bug} ${help}`,
      exclude: `${base} -${bug}`,
    },
    [`${base} ${bug} ${help}`]: {
      bug: `${base} ${help}`,
      help: `${base} ${bug}`,
      exclude: `${base} ${help} -${bug}`,
    },
    [`${base} ${help}`]: {
      bug: `${base} ${help} ${bug}`,
      help: base,
      exclude: `${base} ${help} -${bug}`,
    },
    [`${base} ${help} -${bug}`]: {
      bug: `${base} ${help} ${bug}`,
      help: `${base} -${bug}`,
      exclude: `${base} ${help}`,
    },
    [`${base} -${bug}`]: {
      bug: `${base} ${bug}`,
      help: `${base} -${bug} ${help}`,
      exclude: base,
    },
    [`${base} no:label`]: {
      bug: `${base} ${bug}`,
      help: `${base} ${help}`,
      exclude: `${base} -${bug}`,
    },
  };
  await page.route("**/api/repos/team/project/issues?**", async (route) => {
    const query = new URL(route.request().url()).searchParams.get("q") ?? base;
    requests.push(query);
    if (query === `${base} ${bug}`) {
      if (hold)
        await new Promise<void>((resolve) => {
          release = resolve;
        });
      else await new Promise((resolve) => setTimeout(resolve, 250));
    }
    const matching =
      query.includes(bug) && query.includes(help)
        ? 1
        : query.includes("no:label")
          ? 0
          : 2;
    return route.fulfill({
      json: {
        items: [],
        next: null,
        authored_by_me: false,
        matching,
        counts: { open: matching, closed: 0, merged: 0 },
        navigation: { kind: "results" },
      },
    });
  });
  await page.route(
    "**/api/repos/team/project/discussion-search/labels?**",
    async (route) => {
      const params = new URL(route.request().url()).searchParams;
      if (fail)
        return route.fulfill({
          status: 503,
          json: { error: { message: "Label catalog unavailable" } },
        });
      const source = params.get("query") ?? base;
      const target = targets[source];
      if (!target) throw new Error(`Unexpected query: ${source}`);
      const items = [
        {
          number: 1,
          title: "Bug",
          color: "d73a4a",
          description: "Something is not working",
          query: target.bug,
          selected: source.includes(bug) && !source.includes(`-${bug}`),
          excluded: source.includes(`-${bug}`),
          exclude_query: target.exclude,
        },
        {
          number: 2,
          title: "Help wanted",
          color: "008672",
          description: "Extra attention is needed",
          query: target.help,
          selected: source.includes(help),
          excluded: false,
          exclude_query: null,
        },
      ].filter((value) =>
        value.title
          .toLowerCase()
          .includes((params.get("search") ?? "").toLowerCase()),
      );
      return route.fulfill({
        headers: { "Server-Timing": "cell;dur=1.2" },
        json: {
          items,
          clear: { query: base, selected: source === base },
          missing: {
            query: source.includes("no:label") ? base : `${base} no:label`,
            selected: source.includes("no:label"),
          },
          truncated: false,
        },
      });
    },
  );
  return {
    requests,
    hold: () => {
      hold = true;
    },
    release: () => {
      hold = false;
      release?.();
    },
    fail: (value: boolean) => {
      fail = value;
    },
  };
}

test("label menu stays open across multi-selection and fresh counts while preserving URL navigation", async ({
  page,
}) => {
  await fixture(page);
  await page.goto(
    `/team/project/issues?q=${encodeURIComponent(base)}&sort=comments&direction=asc&before=old&powerbar=1`,
  );
  await page
    .getByRole("button", { name: "Filter by label", exact: true })
    .click();
  const input = page.getByRole("combobox", { name: "Filter labels" });
  await expect(input).toBeFocused();
  await page.getByRole("option", { name: /^Bug/ }).click();
  await expect
    .poll(() => new URL(page.url()).searchParams.get("q"))
    .toBe(`${base} ${bug}`);
  await expect(
    page.getByText("Loading results…", { exact: true }),
  ).toBeVisible();
  await expect(
    page.getByRole("dialog", { name: "Filter by label" }),
  ).toBeVisible();
  await expect(page.getByRole("option", { name: /^Bug/ })).toHaveAttribute(
    "aria-selected",
    "true",
  );
  await page.getByRole("option", { name: /^Help wanted/ }).click();
  await expect
    .poll(() => new URL(page.url()).searchParams.get("q"))
    .toBe(`${base} ${bug} ${help}`);
  await expect(
    page.getByRole("heading", { name: "1 result", exact: true }),
  ).toBeVisible();
  await expect(input).toBeFocused();
  await page
    .getByRole("option", { name: /^Bug/ })
    .click({ modifiers: ["Alt"] });
  await expect
    .poll(() => new URL(page.url()).searchParams.get("q"))
    .toBe(`${base} ${help} -${bug}`);
  await expect(page.getByRole("option", { name: /^Bug/ })).toContainText(
    "Excluded",
  );
  await input.press("Escape");
  const url = new URL(page.url());
  expect(url.searchParams.get("before")).toBeNull();
  expect(url.searchParams.get("sort")).toBe("comments");
  expect(url.searchParams.get("direction")).toBe("asc");
  expect(url.searchParams.get("powerbar")).toBe("1");
  await page.reload();
  await page
    .getByRole("button", { name: "Filter by label", exact: true })
    .click();
  await expect(page.getByRole("option", { name: /^Bug/ })).toContainText(
    "Excluded",
  );
  await page.getByRole("option", { name: "No labels", exact: true }).click();
  await expect
    .poll(() => new URL(page.url()).searchParams.get("q"))
    .toBe(`${base} no:label`);
  await expect(
    page.getByRole("option", { name: "No labels", exact: true }),
  ).toHaveAttribute("aria-selected", "true");
  await page.getByRole("button", { name: "Clear label filter" }).click();
  await expect.poll(() => new URL(page.url()).searchParams.get("q")).toBe(base);
});

test("label menu supports keyboard exclusion, remote search and mobile accessibility", async ({
  page,
}) => {
  await fixture(page);
  await page.goto(`/team/project/issues?q=${encodeURIComponent(base)}`);
  const trigger = page.getByRole("button", {
    name: "Filter by label",
    exact: true,
  });
  await trigger.click();
  const input = page.getByRole("combobox", { name: "Filter labels" });
  await expect(page.getByRole("option", { name: /^Bug/ })).toBeVisible();
  await input.press("ArrowDown");
  await input.press("ArrowDown");
  await input.press("Alt+Enter");
  await expect
    .poll(() => new URL(page.url()).searchParams.get("q"))
    .toBe(`${base} -${bug}`);
  await input.fill("wanted");
  await expect(
    page.getByRole("option", { name: /^Help wanted/ }),
  ).toBeVisible();
  await expect(page.getByRole("option", { name: /^Bug/ })).toHaveCount(0);
  await input.fill("unknown");
  await expect(
    page.getByText("No labels match.", { exact: true }),
  ).toBeVisible();
  await input.fill("");
  await selectDarkTheme(page);
  await page.setViewportSize({ width: 390, height: 844 });
  await trigger.click();
  await expect(page.getByRole("option", { name: /^Bug/ })).toBeVisible();
  const popup = await page
    .getByRole("dialog", { name: "Filter by label" })
    .boundingBox();
  expect(popup!.x).toBeGreaterThanOrEqual(16);
  expect(popup!.x + popup!.width).toBeLessThanOrEqual(374);
  await expectNoAccessibilityViolations(page);
  expect(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= window.innerWidth,
    ),
  ).toBe(true);
  await input.press("Escape");
  await expect(trigger).toBeFocused();
});

test("label choices show native errors and retry without inventing selections", async ({
  page,
}) => {
  const data = await fixture(page);
  data.fail(true);
  await page.goto(`/team/project/issues?q=${encodeURIComponent(base)}`);
  await page
    .getByRole("button", { name: "Filter by label", exact: true })
    .click();
  await expect(page.getByRole("alert")).toContainText(
    "Label catalog unavailable",
  );
  await expect(page.getByRole("option")).toHaveCount(0);
  data.fail(false);
  await page.getByRole("button", { name: "Retry labels", exact: true }).click();
  await expect(page.getByRole("option", { name: /^Bug/ })).toBeVisible();
  await page.route("**/api/repos/team/project/issues?**", (route) =>
    route.fulfill({
      status: 503,
      json: { error: { message: "Discussion list unavailable" } },
    }),
  );
  await page.getByRole("option", { name: /^Bug/ }).click();
  await expect(page.getByRole("alert")).toContainText(
    "Discussion list unavailable",
  );
  await expect(
    page.getByText("Results unavailable", { exact: true }),
  ).toBeVisible();
  await expect(page.getByText("Loading results…", { exact: true })).toHaveCount(
    0,
  );
});

for (const viewport of [
  { width: 1280, height: 900 },
  { width: 390, height: 844 },
]) {
  test(`a pressed label choice stays anchored when timing arrives at ${viewport.width}px`, async ({
    page,
  }) => {
    await page.setViewportSize(viewport);
    const data = await fixture(page);
    await page.goto(
      `/team/project/issues?q=${encodeURIComponent(base)}&powerbar=1`,
    );
    await expect(
      page.getByRole("heading", { name: "2 results", exact: true }),
    ).toBeVisible();
    data.hold();
    try {
      await page
        .getByRole("button", { name: "Filter by label", exact: true })
        .click();
      await page.getByRole("option", { name: /^Bug/ }).click();
      await expect(
        page.getByText("Loading results…", { exact: true }),
      ).toBeVisible();
      await expect(page.getByRole("option", { name: /^Bug/ })).toHaveAttribute(
        "aria-selected",
        "true",
      );
      const option = page.getByRole("option", { name: /^Help wanted/ });
      await option.click({ trial: true });
      const before = (await option.boundingBox())!;
      const popup = page.getByRole("dialog", { name: "Filter by label" });
      const anchor = (await popup.boundingBox())!;

      await page.mouse.move(
        before.x + before.width / 2,
        before.y + before.height / 2,
      );
      await page.mouse.down();
      data.release();
      await expect(
        page.getByRole("heading", { name: "2 results", exact: true }),
      ).toBeVisible();
      await page.evaluate(
        () =>
          new Promise<void>((resolve) =>
            requestAnimationFrame(() => requestAnimationFrame(() => resolve())),
          ),
      );
      const after = (await popup.boundingBox())!;
      expect(after.y).toBeCloseTo(anchor.y, 0);
      await page.mouse.up();
      await expect
        .poll(() => new URL(page.url()).searchParams.get("q"))
        .toBe(`${base} ${bug} ${help}`);
    } finally {
      data.release();
      await page.mouse.up();
    }
  });
}
