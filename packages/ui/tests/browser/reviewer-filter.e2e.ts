import { openSecondaryFilter } from "./discussion-filters";
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
  ["Awaiting review from you or your team", "is:open review-requested:@me"],
] as const;
async function fixture(page: Page, view: "issues" | "pulls" = "pulls") {
  await mockPullRequest(page);
  const state = {
    fail: false,
    limit: false,
    menuReads: 0,
    queries: [] as string[],
  };
  await page.route(
    (url) => url.pathname === `/api/repos/team/project/${view}`,
    (route) => {
      const q =
        new URL(route.request().url()).searchParams.get("q") ?? "is:open";
      state.queries.push(q);
      const base = q.replace(/ is:(locked|unlocked)/g, "");
      return route.fulfill({
        json: {
          items: [],
          next: null,
          matching: 0,
          counts: { open: 0, closed: 0, merged: 0 },
          authored_by_me: false,
          assigned_to_me: false,
          review_requested_from_me: q === "is:open user-review-requested:@me",
          lock_filter: {
            any: { query: base, selected: q === base },
            locked: {
              query: `${base} is:locked`,
              selected: q.endsWith(" is:locked"),
            },
            unlocked: {
              query: `${base} is:unlocked`,
              selected: q.endsWith(" is:unlocked"),
            },
          },
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

test("the reference review filter uses native destinations and preserves navigation state and focus", async ({
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
  await openSecondaryFilter(trigger);
  const menu = page.getByRole("dialog", { name: "Filter by reviews" });
  for (const [label] of choices)
    await expect(
      menu.getByRole("option", { name: label, exact: true }),
    ).toBeVisible();
  await menu
    .getByRole("option", {
      name: "Awaiting review from you or your team",
      exact: true,
    })
    .click();
  await expect
    .poll(() => new URL(page.url()).searchParams.get("q"))
    .toBe("is:open review-requested:@me");
  await openSecondaryFilter(trigger);
  await menu
    .getByRole("option", { name: "Approved review", exact: true })
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
  await openSecondaryFilter(trigger);
  await expect(
    menu.getByRole("option", { name: "Approved review", exact: true }),
  ).toHaveAttribute("aria-selected", "true");
  await menu
    .getByRole("option", { name: "Not reviewed by you", exact: true })
    .click();
  await expect
    .poll(() => new URL(page.url()).searchParams.get("q"))
    .toBe("is:open -reviewed-by:@me");
  await openSecondaryFilter(trigger);
  await menu
    .getByRole("option", {
      name: "Awaiting review from you",
      exact: true,
    })
    .click();
  await expect(
    page
      .getByRole("navigation", { name: "Pull request navigation", exact: true })
      .getByRole("link", { name: "Review requests", exact: true }),
  ).toHaveAttribute("aria-current", "page");
  await openSecondaryFilter(trigger);
  await menu
    .getByRole("button", { name: "Clear review filters", exact: true })
    .click();
  await expect
    .poll(() => new URL(page.url()).searchParams.get("q"))
    .toBe("is:open");
  await openSecondaryFilter(trigger);
  const search = page
    .getByRole("search", { name: "Search pulls" })
    .getByRole("combobox");
  await search.click();
  await expect(menu).toHaveCount(0);
  await expect(search).toBeFocused();
  await search.press("End");
  await search.press("!");
  await expect(search).toHaveValue("is:open!");
  await page.keyboard.press("Escape");
  await expectNoAccessibilityViolations(page);
});

for (const view of ["issues", "pulls"] as const) {
  test(`${view} clears conversation filters from the picker input`, async ({
    page,
  }) => {
    await fixture(page, view);
    await page.goto(
      `/team/project/${view}?q=is%3Aopen%20label%3Abug%20is%3Alocked&sort=updated`,
    );
    const trigger = page.getByRole("button", {
      name: "Filter by conversation lock",
    });
    await expect(trigger).toHaveText("Locked");
    await openSecondaryFilter(trigger);
    const dialog = page.getByRole("dialog", {
      name: "Filter by conversation lock",
    });
    const input = dialog.getByRole("combobox");
    const field = dialog.locator('[data-slot="input-group"]');
    await input.fill("unlocked");
    await expect(
      dialog.getByRole("option", { name: "Unlocked", exact: true }),
    ).toBeVisible();
    await expect(
      dialog.getByRole("option", { name: "Locked", exact: true }),
    ).toHaveCount(0);
    await field.getByRole("button", { name: "Clear filter search" }).click();
    await expect(input).toBeFocused();
    await expect(input).toHaveValue("");
    await field
      .getByRole("button", { name: "Clear conversation filter" })
      .click();
    await expect
      .poll(() => new URL(page.url()).searchParams.get("q"))
      .toBe("is:open label:bug");
    expect(new URL(page.url()).searchParams.get("sort")).toBe("updated");
    await expect(trigger).toBeFocused();
  });
}

test("review choices retry failed reads and keep keyboard focus within a narrow dark picker", async ({
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
  await openSecondaryFilter(trigger);
  await expect(page.getByRole("alert")).toContainText(
    "Review filters unavailable",
  );
  state.fail = false;
  const retry = page.getByRole("button", { name: "Retry review filters" });
  await retry.focus();
  await page.keyboard.press("Enter");
  const menu = page.getByRole("dialog", { name: "Filter by reviews" });
  await expect(
    menu.getByRole("option", { name: "No reviews", exact: true }),
  ).toBeVisible();
  const box = await menu.boundingBox();
  expect(box!.x).toBeGreaterThanOrEqual(16);
  expect(box!.x + box!.width).toBeLessThanOrEqual(374);
  await expectNoAccessibilityViolations(page);
  await page.keyboard.press("Escape");
  await expect(trigger).toBeFocused();
  state.limit = true;
  await openSecondaryFilter(trigger);
  await expect(
    menu.getByRole("option", { name: "No reviews", exact: true }),
  ).toBeDisabled();
  await expect(menu).toContainText("Shorten the search query");
  expect(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= innerWidth,
    ),
  ).toBe(true);
});
