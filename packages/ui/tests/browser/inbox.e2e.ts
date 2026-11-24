import { chooseSelectOption } from "./select";
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
      subject: { kind: "pull", number: 2 },
      href: "/team/project/pull/2",
      updated: { id: "activity-14", created_at_ms: 1_700_000_100_000 },
      read: false,
      saved: false,
      done: false,
      title: "Review the README",
      reason: "review_requested",
    },
    {
      repository,
      sender: actor.name,
      id: "notice-13",
      subject: { kind: "pull", number: 1 },
      href: "/team/project/pull/1/files?thread=1&reply=86",
      updated: { id: "activity-13", created_at_ms: 1_700_000_060_000 },
      read: false,
      saved: false,
      done: false,
      title: "Reply about the README",
      reason: "mention",
    },
    {
      repository: second,
      sender: actor.name,
      id: "notice-3",
      subject: { kind: "issue", number: 2 },
      href: "/team/library/issues/2#issuecomment-9",
      updated: { id: "activity-3", created_at_ms: 1_700_000_000_000 },
      read: false,
      saved: false,
      done: false,
      title: "Library issue",
      reason: "mention",
    },
    {
      repository,
      sender: actor.name,
      id: "notice-2",
      subject: { kind: "issue", number: 3 },
      href: "/team/project/issues/3",
      updated: { id: "activity-2", created_at_ms: 1_699_999_900_000 },
      read: false,
      saved: false,
      done: false,
      title: "Assigned issue",
      reason: "assigned",
    },
    {
      repository: second,
      sender: actor.name,
      id: "notice-1",
      subject: { kind: "issue_transfer", number: 4 },
      href: "/team/library/issues/4",
      updated: { id: "activity-1", created_at_ms: 1_699_999_800_000 },
      read: false,
      saved: false,
      done: false,
      title: "Moved issue",
      reason: "issue_transferred",
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
  await page.route("**/api/notifications/notice-14/state", (route) => {
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
      action: { read: true },
    });
    notices[0].read = body.action.read;
    if (loseReadReply) {
      loseReadReply = false;
      return route.abort("failed");
    }
    return route.fulfill({
      json: { read: notices[0].read, saved: false, done: false },
    });
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
  await chooseSelectOption(reason, "assigned");
  await expect(page.getByText(/Assignment · Updated by Alice/)).toBeVisible();
  await expect(
    page.getByRole("link", { name: "Assigned issue" }),
  ).toHaveAttribute("href", "/team/project/issues/3");
  await expect(review).toHaveCount(0);
  await page.reload();
  await expect(reason).toHaveText("Assignments");
  await expectNoAccessibilityViolations(page);
  await chooseSelectOption(reason, "issue_transferred");
  const transferred = page.getByRole("link", { name: "Moved issue" });
  await expect(transferred).toHaveAttribute("href", "/team/library/issues/4");
  const transferRow = page.getByRole("listitem").filter({ has: transferred });
  await expect(
    transferRow.getByText(/Issue transferred · Updated by Alice/),
  ).toBeVisible();
  await expect(
    transferRow.getByRole("button", { name: "Unsubscribe" }),
  ).toHaveCount(0);
  await chooseSelectOption(reason, "");
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
    page
      .getByRole("region", { name: "Notifications" })
      .getByLabel("Repository", { exact: true }),
  ).toBeFocused();
  await expect(
    reviewRow.getByRole("button", { name: "Mark as read" }),
  ).toBeVisible();
  await reviewRow.getByRole("button", { name: "Mark as read" }).click();
  await expect(
    reviewRow.getByRole("button", { name: "Mark as unread" }),
  ).toBeVisible();
  await chooseSelectOption(
    page
      .getByRole("region", { name: "Notifications" })
      .getByLabel("Repository", { exact: true }),
    second.id,
  );
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

