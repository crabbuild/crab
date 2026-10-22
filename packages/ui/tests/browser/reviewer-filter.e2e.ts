import { expect, test, type Page } from "@playwright/test";
import { mockPullRequest } from "./pulls/fixture";
import {
  expectNoAccessibilityViolations,
  selectDarkTheme,
} from "./accessibility";

const choices = [
  ["No reviews", "is:open review:none"],
  ["Review required", "is:open review:required"],
  ["Approved review", 'is:open label:"needs review" review:approved'],
  ["Changes requested", "is:open review:changes_requested"],
  ["Reviewed by you", "is:open reviewed-by:@me"],
  ["Not reviewed by you", "is:open -reviewed-by:@me"],
  ["Awaiting review from you", "is:open user-review-requested:@me"],
] as const;
async function fixture(page: Page) {
  await mockPullRequest(page);
  const state = {
    fail: false,
    limit: false,
    menuReads: 0,
    queries: [] as string[],
  };
  await page.route(
    (url) => url.pathname === "/api/repos/team/project/pulls",
    (route) => {
      const q =
        new URL(route.request().url()).searchParams.get("q") ?? "is:open";
      state.queries.push(q);
      return route.fulfill({
        json: {
          items: [],
          next: null,
          matching: 0,
          counts: { open: 0, closed: 0, merged: 0 },
          authored_by_me: false,
          assigned_to_me: false,
          review_requested_from_me: q === "is:open user-review-requested:@me",
          navigation: {
            kind: "state",
            selected: "open",
            open: q,
            closed: q.replace("is:open", "is:closed"),
          },
        },
      });
    },
  );
  await page.route(
    (url) =>
      url.pathname === "/api/repos/team/project/discussion-search/reviews",
    (route) => {
      state.menuReads++;
      if (state.fail)
        return route.fulfill({
          status: 503,
          json: { error: { message: "Review filters unavailable" } },
        });
      const query = new URL(route.request().url()).searchParams.get("query");
      return route.fulfill({
        json: {
          clear: { query: "is:open", selected: query === "is:open" },
          items: choices.map(([label, target]) => ({
            label,
            query: state.limit ? null : target,
            selected: query === target,
          })),
        },
      });
    },
  );
  return state;
}

test("the reference review menu uses native destinations and preserves navigation state and focus", async ({
  page,
}) => {
  const state = await fixture(page);
  await page.goto(
    "/team/project/pulls?q=is%3Aopen&sort=updated&direction=asc&before=old&powerbar=1",
  );
  expect(state.menuReads).toBe(0);
  const trigger = page.getByRole("button", {
    name: "Filter by reviews",
    exact: true,
  });
  await trigger.click();
  const menu = page.getByRole("menu", { name: "Filter by reviews" });
  for (const [label] of choices)
    await expect(
      menu.getByRole("menuitemradio", { name: label, exact: true }),
    ).toBeVisible();
  await expect(
    menu.getByRole("menuitemradio", {
      name: /Awaiting review from you or your team/,
    }),
  ).toBeDisabled();
  await menu
    .getByRole("menuitemradio", { name: "Approved review", exact: true })
    .click();
  await expect
    .poll(() => new URL(page.url()).searchParams.get("q"))
    .toBe('is:open label:"needs review" review:approved');
  const url = new URL(page.url());
  expect([
    url.searchParams.get("sort"),
    url.searchParams.get("direction"),
    url.searchParams.get("powerbar"),
    url.searchParams.has("before"),
  ]).toEqual(["updated", "asc", "1", false]);
  await expect(trigger).toBeFocused();
  await page.reload();
  await trigger.click();
  await expect(
    menu.getByRole("menuitemradio", { name: "Approved review", exact: true }),
  ).toHaveAttribute("aria-checked", "true");
  await menu
    .getByRole("menuitemradio", { name: "Not reviewed by you", exact: true })
    .click();
  await expect
    .poll(() => new URL(page.url()).searchParams.get("q"))
    .toBe("is:open -reviewed-by:@me");
  await trigger.click();
  await menu
    .getByRole("menuitemradio", {
      name: "Awaiting review from you",
      exact: true,
    })
    .click();
  await expect(
    page
      .getByRole("navigation", { name: "Pull request navigation", exact: true })
      .getByRole("link", { name: "Review requests", exact: true }),
  ).toHaveAttribute("aria-current", "page");
  await trigger.click();
  await menu
    .getByRole("menuitem", { name: "Clear review filters", exact: true })
    .click();
  await expect
    .poll(() => new URL(page.url()).searchParams.get("q"))
    .toBe("is:open");
  await trigger.click();
  const search = page.getByRole("combobox");
  await search.click();
  await expect(menu).toHaveCount(0);
  await expect(search).toBeFocused();
  await search.press("End");
  await search.press("!");
  await expect(search).toHaveValue("is:open!");
  await page.keyboard.press("Escape");
  await expectNoAccessibilityViolations(page);
});

test("review choices retry failed reads and keep keyboard focus within a narrow dark menu", async ({
  page,
}) => {
  const state = await fixture(page);
  state.fail = true;
  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto("/team/project/pulls?q=is%3Aopen&powerbar=1");
  await selectDarkTheme(page);
  const trigger = page.getByRole("button", {
    name: "Filter by reviews",
    exact: true,
  });
  await trigger.click();
  await expect(page.getByRole("alert")).toContainText(
    "Review filters unavailable",
  );
  state.fail = false;
  const retry = page.getByRole("menuitem", { name: "Retry review filters" });
  await retry.focus();
  await page.keyboard.press("Enter");
  const menu = page.getByRole("menu", { name: "Filter by reviews" });
  await expect(
    menu.getByRole("menuitemradio", { name: "No reviews", exact: true }),
  ).toBeVisible();
  const box = await menu.boundingBox();
  expect(box!.x).toBeGreaterThanOrEqual(16);
  expect(box!.x + box!.width).toBeLessThanOrEqual(374);
  await expectNoAccessibilityViolations(page);
  await page.keyboard.press("Escape");
  await expect(trigger).toBeFocused();
  state.limit = true;
  await trigger.click();
  await expect(
    menu.getByRole("menuitemradio", { name: "No reviews", exact: true }),
  ).toBeDisabled();
  await expect(menu).toContainText("Shorten the search query");
  expect(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= innerWidth,
    ),
  ).toBe(true);
});
