import type { ActivityEvent } from "@/components/discussion/conversation/model";
import { expect, test, type Page } from "@playwright/test";
import { conversationPage } from "./conversation";
import { emptyReactions } from "./fixtures";
import {
  expectNoAccessibilityViolations,
  selectDarkTheme,
} from "./accessibility";

async function repository(page: Page) {
  const unexpected: string[] = [];
  await page.route("**/api/**", async (route) => {
    const path = new URL(route.request().url()).pathname;
    if (path === "/api/session")
      return route.fulfill({
        json: { authenticated: true, mode: "local", user: null, csrf: null },
      });
    if (path === "/api/repos")
      return route.fulfill({
        json: {
          repositories: [
            {
              owner: "team",
              name: "project",
              description: "",
              access: "write",
              can_admin: false,
              archived: false,
              archive_version: 0,
              protection_version: 0,
              protected_branches: [],
            },
          ],
        },
      });
    unexpected.push(path);
    return route.fulfill({
      status: 404,
      json: { error: { message: `Unexpected ${path}` } },
    });
  });
  return unexpected;
}

function event(number: number, kind: "created" | "closed"): ActivityEvent {
  return {
    number,
    actor: { name: "Alice", profile: null },
    target: { kind: "issue", number: 1 },
    title: "Timeline issue",
    action: { kind },
    created_at: 1700000000000 + number,
  };
}

test("repository activity retains pages through errors and supports refresh and milestone links", async ({
  page,
}) => {
  const unexpected = await repository(page);
  const events: ActivityEvent[] = [event(2, "closed"), event(1, "created")];
  let failOlder = true;
  await page.route("**/api/repos/team/project/activity?**", async (route) => {
    const before = new URL(route.request().url()).searchParams.get("before");
    if (before && failOlder) {
      failOlder = false;
      return route.fulfill({
        status: 503,
        json: { error: { message: "Activity storage unavailable" } },
      });
    }
    const remaining = events.filter(
      (event) => !before || Number(event.number) < Number(before),
    );
    return route.fulfill({
      headers: { "Server-Timing": "cell;dur=2.5" },
      json: {
        items: remaining.slice(0, 1),
        next: remaining.length > 1 ? remaining[0].number : null,
      },
    });
  });
  await page.goto("/team/project/activity?powerbar=1");
  const panel = page.getByRole("region", { name: "Activity", exact: true });
  await expect(panel.locator("li")).toHaveCount(1);
  await expect(panel).toContainText("Alice closed");
  await panel.getByRole("button", { name: "Load older activity" }).click();
  await expect(panel.getByRole("alert")).toContainText(
    "Activity storage unavailable",
  );
  await expect(panel.locator("li")).toHaveCount(1);
  await panel.getByRole("button", { name: "Retry activity" }).click();
  await expect(panel.locator("li")).toHaveCount(2);
  await expect(
    panel.getByRole("heading", { name: "Recent activity" }),
  ).toBeFocused();
  await expect(
    panel.getByRole("button", { name: "Load older activity" }),
  ).toHaveCount(0);
  events.unshift({
    ...event(3, "created"),
    target: { kind: "pull", number: 4 },
    title: "Related pull",
    action: {
      kind: "milestone_assigned",
      number: 7,
      title: "Release <script>untrusted</script>",
    },
  });
  await panel.getByRole("button", { name: "Refresh activity" }).click();
  await expect(panel.locator("li")).toHaveCount(1);
  await expect(panel).toContainText("added milestone");
  await expect(panel.locator("script")).toHaveCount(0);
  await expect(
    panel.getByRole("link", { name: "Release <script>untrusted</script>" }),
  ).toHaveAttribute("href", "/team/project/milestones/7");
  await expect(
    panel.getByRole("link", { name: "#4 Related pull" }),
  ).toHaveAttribute("href", "/team/project/pull/4");
  const widget = page.getByRole("group", {
    name: "Activity request widget",
    exact: true,
  });
  await expect(widget).toContainText("Recent activity");
  await widget.locator("summary").click();
  await expect(widget).toContainText("cell;dur=2.5");
  await expectNoAccessibilityViolations(page);
  await selectDarkTheme(page);
  await page.setViewportSize({ width: 390, height: 844 });
  await expectNoAccessibilityViolations(page);
  expect(
    await page.evaluate(() => document.documentElement.scrollWidth),
  ).toBeLessThanOrEqual(390);
  expect(unexpected).toEqual([]);
});

