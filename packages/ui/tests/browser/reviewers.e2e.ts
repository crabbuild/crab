import { conversationPage } from "./conversation";
import { expect, test, type Page } from "@playwright/test";
import type { ReviewState } from "../../src/components/discussion/conversation/model";
import { emptyReactions } from "./fixtures";
import {
  expectNoAccessibilityViolations,
  selectDarkTheme,
} from "./accessibility";

const actorId = "01900000-0000-7000-8000-000000000002";
async function fixture(page: Page) {
  let enrolled = false,
    removed = false,
    handle = "public-bob";
  let version = 1;
  let submitted: ReviewState | null = null;
  let decision: ReviewState | null = null;
  let current: boolean | null = true;
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
    if (path.endsWith("/conversation")) return conversationPage(route);
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
          author: { name: "Alice", profile: null },
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
    if (
      (path.endsWith("/reviewers") && request.method() === "GET") ||
      (path.endsWith("/review-requests") && request.method() === "PUT")
    ) {
      if (request.method() === "PUT") {
        expect(request.headers()["x-csrf-token"]).toBe("csrf");
        const input = request.postDataJSON() as {
          version: number;
          change:
            | { kind: "request"; actor_id: string }
            | { kind: "cancel"; id: string };
        };
        if (input.change.kind === "request")
          expect(input.change.actor_id).toBe(actorId);
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
        reviewer: {
          name: "Bob",
          profile: enrolled
            ? { id: actorId, handle, name: "Public Bob" }
            : null,
        },
        candidate_id: removed ? null : actorId,
        request:
          state || paginated
            ? {
                id,
                requested_by: { name: "Alice", profile: null },
                state:
                  state === "fulfilled"
                    ? { kind: state, review: 1 }
                    : { kind: state ?? "requested" },
              }
            : null,
        review: submitted
          ? {
              number: submitted === "commented" && decision ? 2 : 1,
              state: submitted,
              commit_oid: "b".repeat(40),
              current,
            }
          : null,
        decision: decision
          ? { number: 1, state: decision, commit_oid: "b".repeat(40), current }
          : null,
      };
      const carol = {
        id: "a".repeat(64),
        reviewer: { name: "Carol", profile: null },
        candidate_id: null,
        request: {
          id: "a".repeat(64),
          requested_by: { name: "Alice", profile: null },
          state: { kind: "requested" },
        },
        review: null,
        decision: null,
      };
      return route.fulfill({
        json: {
          items: paginated
            ? url.searchParams.has("after")
              ? [carol]
              : [bob]
            : submitted || (state && state !== "cancelled")
              ? [bob]
              : [],
          next: paginated && !url.searchParams.has("after") ? id : null,
          version,
          can_manage: canManage,
          candidates: removed
            ? []
            : [
                {
                  id: actorId,
                  name: "Bob",
                  profile: enrolled
                    ? { id: actorId, handle, name: "Public Bob" }
                    : null,
                },
              ],
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
      submitted = "commented";
      version++;
    },
    review: (state: ReviewState, freshness: boolean | null = true) => {
      submitted = state;
      if (state !== "commented") decision = state;
      current = freshness;
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
    enroll: () => {
      enrolled = true;
    },
    rename: () => {
      handle = "renamed-bob";
    },
    remove: () => {
      removed = true;
    },
    writes,
    unexpected,
  };
}

async function openPicker(page: Page) {
  await page
    .getByRole("button", { name: "Request reviewers", exact: true })
    .click();
  return page.getByRole("dialog", { name: "Request reviewers", exact: true });
}

test("review requests retain failed choices, persist on reload, cancel and can be requested again", async ({
  page,
}) => {
  const model = await fixture(page);
  await page.goto("/team/project/pull/1");
  const widget = page.getByRole("region", {
    name: "Reviewers",
    exact: true,
  });
  model.fail();
  const picker = await openPicker(page);
  const search = picker.getByRole("textbox", { name: "Find a reviewer" });
  await search.fill("bo");
  const choose = picker.getByRole("button", {
    name: "Request review from Bob",
    exact: true,
  });
  await choose.click();
  await expect(picker.getByRole("alert")).toHaveText(
    "Review request storage unavailable",
  );
  await expect(search).toHaveValue("bo");
  await choose.click();
  await expect(
    widget.getByRole("button", { name: "Cancel review request for Bob" }),
  ).toBeVisible();
  await page.reload();
  await expect(widget.getByText("Review requested by Alice")).toBeVisible();
  await widget
    .getByRole("button", { name: "Cancel review request for Bob" })
    .click();
  await expect(widget.getByText("No reviews yet.")).toBeVisible();
  await openPicker(page);
  await choose.click();
  await expect(widget.getByText("Review requested by Alice")).toBeVisible();
  model.fulfill();
  await page.reload();
  await expect(widget.getByText("Commented", { exact: true })).toBeVisible();
  await expect(
    widget.getByRole("button", { name: "Cancel review request for Bob" }),
  ).toHaveCount(0);
  await widget
    .getByRole("button", { name: "Re-request review from Bob" })
    .click();
  await expect(widget.getByText("Review requested by Alice")).toBeVisible();
  expect(model.writes).toHaveLength(5);
  expect(model.unexpected).toEqual([]);
});

test("stale requests require explicit reload and permissions preserve read-only status", async ({
  page,
}) => {
  const model = await fixture(page);
  await page.goto("/team/project/pull/1");
  const widget = page.getByRole("region", {
    name: "Reviewers",
    exact: true,
  });
  const picker = await openPicker(page);
  const choice = picker.getByRole("button", {
    name: "Request review from Bob",
  });
  model.race();
  await choice.click();
  await expect(picker.getByRole("alert")).toHaveText("Review requests changed");
  await expect(choice).toBeDisabled();
  await picker.getByRole("button", { name: "Reload review requests" }).click();
  await expect(choice).toBeEnabled();
  await choice.click();
  await expect(widget.getByText("Review requested by Alice")).toBeVisible();
  model.readOnly();
  await page.reload();
  await expect(widget.getByText("Review requested by Alice")).toBeVisible();
  await expect(
    widget.getByRole("button", { name: "Request reviewers" }),
  ).toHaveCount(0);
  await expect(
    widget.getByRole("button", { name: "Cancel review request for Bob" }),
  ).toHaveCount(0);
  model.archive();
  await page.reload();
  await expect(widget.getByText("Review requested by Alice")).toBeVisible();
  await expect(
    widget.getByRole("button", { name: "Request reviewers" }),
  ).toHaveCount(0);
  expect(model.unexpected).toEqual([]);
});

test("reviewer pagination, whole-widget timing and mobile themes remain accessible", async ({
  page,
}) => {
  const model = await fixture(page);
  model.pages();
  await page.goto("/team/project/pull/1?powerbar=1");
  const widget = page.getByRole("region", {
    name: "Reviewers",
    exact: true,
  });
  const timing = page.getByRole("group", {
    name: "Reviewers request widget",
    exact: true,
  });
  await expect(
    timing.getByRole("button", { name: "Request reviewers", exact: true }),
  ).toBeVisible();
  await widget.getByRole("button", { name: "More reviewers" }).click();
  await expect(widget.getByText("Carol", { exact: true })).toBeVisible();
  await expect(
    widget.getByText("Unavailable for new review requests."),
  ).toBeVisible();
  await widget.getByRole("button", { name: "First reviewer page" }).click();
  await expect(
    widget.getByRole("listitem").getByText("Bob", { exact: true }),
  ).toBeVisible();
  await page.setViewportSize({ width: 390, height: 844 });
  await openPicker(page);
  await expectNoAccessibilityViolations(page);
  await page.keyboard.press("Escape");
  await selectDarkTheme(page);
  await openPicker(page);
  await expectNoAccessibilityViolations(page);
  expect(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= innerWidth,
    ),
  ).toBe(true);
  expect(model.unexpected).toEqual([]);
});

test("public reviewer search uses UUIDs and keeps profile links through rename and removal", async ({
  page,
}) => {
  const model = await fixture(page);
  model.enroll();
  await page.goto("/team/project/pull/1");
  const trigger = page.getByRole("button", {
    name: "Request reviewers",
    exact: true,
  });
  await trigger.focus();
  await page.keyboard.press("Enter");
  const picker = page.getByRole("dialog", {
    name: "Request reviewers",
    exact: true,
  });
  const search = picker.getByRole("textbox", { name: "Find a reviewer" });
  await expect(search).toBeFocused();
  await search.fill("no-match");
  await expect(picker.getByRole("status")).toHaveText(
    "No people match your search.",
  );
  await search.fill("public-bob");
  await page.keyboard.press("Tab");
  await page.keyboard.press("Enter");
  const widget = page.getByRole("region", {
    name: "Reviewers",
    exact: true,
  });
  await expect(
    widget.getByRole("link", { name: "public-bob", exact: true }),
  ).toHaveAttribute("href", `/-/users/${actorId}`);
  expect(model.writes[0]).toEqual({
    version: 1,
    change: { kind: "request", actor_id: actorId },
  });
  model.rename();
  await page.reload();
  await expect(
    widget.getByRole("link", { name: "renamed-bob", exact: true }),
  ).toHaveAttribute("href", `/-/users/${actorId}`);
  model.fulfill();
  await page.reload();
  await expect(
    widget.getByRole("button", { name: "Re-request review from renamed-bob" }),
  ).toBeVisible();
  model.remove();
  await page.reload();
  await expect(
    widget.getByRole("link", { name: "renamed-bob", exact: true }),
  ).toHaveAttribute("href", `/-/users/${actorId}`);
  await expect(
    widget.getByText("Unavailable for new review requests."),
  ).toBeVisible();
  await expect(widget.getByRole("button", { name: /Re-request/ })).toHaveCount(
    0,
  );
  expect(model.unexpected).toEqual([]);
});

test("uninvited reviewers show decisions independently of comments and freshness", async ({
  page,
}) => {
  const model = await fixture(page);
  model.review("approved");
  await page.goto("/team/project/pull/1?powerbar=1");
  const widget = page.getByRole("region", { name: "Reviewers", exact: true });
  await expect(widget.getByText("Approved", { exact: true })).toBeVisible();
  await expect(widget.getByText("Review requested by Alice")).toHaveCount(0);
  await expect(
    widget.getByRole("button", { name: "Re-request review from Bob" }),
  ).toBeVisible();
  model.review("commented");
  await page.reload();
  await expect(widget.getByText("Approved", { exact: true })).toBeVisible();
  await expect(
    widget.getByText("Commented after this decision."),
  ).toBeVisible();
  for (const [freshness, label] of [
    [false, "Outdated review"],
    [null, "Head unavailable"],
  ] as const) {
    model.review("changes_requested", freshness);
    await page.reload();
    await expect(
      widget.getByText("Changes requested", { exact: true }),
    ).toBeVisible();
    await expect(widget.getByText(label, { exact: true })).toBeVisible();
    await expect(widget.getByText("Approved", { exact: true })).toHaveCount(0);
  }
  await page.setViewportSize({ width: 390, height: 844 });
  await selectDarkTheme(page);
  await expectNoAccessibilityViolations(page);
  expect(model.writes).toHaveLength(0);
  expect(model.unexpected).toEqual([]);
});
