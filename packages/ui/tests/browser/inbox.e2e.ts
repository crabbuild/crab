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
      actor,
      event: 14,
      at: 1_700_000_100_000,
      read: false,
      target: { kind: "pull", number: 1 },
      title: "Review the README",
      reason: "review_requested",
    },
    {
      repository,
      actor,
      event: 13,
      at: 1_700_000_060_000,
      read: false,
      target: { kind: "pull_reply", pull: 1, thread: 1, number: 86 },
      title: "Reply about the README",
      reason: "mention",
    },
    {
      repository: second,
      actor,
      event: 3,
      at: 1_700_000_000_000,
      read: false,
      target: { kind: "issue_comment", issue: 2, number: 9 },
      title: "Library issue",
      reason: "mention",
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
    (url) => url.pathname === "/api/inbox",
    (route) => {
      const query = new URL(route.request().url()).searchParams;
      const selected = query.get("repository");
      const cursor = query.get("cursor");
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
          (query.get("unread") === "false" || !item.read),
      );
      const start = cursor ? 1 : 0;
      return route.fulfill({
        json: {
          items: cursor ? items.slice(start) : items.slice(0, 1),
          next: !cursor && items.length > 1 ? "fixture-cursor" : null,
          repositories: [repository, second],
          scope_required: false,
        },
      });
    },
  );
  await page.route(`**/api/inbox/${repository.id}/14`, (route) => {
    const request = route.request();
    expect(request.method()).toBe("PATCH");
    expect(request.headers()["x-csrf-token"]).toBe("fixture-csrf");
    const body = request.postDataJSON();
    expect(body).toEqual({ at: notices[0].at, read: true });
    notices[0].read = body.read;
    if (loseReadReply) {
      loseReadReply = false;
      return route.abort("failed");
    }
    return route.fulfill({ json: { read: notices[0].read } });
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
          created_at: notices[1].at,
          updated_at: notices[1].at,
          can_edit: false,
        },
      }),
  );

  await page.goto("/team/project/pull/1");
  await page.getByRole("link", { name: "Notifications", exact: true }).click();
  await expect(
    page.getByRole("heading", { name: "Notifications" }),
  ).toBeVisible();
  const review = page.getByRole("link", { name: "Review the README" });
  await expect(review).toHaveAttribute("href", "/team/project/pull/1");
  await expectNoAccessibilityViolations(page);
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