test("saved and done views persist and restore a conversation", async ({
  page,
}) => {
  const { lifecycle } = await mockPullRequest(page);
  const notice = {
    repository: { id: lifecycle.repositoryId, owner: "team", name: "project" },
    id: "triage-thread",
    subject: { kind: "pull", number: 1 },
    href: "/team/project/pull/1",
    updated: { id: "activity-1", created_at_ms: 1_700_000_100_000 },
    sender: "Alice",
    title: "Dependency review",
    reason: "review_requested",
  };
  const state = { read: false, saved: false, done: false };
  await page.route("**/api/session", (route) =>
    route.fulfill({
      json: {
        authenticated: true,
        user: { name: "Bob", subject: "bob" },
        csrf: "triage-csrf",
      },
    }),
  );
  await page.route("**/api/notifications/unread", (route) =>
    route.fulfill({ json: { unread: !state.read && !state.done } }),
  );
  await page.route(
    (url) => url.pathname === "/api/notifications",
    (route) => {
      const query = new URL(route.request().url()).searchParams;
      const view = query.get("view");
      const visible =
        view === "saved"
          ? state.saved
          : view === "done"
            ? state.done
            : !state.done && (query.get("unread") !== "true" || !state.read);
      return route.fulfill({
        json: { items: visible ? [{ ...notice, ...state }] : [], next: null },
      });
    },
  );
  let loseDoneReply = true;
  await page.route("**/api/notifications/triage-thread/state", (route) => {
    const request = route.request();
    const body = request.postDataJSON();
    expect(request.method()).toBe("PUT");
    expect(request.headers()["x-csrf-token"]).toBe("triage-csrf");
    expect(body.repository_id).toBe(lifecycle.repositoryId);
    expect(body.expected).toEqual(notice.updated);
    const { save, done } = body.action;
    if (typeof save === "boolean") state.saved = save;
    if (typeof done === "boolean") state.done = done;
    if (done === true) {
      state.read = true;
      if (loseDoneReply) {
        loseDoneReply = false;
        return route.abort("failed");
      }
    }
    return route.fulfill({ json: state });
  });

  await page.goto("/-/notifications");
  const row = page.getByRole("listitem").filter({ hasText: notice.title });
  await row.getByRole("button", { name: "Save", exact: true }).click();
  await expect(row.getByRole("button", { name: "Unsave" })).toBeVisible();
  const saved = page.getByRole("link", { name: "Saved", exact: true });
  await saved.click();
  await page.reload();
  await expect(saved).toHaveAttribute("aria-current", "page");
  await expect(row.getByRole("link", { name: notice.title })).toBeVisible();
  await row.getByRole("button", { name: "Mark as done" }).click();
  await expect(row.getByRole("alert")).toContainText("The response was lost");
  await row.getByRole("button", { name: "Mark as done" }).click();
  await expect(
    row.getByRole("button", { name: "Move to inbox" }),
  ).toBeVisible();
  await expect(
    page.getByRole("link", { name: "Notifications", exact: true }),
  ).toBeVisible();
  await page.getByRole("link", { name: "Done", exact: true }).click();
  await expect(
    page.getByText("Done shows the last 90 days.", { exact: false }),
  ).toBeVisible();
  await expect(
    row.getByRole("button", { name: "Move to inbox" }),
  ).toBeVisible();
  await expectNoAccessibilityViolations(page);
  await row.getByRole("button", { name: "Move to inbox" }).click();
  await expect(page.getByText("No completed notifications.")).toBeVisible();
  await expect(
    page
      .getByRole("region", { name: "Notifications" })
      .getByLabel("Repository", { exact: true }),
  ).toBeFocused();

  await page.getByRole("link", { name: "All", exact: true }).click();
  await expect(
    row.getByRole("button", { name: "Mark as unread" }),
  ).toBeVisible();
  await saved.click();
  await row.getByRole("button", { name: "Unsave" }).click();
  await expect(page.getByText("No saved notifications.")).toBeVisible();
  await page.getByRole("link", { name: "All", exact: true }).click();
  await expect(
    row.getByRole("button", { name: "Save", exact: true }),
  ).toBeVisible();
});

