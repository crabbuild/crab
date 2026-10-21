import { expect, test } from "@playwright/test";
import { mockPullRequest } from "./pulls/fixture";
import {
  expectNoAccessibilityViolations,
  selectDarkTheme,
} from "./accessibility";

const id = "01900000-0000-7000-8000-000000000001";
test("mixed list attribution links stable actors and follows renamed profiles without guessing unenrolled identities", async ({
  page,
}) => {
  await mockPullRequest(page);
  const profile = {
    id,
    handle: "public-alice",
    name: "Public Alice",
    version: 1,
    created_at_ms: 1700000000000,
    updated_at_ms: 1700000000000,
  };
  const shared = {
    number: 1,
    state: "open",
    created_at: 1700000000000,
    comment_count: 0,
    reaction_count: 0,
    labels: [],
    assignees: [],
  };
  await page.route("**/api/repos/team/project/issues?**", (route) =>
    route.fulfill({
      json: {
        items: [
          {
            ...shared,
            kind: "issue",
            title: "Registered author",
            author: {
              name: "Historical Alice",
              profile: {
                id: profile.id,
                handle: profile.handle,
                name: profile.name,
              },
            },
          },
          {
            ...shared,
            kind: "pull",
            title: "Unenrolled author",
            draft: false,
            head_ref: "refs/heads/feature",
            base_ref: "refs/heads/main",
            author: { name: "Unenrolled operator", profile: null },
          },
        ],
        counts: { open: 2, closed: 0, merged: 0 },
        matching: 2,
        next: null,
        navigation: { kind: "results" },
      },
    }),
  );
  await page.route(`**/api/actors/${id}`, (route) =>
    route.fulfill({ json: profile }),
  );
  await page.goto("/team/project/issues?q=is%3Aissue+OR+is%3Apr&powerbar=1");
  const rows = page.locator(".issue-list li");
  const author = rows
    .first()
    .getByRole("link", { name: "public-alice", exact: true });
  await expect(author).toHaveAttribute("href", `/-/users/${id}`);
  await expect(rows.nth(1)).toContainText("by Unenrolled operator");
  await expect(rows.nth(1).getByRole("link")).toHaveCount(1);
  profile.handle = "m".repeat(39);
  profile.name = "Renamed Alice";
  profile.version = 2;
  await page.reload();
  const renamed = rows
    .first()
    .getByRole("link", { name: profile.handle, exact: true });
  await expect(renamed).toHaveAttribute("href", `/-/users/${id}`);
  await expect(rows.first()).not.toContainText("Historical Alice");
  await page.setViewportSize({ width: 390, height: 844 });
  await selectDarkTheme(page);
  await page.getByRole("radio", { name: "Compact", exact: true }).click();
  const geometry = await page.locator(".issue-list-panel").evaluate((panel) => {
    const bounds = panel.getBoundingClientRect();
    return {
      scroll: panel.scrollLeft,
      contained: Array.from(
        panel.querySelectorAll(".issues-filters button, .discussion-author"),
      ).every((button) => {
        const rect = button.getBoundingClientRect();
        return rect.left >= bounds.left && rect.right <= bounds.right;
      }),
    };
  });
  expect(geometry).toEqual({ scroll: 0, contained: true });
  await page.mouse.move(0, 0);
  await page.keyboard.press("Tab");
  await page.keyboard.press("Escape");
  await expectNoAccessibilityViolations(page);
  await renamed.click();
  await expect(page).toHaveURL(new RegExp(`/-/users/${id}\\?powerbar=1$`));
  await expect(
    page.getByRole("heading", { name: "Renamed Alice", exact: true }),
  ).toBeVisible();
});

test("missing Directory preparation is a visible list failure with an explicit retry", async ({
  page,
}) => {
  await mockPullRequest(page);
  let ready = false;
  await page.route("**/api/repos/team/project/issues?**", (route) =>
    ready
      ? route.fulfill({
          json: {
            items: [],
            next: null,
            matching: 0,
            counts: { open: 0, closed: 0, merged: 0 },
            navigation: { kind: "results" },
          },
        })
      : route.fulfill({
          status: 503,
          json: {
            error: {
              code: "directory_unavailable",
              message:
                "Profiles are not ready. Contact your server administrator.",
            },
          },
        }),
  );
  await page.goto("/team/project/issues");
  await expect(page.getByRole("alert")).toContainText("Profiles are not ready");
  await expect(
    page.getByText("Results unavailable", { exact: true }),
  ).toBeVisible();
  ready = true;
  await page.getByRole("button", { name: "Try again", exact: true }).click();
  await expect(
    page.getByRole("heading", { name: "No results", exact: true }),
  ).toBeVisible();
});
