import { expect, test, type Page } from "@playwright/test";
import { mockPullRequest } from "./pulls/fixture";
import {
  expectNoAccessibilityViolations,
  selectDarkTheme,
} from "./accessibility";
async function fixture(page: Page) {
  await mockPullRequest(page);
  const reads: URLSearchParams[] = [];
  let fail = false;
  await page.route("**/api/repos/team/project/pulls?**", (route) => {
    const q = new URL(route.request().url()).searchParams.get("q") ?? "is:open";
    return route.fulfill({
      json: {
        items: [],
        next: null,
        matching: 0,
        counts: { open: 0, closed: 0, merged: 0 },
        authored_by_me: false,
        assigned_to_me: false,
        review_requested_from_me: q === "is:open review-requested:@me",
        navigation: {
          kind: "state",
          selected: "open",
          open: q,
          closed: q.replace("is:open", "is:closed"),
        },
      },
    });
  });
  await page.route(
    "**/api/repos/team/project/discussion-search/reviewers?**",
    (route) => {
      const p = new URL(route.request().url()).searchParams;
      reads.push(p);
      if (fail)
        return route.fulfill({
          status: 503,
          json: { error: { message: "Reviewers unavailable" } },
        });
      const qualifier = (p.get("relation") ?? "review_requested").replaceAll(
        "_",
        "-",
      );
      const q = p.get("query") ?? "is:open";
      const base = q
        .replace(
          /(^| )(?:(?:review-requested|reviewed-by|review-involves):[^ ]+)/g,
          "",
        )
        .trim();
      const choice = (handle: string) => ({
        query: `${base} ${qualifier}:${handle}`,
        selected: q === `${base} ${qualifier}:${handle}`,
      });
      return route.fulfill({
        json: {
          items:
            p.get("search") === "unknown"
              ? []
              : [
                  {
                    actor: {
                      id: "00000000-0000-4000-8000-000000000002",
                      handle: "public-bob",
                      name: "Public Bob",
                    },
                    ...choice("public-bob"),
                  },
                ],
          clear: { query: base, selected: q === base },
          self: choice("@me"),
          truncated: false,
        },
      });
    },
  );
  return {
    reads,
    fail: (value: boolean) => {
      fail = value;
    },
  };
}

test("reviewer menu switches native relationships and preserves sort, cursor reset, focus and reload", async ({
  page,
}) => {
  const { reads } = await fixture(page);
  await page.goto(
    "/team/project/pulls?q=is%3Aopen&sort=updated&direction=asc&before=old&powerbar=1",
  );
  expect(reads).toHaveLength(0);
  const trigger = page.getByRole("button", {
    name: "Filter by reviewer",
    exact: true,
  });
  await trigger.click();
  const input = page.getByRole("combobox", { name: "Find a username" });
  await expect(input).toBeFocused();
  await expect(
    page.getByRole("option", { name: "Public Bob @public-bob" }),
  ).toBeVisible();
  await page.getByRole("radio", { name: "Reviewed", exact: true }).click();
  await expect.poll(() => reads.at(-1)?.get("relation")).toBe("reviewed_by");
  await input.fill("PUBLIC");
  await expect.poll(() => reads.at(-1)?.get("search")).toBe("PUBLIC");
  await expect(
    page.getByRole("option", { name: "Public Bob @public-bob" }),
  ).toBeVisible();
  await input.press("ArrowDown");
  await input.press("ArrowDown");
  await input.press("Enter");
  await expect
    .poll(() => new URL(page.url()).searchParams.get("q"))
    .toBe("is:open reviewed-by:public-bob");
  expect(new URL(page.url()).searchParams.get("before")).toBeNull();
  expect(new URL(page.url()).searchParams.get("sort")).toBe("updated");
  expect(new URL(page.url()).searchParams.get("direction")).toBe("asc");
  expect(new URL(page.url()).searchParams.get("powerbar")).toBe("1");
  await expect(trigger).toBeFocused();
  await trigger.click();
  await page.getByRole("radio", { name: "Involved", exact: true }).click();
  await expect
    .poll(() => reads.at(-1)?.get("relation"))
    .toBe("review_involves");
  await page.getByRole("option", { name: "Yourself", exact: true }).click();
  await expect
    .poll(() => new URL(page.url()).searchParams.get("q"))
    .toBe("is:open review-involves:@me");
  const nav = page.getByRole("navigation", {
    name: "Pull request navigation",
    exact: true,
  });
  const pending = nav.getByRole("link", {
    name: "Review requests",
    exact: true,
  });
  await expect(pending).not.toHaveAttribute("aria-current");
  await pending.click();
  await expect(pending).toHaveAttribute("aria-current", "page");
  await page.reload();
  await expect(pending).toHaveAttribute("aria-current", "page");
  await trigger.click();
  await expect(
    page.getByRole("option", { name: "Yourself", exact: true }),
  ).toHaveAttribute("aria-selected", "true");
  await page.getByRole("button", { name: "Clear reviewer filter" }).click();
  await expect(pending).not.toHaveAttribute("aria-current");
});

test("reviewer menu reports failures and supports retry, no matches and mobile dark keyboard focus", async ({
  page,
}) => {
  const state = await fixture(page);
  state.fail(true);
  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto("/team/project/pulls?q=is%3Aopen&powerbar=1");
  await selectDarkTheme(page);
  const trigger = page.getByRole("button", {
    name: "Filter by reviewer",
    exact: true,
  });
  await trigger.click();
  await expect(page.getByRole("alert")).toContainText("Reviewers unavailable");
  state.fail(false);
  await page.getByRole("button", { name: "Retry reviewers" }).click();
  await expect(
    page.getByRole("option", { name: "Public Bob @public-bob" }),
  ).toBeVisible();
  const box = await page
    .getByRole("dialog", { name: "Filter by reviewer" })
    .boundingBox();
  expect(box!.x).toBeGreaterThanOrEqual(16);
  expect(box!.x + box!.width).toBeLessThanOrEqual(374);
  await expectNoAccessibilityViolations(page);
  const input = page.getByRole("combobox", { name: "Find a username" });
  await input.fill("unknown");
  await expect(page.getByText("No usernames match.")).toBeVisible();
  await input.press("Escape");
  await expect(trigger).toBeFocused();
  expect(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= innerWidth,
    ),
  ).toBe(true);
});