test("bulk selection spans loaded pages and retries the same submission", async ({
  page,
}) => {
  const { lifecycle } = await mockPullRequest(page);
  const rows = [
    {
      id: "first-thread",
      subject: { kind: "pull", number: 1 },
      repository: {
        id: lifecycle.repositoryId,
        owner: "team",
        name: "project",
      },
      title: "Review the first change",
      href: "/team/project/pull/1",
      updated: { id: "first-activity", created_at_ms: 1_700_001_000_000 },
      sender: "Alice",
      reason: "review_requested",
      read: false,
      saved: false,
      done: false,
    },
    {
      id: "second-thread",
      subject: { kind: "pull", number: 1 },
      repository: { id: "another-repository", owner: "team", name: "library" },
      title: "Review the second change",
      href: "/team/library/pull/1",
      updated: { id: "second-activity", created_at_ms: 1_700_000_000_000 },
      sender: "Alice",
      reason: "review_requested",
      read: false,
      saved: false,
      done: false,
    },
  ];
  await page.route("**/api/session", (route) =>
    route.fulfill({
      json: {
        authenticated: true,
        user: { name: "Bob", subject: "bob" },
        csrf: "bulk-csrf",
      },
    }),
  );
  await page.route("**/api/notifications/unread", (route) =>
    route.fulfill({
      json: { unread: rows.some((row) => !row.read && !row.done) },
    }),
  );
  await page.route(
    (url) => url.pathname === "/api/notifications",
    (route) => {
      const query = new URL(route.request().url()).searchParams;
      const done = query.get("view") === "done";
      const items = rows.filter(
        (row) => row.done === done && (done || !row.read),
      );
      const before = query.has("before");
      return route.fulfill({
        json: {
          items: before ? items.slice(1) : items.slice(0, 1),
          next: !before && items.length > 1 ? "older" : null,
        },
      });
    },
  );
  let rejectStale = true;
  let loseSaveReply = true;
  let savedIntent: unknown;
  await page.route("**/api/notifications/bulk", (route) => {
    const request = route.request();
    const body = request.postDataJSON();
    expect(request.method()).toBe("POST");
    expect(request.headers()["x-csrf-token"]).toBe("bulk-csrf");
    expect(body.request_id).toEqual(expect.any(String));
    expect(body.issued_at_ms).toEqual(expect.any(Number));
    expect(body.items).toEqual(
      rows.map((row) => ({
        id: row.id,
        repository_id: row.repository.id,
        expected: row.updated,
      })),
    );
    if (rejectStale) {
      rejectStale = false;
      rows[1].updated = {
        id: "newer-activity",
        created_at_ms: 1_700_000_500_000,
      };
      return route.fulfill({
        status: 409,
        json: { error: { code: "page_changed", message: "This list changed" } },
      });
    }
    for (const row of rows) {
      if (typeof body.action.save === "boolean") row.saved = body.action.save;
      if (typeof body.action.done === "boolean") {
        row.done = body.action.done;
        row.read ||= row.done;
      }
    }
    if (body.action.save && loseSaveReply) {
      loseSaveReply = false;
      savedIntent = body;
      return route.abort("failed");
    }
    if (body.action.save) expect(body).toEqual(savedIntent);
    return route.fulfill({
      json: {
        items: rows.map(({ id, read, saved, done }) => ({
          id,
          read,
          saved,
          done,
        })),
      },
    });
  });

  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto("/-/notifications");
  const inbox = page.getByRole("region", { name: "Notifications" });
  const selectAll = inbox.getByRole("checkbox", {
    name: "Select all loaded notifications",
  });
  const older = inbox.getByRole("button", { name: "Load older notifications" });
  const action = inbox.getByLabel("Bulk notification action");
  const apply = inbox.getByRole("button", { name: "Apply to selected" });
  await inbox
    .getByRole("checkbox", { name: /Select Review the first change/ })
    .click();
  await expect(inbox.getByText("1 selected", { exact: true })).toBeVisible();
  await older.click();
  await expect(selectAll).toHaveAttribute("aria-checked", "mixed");
  await inbox
    .getByRole("checkbox", { name: /Select Review the second change/ })
    .click();
  await expect(inbox.getByText("2 selected", { exact: true })).toBeVisible();
  expect(
    await page.evaluate(() => document.documentElement.scrollWidth),
  ).toBeLessThanOrEqual(390);
  await expectNoAccessibilityViolations(page);
  await chooseSelectOption(action, "done");
  await apply.click();
  await expect(inbox.getByRole("alert")).toContainText("This list changed");
  await expect(inbox.getByRole("link", { name: rows[0].title })).toBeVisible();
  await expect(inbox.getByRole("link", { name: rows[1].title })).toBeVisible();
  await inbox.getByRole("button", { name: "Refresh inbox" }).click();
  await older.click();
  await expect(inbox.getByRole("link", { name: rows[1].title })).toBeVisible();
  await selectAll.click();
  await chooseSelectOption(action, "save");
  await apply.click();
  await expect(inbox.getByRole("alert")).toContainText("The response was lost");
  await expect(selectAll).toBeDisabled();
  await inbox.getByRole("button", { name: "Retry bulk action" }).click();
  await expect(inbox.getByRole("button", { name: "Unsave" })).toHaveCount(2);
  await expect(selectAll).not.toBeChecked();
  await expect(inbox.getByLabel("Repository", { exact: true })).toBeFocused();

  await selectAll.click();
  await chooseSelectOption(action, "done");
  await apply.click();
  await expect(inbox.getByText("No unread notifications.")).toBeVisible();
  await inbox.getByRole("link", { name: "Done", exact: true }).click();
  await older.click();
  await expect(inbox.getByRole("link", { name: rows[1].title })).toBeVisible();
  await selectAll.click();
  await chooseSelectOption(action, "restore");
  await apply.click();
  await expect(inbox.getByText("No completed notifications.")).toBeVisible();
});

