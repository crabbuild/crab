import { expect, test } from "@playwright/test";
import {
  expectNoAccessibilityViolations,
  selectDarkTheme,
} from "./accessibility";
import { mockPullRequest } from "./pulls/fixture";

test("a repository administrator can configure and clear a team alias", async ({
  page,
}) => {
  const { lifecycle } = await mockPullRequest(page);
  const organization = "b4f2beee-435c-49cc-87ee-7dcfc4a096ea";
  const team = "276ba65a-db59-4efc-b969-99a3318785cc";
  let settings: {
    organization: string | null;
    namespace: string;
    teams: { id: string; slug: string }[];
    version: number;
  } = { organization: null, namespace: "", teams: [], version: 0 };
  await page.route(
    (url) => url.pathname === "/api/repos",
    (route) =>
      route.fulfill({
        json: {
          repositories: [
            {
              id: lifecycle.repositoryId,
              owner: "team",
              name: "project",
              access: "write",
              can_admin: true,
              archived: false,
            },
          ],
        },
      }),
  );
  await page.route(`**/api/organizations/${organization}/teams?**`, (route) =>
    route.fulfill({
      json: {
        can_create: true,
        items: [
          { team: { id: team, name: "Platform", visibility: "closed" } },
          { team: { id: "hidden", name: "Invisible", visibility: "secret" } },
        ],
        next: null,
        revision: 1,
      },
    }),
  );
  await page.route(
    (url) => url.pathname === "/api/repos/team/project/mention-settings",
    (route) => {
      if (route.request().method() === "PUT") {
        const input = route.request().postDataJSON();
        expect(input.version).toBe(settings.version);
        settings = { ...input, version: settings.version + 1 };
      }
      return route.fulfill({ json: settings });
    },
  );
  await page.goto("/team/project/settings");
  await page.getByRole("link", { name: "Team mentions" }).click();
  await expect(page).toHaveURL(/\/settings\/mentions$/);
  await page.getByLabel("Organization ID").fill(organization);
  await page.getByLabel("Mention namespace").fill("engineering");
  const picker = page.getByLabel("Closed team");
  await expect(picker.locator("option")).toHaveText([
    "Choose a team",
    "Platform",
  ]);
  await picker.selectOption(team);
  await page.getByRole("button", { name: "Add team" }).click();
  const alias = page.getByRole("textbox", { name: "Alias for Platform" });
  await alias.fill("platform");
  await page.getByRole("button", { name: "Save settings" }).click();
  const feedback = page
    .getByRole("status")
    .filter({ hasText: "Team mention settings saved." });
  await expect(feedback).toBeVisible();
  expect(settings).toEqual({
    organization,
    namespace: "engineering",
    teams: [{ id: team, slug: "platform" }],
    version: 1,
  });
  await page.reload();
  await expect(alias).toHaveValue("platform");
  await page.getByRole("button", { name: "Clear binding" }).click();
  await page.getByRole("button", { name: "Save settings" }).click();
  await expect(feedback).toBeVisible();
  expect(settings).toEqual({
    organization: null,
    namespace: "",
    teams: [],
    version: 2,
  });
});

test("a team suggestion inserts a stable mention that survives submission", async ({
  page,
}) => {
  const fixture = await mockPullRequest(page);
  const organization = "b4f2beee-435c-49cc-87ee-7dcfc4a096ea";
  const team = "276ba65a-db59-4efc-b969-99a3318785cc";
  const href = `/-/organizations/${organization}?tab=teams&team=${team}`;
  let body = "";
  await page.route("**/api/repos/team/project/**", async (route) => {
    const request = route.request();
    const url = new URL(request.url());
    if (url.pathname.endsWith("/team-mentions")) {
      expect(url.searchParams.get("repository_id")).toBeTruthy();
      expect(url.searchParams.get("search")).toBe("engineering/pla");
      return route.fulfill({
        json: {
          items: [
            {
              id: team,
              organization,
              handle: "engineering/platform",
              name: "Platform",
            },
          ],
        },
      });
    }
    if (url.pathname.endsWith("/issues") && request.method() === "POST") {
      const input = request.postDataJSON();
      body = input.body;
      return route.fulfill({
        status: 201,
        json: { ...fixture.pull(), ...input, number: 2 },
      });
    }
    if (url.pathname.endsWith("/issues/2"))
      return route.fulfill({
        json: { ...fixture.pull(), body, number: 2 },
      });
    return route.fallback();
  });
  await page.goto("/team/project/issues/new");
  await page
    .getByRole("textbox", { name: "Title", exact: true })
    .fill("Plan rollout");
  const editor = page.getByRole("textbox", {
    name: "Description",
    exact: true,
  });
  await editor.fill("Ask @engineering/pla");
  const menu = page.getByRole("listbox", { name: "Mention a team" });
  await expect(
    menu.getByRole("option", { name: /@engineering\/platform/ }),
  ).toBeVisible();
  await editor.press("Tab");
  await expect(editor).toHaveValue(`Ask [@engineering/platform](${href}) `);
  await page.getByRole("button", { name: "Create issue" }).click();
  await expect(page).toHaveURL(/\/team\/project\/issues\/2$/);
  await expect(
    page.getByRole("link", { name: "@engineering/platform" }),
  ).toHaveAttribute("href", href);
});

