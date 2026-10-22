import { expect, test, type Page } from "@playwright/test";
import { mockPullRequest } from "./pulls/fixture";
import {
  expectNoAccessibilityViolations,
  selectDarkTheme,
} from "./accessibility";

for (const view of ["issues", "pulls"] as const) {
  test(`${view} personal navigation follows native selection and keeps author filters through state, sort and reload`, async ({
    page,
  }) => {
    await mockPullRequest(page);
    const mine = view === "issues" ? "is:issue author:@me" : "author:@me";
    const open = `${mine} is:open`;
    const closed = `${mine} is:closed`;
    const mixed = "author:@me OR label:bug";
    const excluded = "-author:@me";
    const reads: URLSearchParams[] = [];
    await page.route(`**/api/repos/team/project/${view}?**`, (route) => {
      const params = new URL(route.request().url()).searchParams;
      reads.push(params);
      const query = params.get("q");
      const personal = query === open || query === closed;
      return route.fulfill({
        json: {
          items: [],
          next: null,
          matching: 0,
          counts: { open: 0, closed: 0, merged: 0 },
          authored_by_me: personal,
          assigned_to_me: false,
          review_requested_from_me: false,
          navigation: personal
            ? {
                kind: "state",
                selected: query === closed ? "closed" : "open",
                open,
                closed,
              }
            : { kind: "results" },
        },
      });
    });
    await page.route(
      "**/api/repos/team/project/discussion-search?**",
      (route) => {
        const query =
          new URL(route.request().url()).searchParams.get("query") ?? "";
        return route.fulfill({
          json: {
            start: 0,
            end: query.length,
            items: [
              { value: "author:public-alice", description: "Public Alice" },
            ],
            error: null,
          },
        });
      },
    );
    await page.goto(`/team/project/${view}?powerbar=1`);
    const nav = page.getByRole("navigation", {
      name: view === "issues" ? "Issue navigation" : "Pull request navigation",
      exact: true,
    });
    const personal = nav.getByRole("link", {
      name: view === "issues" ? "Created by me" : "Authored by me",
      exact: true,
    });
    await personal.click();
    await expect(personal).toHaveAttribute("aria-current", "page");
    expect(new URL(page.url()).searchParams.get("q")).toBe(open);
    const states = page.getByRole("navigation", {
      name: view === "issues" ? "Issue state" : "Pull request state",
      exact: true,
    });
    await states.getByRole("link", { name: "Closed 0", exact: true }).click();
    await expect.poll(() => reads.at(-1)?.get("q")).toBe(closed);
    await expect(personal).toHaveAttribute("aria-current", "page");
    await page.getByRole("button", { name: "Sort discussions" }).click();
    await page
      .getByRole("menuitemradio", { name: "Comments", exact: true })
      .click();
    await expect.poll(() => reads.at(-1)?.get("sort")).toBe("comments");
    expect(reads.at(-1)?.get("q")).toBe(closed);
    await page.reload();
    await expect(personal).toHaveAttribute("aria-current", "page");
    const search = page.getByRole("search", {
      name: view === "issues" ? "Search issues" : "Search pulls",
      exact: true,
    });
    const input = search.getByRole("combobox");
    for (const query of [mixed, excluded]) {
      await input.fill(query);
      await search.getByRole("button", { name: "Search", exact: true }).click();
      await expect.poll(() => reads.at(-1)?.get("q")).toBe(query);
      await expect(personal).not.toHaveAttribute("aria-current");
      await expect(
        nav.getByRole("link", {
          name: view === "issues" ? "Issues" : "Pull requests",
          exact: true,
        }),
      ).toHaveAttribute("aria-current", "page");
    }
    await input.fill("author:pu");
    await expect(
      page.getByRole("option", { name: "author:public-alice Public Alice" }),
    ).toBeVisible();
    await input.press("ArrowDown");
    await input.press("Enter");
    await expect(input).toHaveValue("author:public-alice ");
    await input.press("Enter");
    await expect.poll(() => reads.at(-1)?.get("q")).toBe("author:public-alice");
    expect(new URL(page.url()).searchParams.get("sort")).toBe("comments");
    expect(new URL(page.url()).searchParams.get("powerbar")).toBe("1");
    await page.setViewportSize({ width: 390, height: 844 });
    await selectDarkTheme(page);
    await expectNoAccessibilityViolations(page);
    expect(
      await page.evaluate(
        () => document.documentElement.scrollWidth <= innerWidth,
      ),
    ).toBe(true);
  });
}