test("unsubscribe recovers a lost reply and reports a stale row in a batch", async ({
  page,
}) => {
  const { lifecycle } = await mockPullRequest(page);
  const repository = {
    id: lifecycle.repositoryId,
    owner: "team",
    name: "project",
  };
  const rows = [
    {
      id: "saved-thread",
      repository,
      subject: { kind: "pull", number: 1 },
      updated: { id: "first-activity", created_at_ms: 1_700_001_000_000 },
      title: "Saved discussion",
      href: "/team/project/pull/1",
      sender: "Alice",
      reason: "mention",
      read: false,
      saved: true,
      done: false,
      dismissed: false,
    },
    {
      id: "other-thread",
      repository,
      subject: { kind: "pull", number: 2 },
      updated: { id: "second-activity", created_at_ms: 1_700_000_000_000 },
      title: "Other discussion",
      href: "/team/project/pull/2",
      sender: "Alice",
      reason: "mention",
      read: false,
      saved: false,
      done: false,
      dismissed: false,
    },
  ];
  await page.route("**/api/session", (route) =>
    route.fulfill({
      json: {
        authenticated: true,
        user: { name: "Bob", subject: "bob" },
        csrf: "unsubscribe-csrf",
      },
    }),
  );
  await page.route("**/api/notifications/unread", (route) =>
    route.fulfill({ json: { unread: rows.some((item) => !item.dismissed) } }),
  );
  await page.route(
    (url) => url.pathname === "/api/notifications",
    (route) => {
      const saved =
        new URL(route.request().url()).searchParams.get("view") === "saved";
      return route.fulfill({
        json: {
          items: rows.filter((row) => (saved ? row.saved : !row.dismissed)),
          next: null,
        },
      });
    },
  );
  let lost = true;
  let original: unknown;
  await page.route("**/api/notifications/unsubscribe", (route) => {
    const request = route.request();
    const body = request.postDataJSON();
    expect(request.method()).toBe("POST");
    expect(request.headers()["x-csrf-token"]).toBe("unsubscribe-csrf");
    expect(body.items).toEqual(
      rows.slice(0, body.items.length).map((row) => ({
        id: row.id,
        repository_id: repository.id,
        subject: row.subject,
        expected: row.updated,
      })),
    );
    rows[0].dismissed = true;
    if (lost) {
      lost = false;
      original = body;
      return route.abort("failed");
    }
    if (body.items.length === 1) {
      expect(body).toEqual(original);
      return route.fulfill({
        json: { items: [{ id: rows[0].id, status: "unsubscribed" }] },
      });
    }
    rows[1].updated = {
      id: "new-second-activity",
      created_at_ms: 1_700_002_000_000,
    };
    return route.fulfill({
      json: {
        items: [
          { id: rows[0].id, status: "unsubscribed" },
          { id: rows[1].id, status: "changed" },
        ],
      },
    });
  });

  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto("/-/notifications");
  const inbox = page.getByRole("region", { name: "Notifications" });
  const savedLink = inbox.getByRole("link", { name: rows[0].title });
  const savedRow = inbox.getByRole("listitem").filter({
    has: page.getByRole("link", { name: rows[0].title }),
  });
  await savedRow.getByRole("button", { name: "Unsubscribe" }).click();
  await expect(inbox.getByRole("alert")).toContainText("The response was lost");
  await inbox.getByRole("button", { name: "Retry unsubscribe" }).click();
  await expect(savedLink).toHaveCount(0);
  await inbox.getByRole("link", { name: "Saved", exact: true }).click();
  await expect(savedLink).toBeVisible();
  await expect(
    savedRow.getByRole("button", { name: "Unsubscribe" }),
  ).toHaveCount(0);

  rows[0].updated = {
    id: "new-first-activity",
    created_at_ms: 1_700_002_000_000,
  };
  rows[0].dismissed = false;
  await inbox.getByRole("link", { name: "All", exact: true }).click();
  await expect(savedLink).toBeVisible();
  await inbox
    .getByRole("checkbox", { name: "Select all loaded notifications" })
    .click();
  await chooseSelectOption(
    inbox.getByLabel("Bulk notification action"),
    "unsubscribe",
  );
  await inbox.getByRole("button", { name: "Apply to selected" }).click();
  await expect(inbox.getByRole("alert")).toContainText(
    "One conversation changed",
  );
  await expect(savedLink).toHaveCount(0);
  await expect(inbox.getByRole("link", { name: rows[1].title })).toBeVisible();
  await inbox.getByRole("button", { name: "Refresh inbox" }).click();
  await expect(inbox.getByRole("alert")).toHaveCount(0);
  await expectNoAccessibilityViolations(page);
});