test("the private inbox links to the source and persists read state", async ({
  page,
}) => {
  const { lifecycle } = await mockPullRequest(page);
  const notice = {
    id: "ba3633c5-7c00-4542-bf14-282eb2c91b0f",
    repository: { owner: "team", name: "project", id: lifecycle.repositoryId },
    title: "Plan rollout",
    href: `/team/project/pull/2/files?repository_id=${lifecycle.repositoryId}#review-thread-1`,
    sender: "Alice",
    reason: "team_mention",
    updated: {
      id: "team-mention-activity",
      created_at_ms: Date.now() - 60_000,
    },
    read: false,
    saved: false,
    done: false,
  };
  await page.route("**/api/session", (route) =>
    route.fulfill({
      json: {
        authenticated: true,
        mode: "oidc",
        user: { issuer: "fixture", subject: "bob", name: "Bob" },
        csrf: "csrf",
      },
    }),
  );
  await page.route(
    (url) => url.pathname === "/api/notifications",
    (route) => {
      const unread = new URL(route.request().url()).searchParams.get("unread");
      return route.fulfill({
        json: {
          items: unread === "true" && notice.read ? [] : [notice],
          next: null,
        },
      });
    },
  );
  await page.route("**/api/notifications/unread", (route) =>
    route.fulfill({ json: { unread: !notice.read } }),
  );
  await page.route(
    (url) => url.pathname === "/api/repos/team/project/pulls/2/threads",
    (route) => route.fulfill({ json: { items: [], next: null } }),
  );
  await page.route(`**/api/notifications/${notice.id}/state`, (route) => {
    expect(route.request().method()).toBe("PUT");
    expect(route.request().headers()["x-csrf-token"]).toBe("csrf");
    expect(route.request().postDataJSON()).toEqual({
      repository_id: lifecycle.repositoryId,
      expected: notice.updated,
      action: { read: true },
    });
    notice.read = true;
    return route.fulfill({ json: { read: true, saved: false, done: false } });
  });
  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto("/");
  await page.getByRole("link", { name: "Notifications, unread" }).click();
  await expect(
    page.getByRole("heading", { name: "Notifications" }),
  ).toBeVisible();
  await selectDarkTheme(page);
  const row = page
    .getByRole("list", { name: "Notifications" })
    .getByRole("listitem");
  await expect(row.getByText(/Team mention · Updated by Alice/)).toBeVisible();
  await expect(row.getByRole("link", { name: "Plan rollout" })).toHaveAttribute(
    "href",
    notice.href,
  );
  await expectNoAccessibilityViolations(page);
  await row.getByRole("button", { name: "Mark as read" }).click();
  await expect(page.getByText("No unread notifications.")).toBeVisible();
  await expect(
    page.getByRole("link", { name: "Notifications", exact: true }),
  ).toBeVisible();
  await page.getByRole("link", { name: "All", exact: true }).click();
  await page.reload();
  await expect(
    row.getByRole("button", { name: "Mark as unread" }),
  ).toBeVisible();
  await row.getByRole("link", { name: "Plan rollout" }).click();
  await expect(page).toHaveURL(new RegExp("#review-thread-1$"));
  const contribution = page.locator("#review-thread-1");
  await expect(contribution).toContainText("Consider a clearer opening.");
  await expect(contribution).toBeFocused();
});
