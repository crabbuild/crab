import { expect, test, type Page } from "@playwright/test";
import { emptyReactions } from "./fixtures";
import {
  expectNoAccessibilityViolations,
  selectDarkTheme,
} from "./accessibility";

async function fixture(page: Page) {
  let version = 1;
  let state: "requested" | "cancelled" | "fulfilled" | null = null;
  let fail = false,
    canManage = true,
    archived = false,
    paginated = false;
  const id = "b".repeat(64);
  const unexpected: string[] = [];
  const writes: unknown[] = [];
  await page.route("**/api/**", async (route) => {
    const request = route.request();
    const url = new URL(request.url()),
      path = url.pathname;
    if (path === "/api/session")
      return route.fulfill({
        json: {
          authenticated: true,
          mode: "oidc",
          user: { issuer: "test", subject: "alice", name: "Alice" },
          csrf: "csrf",
        },
      });
    if (path === "/api/repos")
      return route.fulfill({
        json: {
          repositories: [
            {
              owner: "team",
              name: "project",
              description: "",
              access: canManage ? "write" : "read",
              archived,
              archive_version: 0,
              can_admin: false,
              protection_version: 0,
              protected_branches: [],
            },
          ],
        },
      });
    if (path.endsWith("/refs"))
      return route.fulfill({
        json: {
          head: { name: "refs/heads/main", oid: "a".repeat(40) },
          refs: [
            { name: "refs/heads/main", oid: "a".repeat(40) },
            { name: "refs/heads/feature", oid: "b".repeat(40) },
          ],
          generation: 1,
        },
      });
    if (path.endsWith("/pulls/1"))
      return route.fulfill({
        json: {
          number: 1,
          title: "Reviewable change",
          body: "A proposed change",
          draft: false,
          version,
          state: "open",
          author: "Alice",
          created_at: 1700000000000,
          updated_at: 1700000000000,
          base_ref: "refs/heads/main",
          head_ref: "refs/heads/feature",
          base_oid: "a".repeat(40),
          head_oid: "b".repeat(40),
          original_base_oid: "a".repeat(40),
          original_head_oid: "b".repeat(40),
          labels: [],
          assignees: [],
          can_edit: false,
          can_label: false,
          can_assign: false,
          can_manage: false,
          can_decide: false,
          can_merge: false,
          branches_available: true,
          merge: null,
          merge_pending: null,
          reactions: emptyReactions(),
          merge_requirements: {
            protected: true,
            required_approvals: 1,
            approvals: 0,
            changes_requested: 0,
            checks_satisfied: true,
            checks: [],
            satisfied: false,
          },
        },
      });
    if (path.endsWith("/review-requests")) {
      if (request.method() === "PUT") {
        expect(request.headers()["x-csrf-token"]).toBe("csrf");
        const input = request.postDataJSON() as {
          version: number;
          change:
            | { kind: "request"; subject: string }
            | { kind: "cancel"; id: string };
        };
        writes.push(input);
        if (fail) {
          fail = false;
          return route.fulfill({
            status: 503,
            json: { error: { message: "Review request storage unavailable" } },
          });
        }
        if (input.version !== version)
          return route.fulfill({
            status: 409,
            json: {
              error: { code: "conflict", message: "Review requests changed" },
            },
          });
        state = input.change.kind === "request" ? "requested" : "cancelled";
        version++;
        return route.fulfill({ json: { id, state: { kind: state } } });
      }
      const bob = {
        id,
        name: "Bob",
        available: true,
        requested_by: "Alice",
        state:
          state === "fulfilled"
            ? { kind: state, review: 1 }
            : { kind: state ?? "requested" },
      };
      const carol = {
        id: "a".repeat(64),
        name: "Carol",
        available: false,
        requested_by: "Alice",
        state: { kind: "requested" },
      };
      return route.fulfill({
        json: {
          items: paginated
            ? url.searchParams.has("before")
              ? [carol]
              : [bob]
            : state
              ? [bob]
              : [],
          next: paginated && !url.searchParams.has("before") ? id : null,
          version,
          can_manage: canManage,
          eligible: [{ subject: "bob-id", name: "Bob" }],
        },
      });
    }
    if (
      path.endsWith("/comments") ||
      path.endsWith("/reviews") ||
      path.endsWith("/activity")
    )
      return route.fulfill({ json: { items: [], next: null } });
    if (path.endsWith("/links"))
      return route.fulfill({
        json: { items: [], next: null, can_manage: false },
      });
    if (path.endsWith("/milestone"))
      return route.fulfill({
        json: { milestone: null, version: 0, can_manage: false },
      });
    unexpected.push(`${request.method()} ${path}`);
    return route.fulfill({
      status: 404,
      json: { error: { message: `Unexpected ${path}` } },
    });
  });
  return {
    fail: () => {
      fail = true;
    },
    race: () => {
      version++;
    },
    fulfill: () => {
      state = "fulfilled";
      version++;
    },
    readOnly: () => {
      canManage = false;
    },
    archive: () => {
      archived = true;
    },
    pages: () => {
      paginated = true;
    },
    writes,
    unexpected,
  };
}

