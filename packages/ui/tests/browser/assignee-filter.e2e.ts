import { expect, test, type Page } from "@playwright/test";
import { mockPullRequest } from "./pulls/fixture";
import {
  expectNoAccessibilityViolations,
  selectDarkTheme,
} from "./accessibility";

async function fixture(page: Page) {
  await mockPullRequest(page);
  let fail = false;
  const requests: URLSearchParams[] = [];
  await page.route(/\/api\/repos\/team\/project\/(issues|pulls)\?/, (route) => {
    const q = new URL(route.request().url()).searchParams.get("q") ?? "is:open";
    const assigned =
      q.includes("assignee:@me") &&
      !q.includes(" OR ") &&
      !q.includes("-assignee");
    return route.fulfill({
      json: {
        items: [],
        next: null,
        matching: 0,
        counts: { open: 0, closed: 0, merged: 0 },
        authored_by_me: false,
        assigned_to_me: assigned,
        review_requested_from_me: false,
        navigation: {
          kind: "state",
          selected: q.includes("is:closed") ? "closed" : "open",
          open: q.replace("is:closed", "is:open"),
          closed: q.replace("is:open", "is:closed"),
        },
      },
    });
  });
  await page.route(
    "**/api/repos/team/project/discussion-search/assignees?**",
    (route) => {
      const params = new URL(route.request().url()).searchParams;
      requests.push(params);
      if (fail)
        return route.fulfill({
          status: 503,
          json: { error: { message: "Assignees unavailable" } },
        });
      const q = params.get("query") ?? "is:open";
      const base = q.replace(/(^| )(?:assignee:[^ ]+|no:assignee)/g, "").trim();
      const choice = (value: string) => ({
        query: `${base} ${value}`,
        selected: q.endsWith(value),
      });
      const search = params.get("search")?.toLowerCase() ?? "";
      return route.fulfill({
        json: {
          items:
            search === "unknown"
              ? []
              : [
                  {
                    person: {
                      id: "00000000-0000-4000-8000-000000000001",
                      name: "Public Alice",
                      profile: {
                        id: "00000000-0000-4000-8000-000000000001",
                        handle: "public-alice",
                        name: "Public Alice",
                      },
                    },
                    ...choice("assignee:public-alice"),
                  },
                  {
                    person: {
                      id: "00000000-0000-4000-8000-000000000002",
                      name: "Former member",
                      profile: null,
                    },
                    ...choice(
                      "assignee:actor/00000000-0000-4000-8000-000000000002",
                    ),
                  },
                ],
          self: choice("assignee:@me"),
          missing: choice("no:assignee"),
          any: choice("assignee:*"),
          clear: { query: base, selected: base === q },
          truncated: search === "",
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
  test(`${view} assignee menu filters historical people, unassigned work and personal views with keyboard navigation`, async ({
    page,
  }) => {
    const { requests } = await fixture(page);
    await page.goto(
      `/team/project/${view}?q=is%3Aopen&before=old&sort=comments&powerbar=1`,
    );
    const trigger = page.getByRole("button", {
      name: "Filter by assignee",
      exact: true,
    });
    expect(requests).toHaveLength(0);
    await trigger.click();
    const input = page.getByRole("combobox", { name: "Find a person" });
    await expect(input).toBeFocused();
    await expect(
      page.getByRole("option", { name: "Former member", exact: true }),
    ).toBeVisible();
    await expect(
      page.getByText("Showing 20 matches. Refine your search for more."),
    ).toBeVisible();
    await input.fill("PUBLIC");
    await expect.poll(() => requests.at(-1)?.get("search")).toBe("PUBLIC");
    await expect(
      page.getByText("Showing 20 matches. Refine your search for more."),
    ).toHaveCount(0);
    for (let i = 0; i < 4; i++) await input.press("ArrowDown");
    await input.press("Enter");
    await expect
      .poll(() => new URL(page.url()).searchParams.get("q"))
      .toBe("is:open assignee:public-alice");
    expect(new URL(page.url()).searchParams.get("before")).toBeNull();
    expect(new URL(page.url()).searchParams.get("sort")).toBe("comments");
    expect(new URL(page.url()).searchParams.get("powerbar")).toBe("1");
    await expect(trigger).toBeFocused();
    await page.reload();
    await trigger.click();
    await expect(
      page.getByRole("option", { name: "Public Alice @public-alice" }),
    ).toHaveAttribute("aria-selected", "true");
    await page
      .getByRole("option", { name: "Assigned to nobody", exact: true })
      .click();
    await expect
      .poll(() => new URL(page.url()).searchParams.get("q"))
      .toBe("is:open no:assignee");
    await trigger.click();
    await page
      .getByRole("option", { name: "Former member", exact: true })
      .click();
    await expect
      .poll(() => new URL(page.url()).searchParams.get("q"))
      .toBe("is:open assignee:actor/00000000-0000-4000-8000-000000000002");
    const nav = page.getByRole("navigation", {
      name: view === "issues" ? "Issue navigation" : "Pull request navigation",
      exact: true,
    });
    const personal = nav.getByRole("link", {
      name: "Assigned to me",
      exact: true,
    });
    await personal.click();
    await expect(personal).toHaveAttribute("aria-current", "page");
    await page.reload();
    await expect(personal).toHaveAttribute("aria-current", "page");
    await trigger.click();
    await page.getByRole("button", { name: "Clear assignee filter" }).click();
    await expect(personal).not.toHaveAttribute("aria-current");
  });
}

test("assignee menu exposes failures, retries and accessible mobile dark content", async ({
  page,
}) => {
  const state = await fixture(page);
  state.fail(true);
  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto("/team/project/issues?q=is%3Aopen&powerbar=1");
  await selectDarkTheme(page);
  const trigger = page.getByRole("button", {
    name: "Filter by assignee",
    exact: true,
  });
  await trigger.click();
  await expect(page.getByRole("alert")).toContainText("Assignees unavailable");
  state.fail(false);
  await page.getByRole("button", { name: "Retry people" }).click();
  await expect(
    page.getByRole("option", { name: "Public Alice @public-alice" }),
  ).toBeVisible();
  const box = await page
    .getByRole("dialog", { name: "Filter by assignee" })
    .boundingBox();
  expect(box!.x).toBeGreaterThanOrEqual(16);
  expect(box!.x + box!.width).toBeLessThanOrEqual(374);
  await expectNoAccessibilityViolations(page);
  const input = page.getByRole("combobox", { name: "Find a person" });
  await input.fill("unknown");
  await expect(page.getByText("No people match.")).toBeVisible();
  await input.press("Escape");
  await expect(trigger).toBeFocused();
  expect(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= innerWidth,
    ),
  ).toBe(true);
});