test("issue changes and comments share one durable conversation without duplicate snapshots", async ({
  page,
}) => {
  const unexpected = await repository(page);
  let state = "open";
  let version = 1;
  const events: ActivityEvent[] = [event(1, "created")];
  const comments: Array<Record<string, unknown>> = [];
  const issue = () => ({
    number: 1,
    title: "Timeline issue",
    body: "Discussion",
    author: { name: "Alice", profile: null },
    state,
    version,
    created_at: 1700000000000,
    updated_at: 1700000000000,
    labels: [],
    assignees: [],
    can_edit: true,
    can_close: true,
    can_label: false,
    can_assign: false,
    reactions: emptyReactions(),
  });
  await page.route("**/api/repos/team/project/**", async (route) => {
    const request = route.request();
    const path = new URL(request.url()).pathname;
    if (path.endsWith("/conversation"))
      return conversationPage(route, { comments, activity: events });
    if (path.endsWith("/links"))
      return route.fulfill({
        json: { items: [], next: null, can_manage: false },
      });
    if (path.endsWith("/milestone"))
      return route.fulfill({
        json: { milestone: null, version: 0, can_manage: false },
      });
    if (path.endsWith("/comments")) {
      if (request.method() === "POST") {
        const input = request.postDataJSON();
        const comment = {
          number: 1,
          author: { name: "Alice", profile: null },
          body: input.body,
          version: 1,
          created_at: 1700000000003,
          updated_at: 1700000000003,
          can_edit: true,
          reactions: emptyReactions(),
        };
        comments.push(comment);
        events.unshift({
          ...event(3, "created"),
          target: { kind: "issue_comment", issue: 1, number: 1 },
        });
        return route.fulfill({ status: 201, json: comment });
      }
      return route.fulfill({ json: { items: comments, next: null } });
    }
    if (path.endsWith("/issues/1")) {
      if (request.method() === "PATCH") {
        const input = request.postDataJSON();
        expect(input.version).toBe(version);
        state = input.state;
        version++;
        events.unshift(event(2, "closed"));
      }
      return route.fulfill({ json: issue() });
    }
    return route.fallback();
  });
  await page.goto("/team/project/issues/1");
  const panel = page.getByRole("region", {
    name: "Conversation history",
    exact: true,
  });
  await expect(panel.locator(".conversation-events > li:visible")).toHaveCount(
    0,
  );
  await page.getByRole("button", { name: "Close issue", exact: true }).click();
  await expect(panel).toContainText("Alice closed");
  await page
    .getByRole("textbox", { name: "Comment", exact: true })
    .fill("A new conversation");
  await page.getByRole("button", { name: "Comment", exact: true }).click();
  await expect(panel).toContainText("A new conversation");
  await expect(panel.locator(".conversation-events > li:visible")).toHaveCount(
    2,
  );
  await page.reload();
  await expect(panel.locator(".conversation-events > li:visible")).toHaveCount(
    2,
  );
  await expect(panel).toContainText("Alice closed");
  await expectNoAccessibilityViolations(page);
  expect(unexpected).toEqual([]);
});

test("inline review activity links remain distinguishable in dark mode", async ({
  page,
}) => {
  const unexpected = await repository(page);
  const events: ActivityEvent[] = ["resolved", "unresolved", "commented"].map(
    (kind, index) => ({
      ...event(index + 1, "created"),
      actor: { name: "Local operator", profile: null },
      target: {
        kind: kind === "commented" ? "pull_review" : "pull_thread",
        pull: 1,
        number: 1,
      },
      title: "Verify PR reactions",
      action:
        kind === "commented"
          ? { kind: "commented" }
          : kind === "resolved"
            ? { kind: "resolved" }
            : { kind: "unresolved" },
    }),
  );
  await page.route("**/api/repos/team/project/activity?**", (route) =>
    route.fulfill({ json: { items: events, next: null } }),
  );
  await page.goto("/team/project/activity");
  await expect(page.locator("[data-activity-number]")).toHaveCount(3);
  await selectDarkTheme(page);
  await page.setViewportSize({ width: 390, height: 844 });
  await expectNoAccessibilityViolations(page);
  expect(unexpected).toEqual([]);
});