test("release notifications filter and open the exact publication", async ({
  page,
}) => {
  const { lifecycle } = await mockPullRequest(page);
  const repository = {
    id: lifecycle.repositoryId,
    owner: "team",
    name: "project",
  };
  const tag = "release/train";
  const releasePath = "/team/project/releases/tag/release%2Ftrain";
  const href = `${releasePath}?repository_id=${repository.id}&release_number=12`;
  const notice = {
    id: "release-notice",
    repository,
    subject: { kind: "release", number: 12 },
    updated: { id: "publish-event", created_at_ms: 1_700_001_000_000 },
    title: "Release train",
    href,
    sender: "Alice",
    reason: "release",
    read: false,
    saved: false,
    done: false,
  };
  await page.route("**/api/session", (route) =>
    route.fulfill({
      json: {
        authenticated: true,
        user: { name: "Bob", subject: "bob" },
        csrf: "fixture-csrf",
      },
    }),
  );
  await page.route("**/api/notifications/unread", (route) =>
    route.fulfill({ json: { unread: true } }),
  );
  await page.route(
    (url) => url.pathname === "/api/notifications",
    (route) => {
      const reason = new URL(route.request().url()).searchParams.get("reason");
      return route.fulfill({
        json: {
          items: !reason || reason === "release" ? [notice] : [],
          next: null,
        },
      });
    },
  );
  await page.route(
    (url) => url.pathname === `/api/repos${releasePath}`,
    (route) => {
      expect(
        new URL(route.request().url()).searchParams.get("release_number"),
      ).toBe("12");
      expect(route.request().headers()["x-crab-repository-id"]).toBe(
        repository.id,
      );
      return route.fulfill({
        json: {
          number: 12,
          tag_name: tag,
          title: notice.title,
          body: "Published notes for this train.",
          tag_oid: "a".repeat(40),
          target_oid: "a".repeat(40),
          prerelease: false,
          draft: false,
          version: 1,
          author: "Alice",
          created_at: notice.updated.created_at_ms,
          published_at: notice.updated.created_at_ms,
          updated_at: notice.updated.created_at_ms,
          assets: [],
        },
      });
    },
  );

  await page.goto("/-/notifications");
  const inbox = page.getByRole("region", { name: "Notifications" });
  await chooseSelectOption(
    inbox.getByLabel("Reason", { exact: true }),
    "release",
  );
  const link = inbox.getByRole("link", { name: notice.title });
  await expect(link).toHaveAttribute("href", href);
  await expect(
    inbox.getByText(/Release published · Updated by Alice/),
  ).toBeVisible();
  await inbox
    .getByRole("checkbox", { name: "Select all loaded notifications" })
    .click();
  await expect(inbox.getByRole("option", { name: "Unsubscribe" })).toHaveCount(
    0,
  );
  await expect(inbox.getByRole("button", { name: "Unsubscribe" })).toHaveCount(
    0,
  );
  await link.click();
  await expect(page).toHaveURL(new RegExp("release_number=12$"));
  await expect(page.getByText("Published notes for this train.")).toBeVisible();
});
