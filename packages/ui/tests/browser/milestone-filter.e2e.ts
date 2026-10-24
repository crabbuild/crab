import { expect, test, type Page } from "@playwright/test";
import { mockPullRequest } from "./pulls/fixture";
import {
  expectNoAccessibilityViolations,
  selectDarkTheme,
} from "./accessibility";

async function fixture(page: Page) {
  await mockPullRequest(page);
  let fail = false;
  const requests: Array<{ state: string; search: string }> = [];
  for (const kind of ["issues", "pulls"])
    await page.route(`**/api/repos/team/project/${kind}?**`, (route) =>
      route.fulfill({
        json: {
          items: [],
          next: null,
          counts: { open: 0, closed: 0, merged: 0 },
          authored_by_me: false,
          assigned_to_me: false,
          review_requested_from_me: false,
          matching: 0,
          navigation: { kind: "results" },
        },
      }),
    );
  await page.route(
    "**/api/repos/team/project/discussion-search/milestones?**",
    async (route) => {
      const params = new URL(route.request().url()).searchParams;
      const state = params.get("state")!;
      const search = params.get("search")!;
      requests.push({ state, search });
      if (fail)
        return route.fulfill({
          status: 503,
          json: {
            error: {
              code: "cell_unavailable",
              message: "Milestones unavailable",
            },
          },
        });
      const base = "is:open";
      const title = state === "closed" ? "Archived planning" : "Release one";
      const selected =
        params.get("query")?.includes(`milestone:"${title}"`) ?? false;
      return route.fulfill({
        json: {
          items: title.toLowerCase().includes(search.toLowerCase())
            ? [
                {
                  number: state === "closed" ? 2 : 1,
                  title,
                  state,
                  query: `${base} milestone:${JSON.stringify(title)}`,
                  selected,
                },
              ]
            : [],
          clear: {
            query: base,
            selected: !params.get("query")?.includes("milestone:"),
          },
          missing: {
            query: `${base} no:milestone`,
            selected: params.get("query")?.includes("no:milestone") ?? false,
          },
          truncated: false,
        },
      });
    },
  );
  return {
    requests,
    fail: (value: boolean) => {
      fail = value;
    },
  };
}

test("milestone filter preserves sort and diagnostics, clears cursors and supports keyboard selection and reload", async ({
  page,
}) => {
  await fixture(page);
  await page.goto(
    "/team/project/issues?q=is%3Aopen&before=old&sort=comments&direction=asc&powerbar=1",
  );
  const trigger = page.getByRole("button", {
    name: "Filter by milestone",
    exact: true,
  });
  await trigger.click();
  const input = page.getByRole("combobox", { name: "Find a milestone" });
  await expect(input).toBeFocused();
  await expect(page.getByRole("option", { name: "Release one" })).toBeVisible();
  await input.press("ArrowDown");
  await input.press("ArrowDown");
  await input.press("Enter");
  await expect
    .poll(() => new URL(page.url()).searchParams.get("q"))
    .toBe('is:open milestone:"Release one"');
  const url = new URL(page.url());
  expect(url.searchParams.get("before")).toBeNull();
  expect(url.searchParams.get("sort")).toBe("comments");
  expect(url.searchParams.get("direction")).toBe("asc");
  expect(url.searchParams.get("powerbar")).toBe("1");
  await page.reload();
  await trigger.click();
  await expect(
    page.getByRole("option", { name: "Release one" }),
  ).toHaveAttribute("aria-selected", "true");
  await page.getByRole("button", { name: "Clear milestone filter" }).click();
  await expect
    .poll(() => new URL(page.url()).searchParams.get("q"))
    .toBe("is:open");
  await trigger.click();
  await page.getByRole("option", { name: "No milestone", exact: true }).click();
  await expect
    .poll(() => new URL(page.url()).searchParams.get("q"))
    .toBe("is:open no:milestone");
});

test("milestone filter searches the native catalog across open and closed milestones and restores focus", async ({
  page,
}) => {
  const { requests } = await fixture(page);
  await page.goto("/team/project/pulls?q=is%3Aopen");
  const trigger = page.getByRole("button", {
    name: "Filter by milestone",
    exact: true,
  });
  await trigger.click();
  const input = page.getByRole("combobox", { name: "Find a milestone" });
  await input.fill("Archived");
  await expect(page.getByText("No open milestones match.")).toBeVisible();
  await page.getByRole("radio", { name: "Closed", exact: true }).click();
  await expect(
    page.getByRole("option", { name: "Archived planning" }),
  ).toBeVisible();
  await expect(input).toBeFocused();
  await selectDarkTheme(page);
  await page.setViewportSize({ width: 390, height: 844 });
  await trigger.click();
  await expect(
    page.getByRole("option", { name: "Archived planning" }),
  ).toBeVisible();
  await expect(input).toBeFocused();
  const popup = await page
    .getByRole("dialog", { name: "Filter by milestone" })
    .boundingBox();
  expect(popup).not.toBeNull();
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
  expect(
    requests.some(
      (value) => value.state === "closed" && value.search === "Archived",
    ),
  ).toBe(true);
});

test("milestone filters show failures and retry without fabricating options", async ({
  page,
}) => {
  const data = await fixture(page);
  data.fail(true);
  await page.goto("/team/project/issues?q=is%3Aopen");
  await page
    .getByRole("button", { name: "Filter by milestone", exact: true })
    .click();
  await expect(page.getByRole("alert")).toContainText("Milestones unavailable");
  await expect(page.getByRole("option")).toHaveCount(0);
  data.fail(false);
  await page.getByRole("button", { name: "Retry milestones" }).click();
  await expect(page.getByRole("option", { name: "Release one" })).toBeVisible();
});