test("review requests retain failed choices, persist on reload, cancel and show fulfilled reviews", async ({
  page,
}) => {
  const model = await fixture(page);
  await page.goto("/team/project/pull/1");
  const widget = page.getByRole("region", {
    name: "Review requests",
    exact: true,
  });
  const choice = widget.getByLabel("Request a reviewer");
  await choice.selectOption("bob-id");
  model.fail();
  await widget
    .getByRole("button", { name: "Request review", exact: true })
    .click();
  await expect(widget.getByRole("alert")).toHaveText(
    "Review request storage unavailable",
  );
  await expect(choice).toHaveValue("bob-id");
  await widget
    .getByRole("button", { name: "Request review", exact: true })
    .click();
  await expect(
    widget.getByRole("button", { name: "Cancel review request for Bob" }),
  ).toBeVisible();
  await page.reload();
  await expect(widget.getByText("Requested by Alice")).toBeVisible();
  await widget
    .getByRole("button", { name: "Cancel review request for Bob" })
    .click();
  await expect(widget.getByText("No reviews requested.")).toBeVisible();
  await choice.selectOption("bob-id");
  await widget
    .getByRole("button", { name: "Request review", exact: true })
    .click();
  await expect(widget.getByText("Requested by Alice")).toBeVisible();
  model.fulfill();
  await page.reload();
  await expect(
    widget.getByText("Review submitted", { exact: true }),
  ).toBeVisible();
  await expect(
    widget.getByRole("button", { name: "Cancel review request for Bob" }),
  ).toHaveCount(0);
  expect(model.writes).toHaveLength(4);
  expect(model.unexpected).toEqual([]);
});

test("stale requests require explicit reload and permissions preserve read-only status", async ({
  page,
}) => {
  const model = await fixture(page);
  await page.goto("/team/project/pull/1");
  const widget = page.getByRole("region", {
    name: "Review requests",
    exact: true,
  });
  const choice = widget.getByLabel("Request a reviewer");
  await choice.selectOption("bob-id");
  model.race();
  await widget
    .getByRole("button", { name: "Request review", exact: true })
    .click();
  await expect(widget.getByRole("alert")).toHaveText("Review requests changed");
  await expect(
    widget.getByRole("button", { name: "Request review", exact: true }),
  ).toBeDisabled();
  await widget.getByRole("button", { name: "Reload review requests" }).click();
  await expect(choice).toHaveValue("bob-id");
  await widget
    .getByRole("button", { name: "Request review", exact: true })
    .click();
  await expect(widget.getByText("Requested by Alice")).toBeVisible();
  model.readOnly();
  await page.reload();
  await expect(widget.getByText("Requested by Alice")).toBeVisible();
  await expect(widget.getByRole("combobox")).toHaveCount(0);
  await expect(
    widget.getByRole("button", { name: "Cancel review request for Bob" }),
  ).toHaveCount(0);
  model.archive();
  await page.reload();
  await expect(widget.getByText("Requested by Alice")).toBeVisible();
  await expect(widget.getByRole("combobox")).toHaveCount(0);
  expect(model.unexpected).toEqual([]);
});

test("reviewer pagination, whole-widget timing and mobile themes remain accessible", async ({
  page,
}) => {
  const model = await fixture(page);
  model.pages();
  await page.goto("/team/project/pull/1?powerbar=1");
  const widget = page.getByRole("region", {
    name: "Review requests",
    exact: true,
  });
  const timing = page.getByRole("group", {
    name: "Review requests request widget",
    exact: true,
  });
  await expect(
    timing.getByRole("button", { name: "Request review", exact: true }),
  ).toBeVisible();
  await widget.getByRole("button", { name: "More reviewers" }).click();
  await expect(widget.getByText("Carol", { exact: true })).toBeVisible();
  await expect(widget.getByText("No longer a repository member")).toBeVisible();
  await widget.getByRole("button", { name: "First reviewer page" }).click();
  await expect(
    widget.getByRole("listitem").getByText("Bob", { exact: true }),
  ).toBeVisible();
  await page.setViewportSize({ width: 390, height: 844 });
  await expectNoAccessibilityViolations(page);
  await selectDarkTheme(page);
  await expectNoAccessibilityViolations(page);
  expect(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= innerWidth,
    ),
  ).toBe(true);
  expect(model.unexpected).toEqual([]);
});