async function authorMenuFixture(page: Page) {
  await mockPullRequest(page);
  let fail = false;
  const requests: Array<{ query: string; search: string }> = [];
  const actors = [
    {
      id: "a0000000-0000-4000-8000-000000000001",
      handle: "public-alice",
      name: "Public Alice",
    },
    ...Array.from({ length: 20 }, (_, index) => ({
      id: `a0000000-0000-4000-8000-${String(index + 2).padStart(12, "0")}`,
      handle: `user-${index}`,
      name: `Other author ${index}`,
    })),
  ];
  for (const view of ["issues", "pulls"])
    await page.route(`**/api/repos/team/project/${view}?**`, (route) =>
      route.fulfill({
        json: {
          items: [],
          next: null,
          matching: 0,
          counts: { open: 0, closed: 0, merged: 0 },
          authored_by_me: false,
          assigned_to_me: false,
          review_requested_from_me: false,
          navigation: { kind: "results" },
        },
      }),
    );
  await page.route(
    "**/api/repos/team/project/discussion-search/authors?**",
    (route) => {
      const params = new URL(route.request().url()).searchParams;
      const query = params.get("query") ?? "";
      const search = params.get("search") ?? "";
      requests.push({ query, search });
      if (fail)
        return route.fulfill({
          status: 503,
          json: {
            error: {
              code: "directory_unavailable",
              message: "Authors unavailable",
            },
          },
        });
      const matches = actors.filter((actor) =>
        actor.handle.startsWith(search.toLowerCase().replace(/^@/, "")),
      );
      return route.fulfill({
        json: {
          items: matches.slice(0, 20).map((actor) => ({
            actor,
            query: `is:open author:${actor.handle}`,
            selected: query === `is:open author:${actor.handle}`,
          })),
          self: {
            query: "is:open author:@me",
            selected: query === "is:open author:@me",
          },
          clear: { query: "is:open", selected: query === "is:open" },
          truncated: matches.length > 20,
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

for (const view of ["issues", "pulls"] as const) {
  test(`${view} author menu uses native choices and preserves navigation through selection and reload`, async ({
    page,
  }) => {
    const { requests } = await authorMenuFixture(page);
    await page.goto(
      `/team/project/${view}?q=is%3Aopen&sort=comments&direction=asc&before=old&powerbar=1`,
    );
    const trigger = page.getByRole("button", {
      name: "Filter by author",
      exact: true,
    });
    await trigger.click();
    const input = page.getByRole("combobox", { name: "Find a username" });
    await expect(input).toBeFocused();
    await expect(
      page.getByRole("option", { name: "Public Alice @public-alice" }),
    ).toBeVisible();
    await expect(
      page.getByText("Showing 20 matches. Refine your search for more."),
    ).toBeVisible();
    await input.fill("@PUBLIC");
    await expect.poll(() => requests.at(-1)?.search).toBe("@PUBLIC");
    await expect(
      page.getByText("Showing 20 matches. Refine your search for more."),
    ).toHaveCount(0);
    await input.press("ArrowDown");
    await input.press("ArrowDown");
    await input.press("Enter");
    await expect
      .poll(() => new URL(page.url()).searchParams.get("q"))
      .toBe("is:open author:public-alice");
    await expect(trigger).toBeFocused();
    const url = new URL(page.url());
    expect(url.searchParams.get("before")).toBeNull();
    expect(url.searchParams.get("sort")).toBe("comments");
    expect(url.searchParams.get("direction")).toBe("asc");
    expect(url.searchParams.get("powerbar")).toBe("1");
    await page.reload();
    await trigger.click();
    await expect(
      page.getByRole("option", { name: "Public Alice @public-alice" }),
    ).toHaveAttribute("aria-selected", "true");
    await page.getByRole("button", { name: "Clear author filter" }).click();
    await expect
      .poll(() => new URL(page.url()).searchParams.get("q"))
      .toBe("is:open");
    await trigger.click();
    await page.getByRole("option", { name: "Yourself", exact: true }).click();
    await expect
      .poll(() => new URL(page.url()).searchParams.get("q"))
      .toBe("is:open author:@me");
  });
}

test("author menu reports errors, retries, and stays accessible in a narrow dark viewport", async ({
  page,
}) => {
  const fixture = await authorMenuFixture(page);
  fixture.fail(true);
  await page.goto("/team/project/issues?q=is%3Aopen");
  const trigger = page.getByRole("button", {
    name: "Filter by author",
    exact: true,
  });
  await trigger.click();
  await expect(page.getByRole("alert")).toContainText("Authors unavailable");
  await expect(page.getByRole("option")).toHaveCount(0);
  fixture.fail(false);
  await page.getByRole("button", { name: "Retry authors" }).click();
  await expect(
    page.getByRole("option", { name: "Public Alice @public-alice" }),
  ).toBeVisible();
  const input = page.getByRole("combobox", { name: "Find a username" });
  await input.fill("unknown");
  await expect(page.getByText("No usernames match.")).toBeVisible();
  await input.press("Escape");
  await expect(trigger).toBeFocused();
  await page.setViewportSize({ width: 390, height: 844 });
  await selectDarkTheme(page);
  await trigger.click();
  const popup = await page
    .getByRole("dialog", { name: "Filter by author" })
    .boundingBox();
  expect(popup!.x).toBeGreaterThanOrEqual(16);
  expect(popup!.x + popup!.width).toBeLessThanOrEqual(374);
  await expectNoAccessibilityViolations(page);
  expect(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= innerWidth,
    ),
  ).toBe(true);
});
