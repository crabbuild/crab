import { expect, test } from "@playwright/test";
import { expectNoAccessibilityViolations } from "./accessibility";
import { emptyReactions } from "./fixtures";
import { mockPullRequest } from "./pulls/fixture";

test("notifications preserve unread state and open the exact review reply", async ({
  page,
}) => {
  const { lifecycle, threads } = await mockPullRequest(page);
  const repository = {
    id: lifecycle.repositoryId,
    owner: "team",
    name: "project",
  };
  const second = {
    id: "00000000-0000-4000-8000-000000000075",
    owner: "team",
    name: "library",
  };
  const actor = { name: "Alice", profile: null };
  const notices = [
    {
      repository,
      sender: actor.name,
      id: "notice-14",
      href: "/team/project/pull/2",
      updated: { id: "activity-14", created_at_ms: 1_700_000_100_000 },
      read: false,
      title: "Review the README",
      reason: "review_requested",
    },
    {
      repository,
      sender: actor.name,
      id: "notice-13",
      href: "/team/project/pull/1/files?thread=1&reply=86",
      updated: { id: "activity-13", created_at_ms: 1_700_000_060_000 },
      read: false,
      title: "Reply about the README",
      reason: "mention",
    },
    {
      repository: second,
      sender: actor.name,
      id: "notice-3",
      href: "/team/library/issues/2#issuecomment-9",
      updated: { id: "activity-3", created_at_ms: 1_700_000_000_000 },
      read: false,
      title: "Library issue",
      reason: "mention",
    },
    {
      repository,
      sender: actor.name,
      id: "notice-2",
      href: "/team/project/issues/3",
      updated: { id: "activity-2", created_at_ms: 1_699_999_900_000 },
      read: false,
      title: "Assigned issue",
      reason: "assigned",
    },
  ];
  let failPage = true;
  let loseReadReply = true;
  await page.route("**/api/session", (route) =>
    route.fulfill({
      json: {
        authenticated: true,
        user: { name: "Bob", subject: "bob" },
        csrf: "fixture-csrf",
      },
    }),
  );
  await page.route(
    (url) => url.pathname === "/api/repos" && !url.search,
    (route) => route.fulfill({ json: { repositories: [repository, second] } }),
  );
  await page.route("**/api/notifications/unread", (route) =>
    route.fulfill({ json: { unread: notices.some((notice) => !notice.read) } }),
  );
  await page.route(
    (url) => url.pathname === "/api/notifications",
    (route) => {
      const query = new URL(route.request().url()).searchParams;
      const selected = query.get("repository");
      const cursor = query.get("before");
      const reason = query.get("reason");
      if (cursor && failPage) {
        failPage = false;
        return route.fulfill({
          status: 503,
          json: { error: { message: "Inbox unavailable" } },
        });
      }
      const items = notices.filter(
        (item) =>
          (!selected || selected === item.repository.id) &&
          (!reason || reason === item.reason) &&
          (query.get("unread") === "false" || !item.read),
      );
      const start = cursor ? 1 : 0;
      return route.fulfill({
        json: {
          items: cursor ? items.slice(start) : items.slice(0, 1),
          next: !cursor && items.length > 1 ? "fixture-cursor" : null,
        },
      });
    },
  );
  await page.route("**/api/notifications/notice-14/read", (route) => {
    const request = route.request();
    expect(request.method()).toBe("PUT");
    expect(request.headers()["x-csrf-token"]).toBe("fixture-csrf");
    const body = request.postDataJSON();
    expect(body.repository_id).toBe(repository.id);
    if (body.expected.id !== notices[0].updated.id) {
      return route.fulfill({
        status: 409,
        json: {
          error: {
            code: "page_changed",
            message: "This list changed; reload from the first page",
          },
        },
      });
    }
    expect(body).toEqual({
      repository_id: repository.id,
      expected: notices[0].updated,
      read: true,
    });
    notices[0].read = body.read;
    if (loseReadReply) {
      loseReadReply = false;
      return route.abort("failed");
    }
    return route.fulfill({ json: null });
  });
  lifecycle.branchesAvailable = false;
  threads[0].resolved = true;
  threads[0].outdated = true;
  await page.route(
    (url) =>
      url.pathname === "/api/repos/team/project/pulls/1/threads/1/replies/86",
    (route) =>
      route.fulfill({
        json: {
          number: 86,
          author: actor,
          body: "@bob This is the specific reply.",
          reactions: emptyReactions(),
          version: 1,
          created_at: notices[1].updated.created_at_ms,
          updated_at: notices[1].updated.created_at_ms,
          can_edit: false,
        },
      }),
  );

  await page.goto("/team/project/pull/1");
  await page.getByRole("link", { name: /^Notifications/ }).click();
  await expect(
    page.getByRole("heading", { name: "Notifications" }),
  ).toBeVisible();
  const review = page.getByRole("link", { name: "Review the README" });
  await expect(review).toHaveAttribute("href", "/team/project/pull/2");
  const reason = page.getByLabel("Reason", { exact: true });
  await reason.selectOption("assigned");
  await expect(page.getByText(/Assignment · Updated by Alice/)).toBeVisible();
  await expect(
    page.getByRole("link", { name: "Assigned issue" }),
  ).toHaveAttribute("href", "/team/project/issues/3");
  await expect(review).toHaveCount(0);
  await page.reload();
  await expect(reason).toHaveValue("assigned");
  await expectNoAccessibilityViolations(page);
  await reason.selectOption("");
  await page.getByRole("button", { name: "Load older notifications" }).click();
  await expect(page.getByRole("alert")).toContainText("Inbox unavailable");
  await expect(review).toBeVisible();
  await page
    .getByRole("button", { name: "Retry loading older notifications" })
    .click();
  await expect(
    page.getByRole("link", { name: "Library issue" }),
  ).toHaveAttribute("href", "/team/library/issues/2#issuecomment-9");
  const reviewRow = page.getByRole("listitem").filter({ has: review });
  await reviewRow.getByRole("button", { name: "Mark as read" }).click();
  await expect(reviewRow.getByRole("alert")).toContainText(
    "The response was lost",
  );
  await reviewRow.getByRole("button", { name: "Mark as read" }).click();
  await expect(review).toHaveCount(0);

  await page.getByRole("link", { name: "All", exact: true }).click();
  await expect(
    reviewRow.getByRole("button", { name: "Mark as unread" }),
  ).toBeVisible();
  notices[0].updated = { id: "activity-15", created_at_ms: 1_700_000_200_000 };
  notices[0].read = false;
  await reviewRow.getByRole("button", { name: "Mark as unread" }).click();
  await expect(reviewRow.getByRole("alert")).toContainText("This list changed");
  await reviewRow.getByRole("button", { name: "Refresh inbox" }).click();
  await expect(
    reviewRow.getByRole("button", { name: "Mark as read" }),
  ).toBeVisible();
  await reviewRow.getByRole("button", { name: "Mark as read" }).click();
  await expect(
    reviewRow.getByRole("button", { name: "Mark as unread" }),
  ).toBeVisible();
  await page
    .getByRole("region", { name: "Notifications" })
    .getByLabel("Repository", { exact: true })
    .selectOption(second.id);
  await expect(page.getByRole("link", { name: "Library issue" })).toBeVisible();
  await expect(review).toHaveCount(0);
  await page.getByRole("link", { name: "Clear repository filter" }).click();
  await page.getByRole("link", { name: "Unread", exact: true }).click();
  await page.getByRole("link", { name: "Reply about the README" }).click();
  await expect(
    page.getByRole("region", { name: "Linked reply" }),
  ).toContainText("@bob This is the specific reply.");
  await expect(
    page.getByRole("region", { name: "Linked reply" }),
  ).toBeFocused();
  await expect(page.getByText("Consider a clearer opening.")).toBeVisible();
});
