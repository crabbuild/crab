import { expect, test } from "@playwright/test";
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
