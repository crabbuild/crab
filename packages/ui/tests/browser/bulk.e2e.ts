import { emptyPullRollup } from "./fixtures";
import { expect, test, type Page } from "@playwright/test";
import { mockPullRequest } from "./pulls/fixture";
import {
  expectNoAccessibilityViolations,
  selectDarkTheme,
} from "./accessibility";

async function fixture(page: Page) {
  await mockPullRequest(page);
  const alice = {
    id: "00000000-0000-4000-8000-000000000001",
    name: "Alice",
    profile: null,
  };
  const labels = [
    { id: 1, name: "Bug", color: "abcdef" },
    { id: 2, name: "Help wanted", color: "fedcba" },
  ];
  const model = {
    items: [
      { kind: "issue", number: 1, title: "First issue", labels: [labels[0]] },
      { kind: "pull", number: 1, title: "First pull", labels: [] },
      { kind: "issue", number: 2, title: "Second issue", labels: [] },
    ].map((item) => ({
      ...item,
      rollup: emptyPullRollup(item.number),
      state: "open",
      version: 1,
      draft: false,
      state_reason: null,
      closed_at: null,
      author: { name: "Alice", profile: null },
      created_at: 1700000000000,
      updated_at: 1700000000000,
      assignees: [] as (typeof alice)[],
      comment_count: 0,
      reaction_count: 0,
      base_ref: "refs/heads/main",
      head_ref: "refs/heads/feature",
    })),
    repository_id: "00000000-0000-4000-8000-000000000099",
    writes: [] as {
      repository_id: string;
      request_id: string;
      items: {
        item: { subject: { kind: string; number: number }; version: number };
        milestone_version: number;
      }[];
      action: {
        kind: string;
        state?: string;
        reason?: string;
        add?: (string | number)[];
        remove?: (string | number)[];
        number?: number | null;
      };
    }[],
    lose: false,
    lostStatus: 503,
    conflict: false,
    preflightStatus: 0,
    canManage: true,
  };
  const saved = new Map<string, number>();
  const rejected = new Set<string>();
  for (const view of ["issues", "pulls"])
    await page.route(`**/api/repos/team/project/${view}?**`, (route) => {
      const items =
        view === "issues"
          ? model.items
          : model.items.filter((item) => item.kind === "pull");
      return route.fulfill({
        json: {
          repository_id: model.repository_id,
          can_bulk_edit: model.canManage,
          items,
          next: null,
          matching: items.length,
          counts: {
            open: items.filter((i) => i.state === "open").length,
            closed: items.filter((i) => i.state === "closed").length,
            merged: 0,
          },
          authored_by_me: false,
          assigned_to_me: false,
          review_requested_from_me: false,
          navigation: { kind: "results" },
        },
      });
    });
  await page.route("**/discussions/selection?**", (route) => {
    const items = new URL(route.request().url()).searchParams
      .get("items")!
      .split(",")
      .map((item) => {
        const [kind, number, version] = item.split(":");
        return {
          item: {
            subject: { kind, number: Number(number) },
            version: Number(version),
          },
          milestone_version: 0,
        };
      });
    return route.fulfill({
      json: { repository_id: model.repository_id, items },
    });
  });
  await page.route("**/discussions/bulk?**", async (route) => {
    const input = route
      .request()
      .postDataJSON() as (typeof model.writes)[number];
    model.writes.push(input);
    if (model.preflightStatus)
      return route.fulfill({
        status: model.preflightStatus,
        json: { error: { message: "Repository access unavailable" } },
      });
    if (input.repository_id !== model.repository_id)
      return route.fulfill({
        status: 400,
        json: { error: { message: "Repository identity changed" } },
      });
    if (
      (model.conflict && !saved.has(input.request_id)) ||
      rejected.has(input.request_id)
    ) {
      model.conflict = false;
      rejected.add(input.request_id);
      return route.fulfill({
        status: 409,
        headers: { "X-Crab-Write-Outcome": "rejected" },
        json: {
          error: { code: "conflict", message: "A selected item changed" },
        },
      });
    }
    if (!saved.has(input.request_id)) {
      for (const selected of input.items) {
        const item = model.items.find(
          (item) =>
            item.kind === selected.item.subject.kind &&
            item.number === selected.item.subject.number,
        )!;
        expect(item.version).toBe(selected.item.version);
        const action = input.action;
        if (action.kind === "state") item.state = action.state!;
        if (action.kind === "labels")
          item.labels = [
            ...item.labels.filter(
              (label) => !action.remove!.includes(label.id),
            ),
            ...labels.filter(
              (label) =>
                action.add!.includes(label.id) &&
                !item.labels.some((previous) => previous.id === label.id),
            ),
          ];
        if (action.kind === "assignees")
          item.assignees = action.add!.includes(alice.id) ? [alice] : [];
        if (action.kind !== "milestone") item.version++;
      }
      saved.set(input.request_id, input.items.length);
    }
    if (model.lose) {
      model.lose = false;
      return route.fulfill({
        status: model.lostStatus,
        json: { error: { message: "Response lost" } },
      });
    }
    await route.fulfill({ json: { count: saved.get(input.request_id) } });
  });
  await page.route("**/labels?**", (route) =>
    route.fulfill({ json: { items: labels, can_manage: true } }),
  );
  await page.route("**/assignees?**", (route) =>
    route.fulfill({ json: { items: [alice], can_manage: true } }),
  );
  await page.route("**/discussion-search/milestones?**", (route) => {
    // Native search distinguishes an absent query from an invalid empty one.
    expect(new URL(route.request().url()).searchParams.get("query")).not.toBe(
      "",
    );
    return route.fulfill({
      json: {
        items: [{ number: 42, title: "Next release", state: "open" }],
        truncated: false,
      },
    });
  });
  return model;
}
const issue = (page: Page, number = 1) =>
  page.getByRole("checkbox", { name: new RegExp(`^Select issue #${number}:`) });
const pull = (page: Page) =>
  page.getByRole("checkbox", { name: /^Select pull request #1:/ });

test("mixed selection supports individual, range, page and keyboard selection without writes", async ({
  page,
}) => {
  const model = await fixture(page);
  await page.goto("/team/project/issues");
  await issue(page).check();
  await expect(
    page.getByRole("checkbox", { name: "Select all items on this page" }),
  ).toHaveAttribute("aria-checked", "mixed");
  await issue(page, 2).click({ modifiers: ["Shift"] });
  await expect(pull(page)).toBeChecked();
  await expect(
    page.getByRole("checkbox", { name: "Select all items on this page" }),
  ).toBeChecked();
  await page.getByRole("button", { name: "Clear selection" }).click();
  await issue(page).focus();
  await page.keyboard.press("Space");
  await expect(issue(page)).toBeChecked();
  expect(model.writes).toHaveLength(0);
  await expectNoAccessibilityViolations(page);
});

test("bulk labels, assignees, milestones and state update the exact mixed selection", async ({
  page,
}) => {
  const model = await fixture(page);
  await page.goto("/team/project/issues");
  await issue(page).check();
  await pull(page).check();
  await page
    .getByRole("button", { name: "Edit labels for selected items" })
    .click();
  await expect(page.getByLabel("Bug", { exact: true })).toHaveAttribute(
    "aria-checked",
    "mixed",
  );
  await page.getByLabel("Help wanted", { exact: true }).check();
  await page.getByRole("button", { name: "Apply labels", exact: true }).click();
  await expect(
    page.getByRole("region", { name: "Bulk discussion actions" }),
  ).toHaveCount(0);
  expect(model.items[0].labels.map((l) => l.id)).toEqual([1, 2]);
  expect(model.items[1].labels.map((l) => l.id)).toEqual([2]);
  expect(model.items[2].labels).toEqual([]);
  await issue(page).check();
  await pull(page).check();
  await page
    .getByRole("button", { name: "Edit assignees for selected items" })
    .click();
  await page.getByLabel("Alice", { exact: true }).check();
  await page
    .getByRole("button", { name: "Apply assignees", exact: true })
    .click();
  await expect(
    page.getByRole("region", { name: "Bulk discussion actions" }),
  ).toHaveCount(0);
  await issue(page).check();
  await pull(page).check();
  await page
    .getByRole("button", { name: "Set milestone for selected items" })
    .click();
  await page.getByRole("textbox", { name: "Find a milestone" }).fill("Next");
  await page.getByRole("button", { name: "Next release Open" }).click();
  await expect(
    page.getByRole("region", { name: "Bulk discussion actions" }),
  ).toHaveCount(0);
  expect(model.writes.at(-1)?.action).toEqual({
    kind: "milestone",
    number: 42,
  });
  await issue(page).check();
  await pull(page).check();
  await page.getByRole("button", { name: "Mark as", exact: true }).click();
  await page.getByRole("menuitem", { name: "Closed", exact: true }).click();
  await expect(
    page.getByRole("region", { name: "Bulk discussion actions" }),
  ).toHaveCount(0);
  expect(model.items.map((item) => item.state)).toEqual([
    "closed",
    "closed",
    "open",
  ]);
  await expectNoAccessibilityViolations(page);
});

for (const view of ["issues", "pulls"])
  for (const status of [503, 403])
    test(`${view} recovers the original bulk action after an uncertain ${status} response`, async ({
      page,
    }) => {
      const model = await fixture(page);
      model.lose = true;
      model.lostStatus = status;
      await page.goto(`/team/project/${view}`);
      await page
        .getByRole("checkbox", { name: "Select all items on this page" })
        .check();
      await page.getByRole("button", { name: "Mark as", exact: true }).click();
      await page.getByRole("menuitem", { name: "Closed", exact: true }).click();
      await expect(
        page.getByRole("button", { name: "Retry original bulk action" }),
      ).toBeVisible();
      await expect(
        page.getByRole("button", { name: "Clear selection" }),
      ).toBeDisabled();
      await page
        .getByRole("button", { name: "Retry original bulk action" })
        .click();
      await expect(
        page.getByRole("button", { name: "Retry original bulk action" }),
      ).toHaveCount(0);
      expect(model.writes[1]).toEqual(model.writes[0]);
      expect(
        model.items.filter((item) => item.state === "closed"),
      ).toHaveLength(view === "issues" ? 3 : 1);
    });

test("confirmed conflicts preserve selection until explicit reload", async ({
  page,
}) => {
  const model = await fixture(page);
  model.conflict = true;
  await page.goto("/team/project/issues");
  await issue(page).check();
  await page.getByRole("button", { name: "Mark as", exact: true }).click();
  await page
    .getByRole("menuitem", { name: "Closed as not planned", exact: true })
    .click();
  await expect(page.getByRole("alert")).toContainText(
    "A selected item changed",
  );
  await expect(issue(page)).toBeChecked();
  await expect(
    page.getByRole("button", { name: "Mark as", exact: true }),
  ).toBeDisabled();
  await page.getByRole("button", { name: "Reload selected items" }).click();
  await expect(issue(page)).not.toBeChecked();
  expect(model.items[0].state).toBe("open");
  await issue(page).check();
  await page.getByRole("button", { name: "Mark as", exact: true }).click();
  await page
    .getByRole("menuitem", { name: "Closed as not planned", exact: true })
    .click();
  await expect(issue(page)).not.toBeChecked();
  expect(model.items[0].state).toBe("closed");
  const { request_id: previousId, ...previous } = model.writes[0];
  const { request_id: nextId, ...next } = model.writes[1];
  expect(next).toEqual(previous);
  expect(nextId).not.toBe(previousId);
});

test("bulk triage stays usable on narrow dark screens and hides for readers", async ({
  page,
}) => {
  const model = await fixture(page);
  await page.goto("/team/project/issues");
  await page.setViewportSize({ width: 390, height: 844 });
  await selectDarkTheme(page);
  const [checkbox, count] = await Promise.all([
    page
      .getByRole("checkbox", { name: "Select all items on this page" })
      .boundingBox(),
    page.getByRole("heading", { name: "3 results", exact: true }).boundingBox(),
  ]);
  expect(
    Math.abs(
      checkbox!.y + checkbox!.height / 2 - (count!.y + count!.height / 2),
    ),
  ).toBeLessThan(1);
  await page
    .getByRole("checkbox", { name: "Select all items on this page" })
    .check();
  await expectNoAccessibilityViolations(page);
  expect(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= innerWidth,
    ),
  ).toBe(true);
  model.canManage = false;
  await page.reload();
  await expect(
    page.getByRole("link", { name: "First issue", exact: true }),
  ).toBeVisible();
  await expect(
    page.getByRole("checkbox", { name: "Select all items on this page" }),
  ).toHaveCount(0);
});

test("an unknown bulk action survives a later access failure and recovers once", async ({
  page,
}) => {
  const model = await fixture(page);
  model.lose = true;
  await page.goto("/team/project/issues");
  await issue(page).check();
  await page.getByRole("button", { name: "Mark as", exact: true }).click();
  await page
    .getByRole("menuitem", { name: "Closed as completed", exact: true })
    .click();
  await expect(
    page.getByRole("button", { name: "Retry original bulk action" }),
  ).toBeVisible();
  model.preflightStatus = 404;
  await page
    .getByRole("button", { name: "Retry original bulk action" })
    .click();
  await expect(page.getByRole("alert")).toContainText(
    "Repository access unavailable",
  );
  await expect(
    page.getByRole("button", { name: "Clear selection" }),
  ).toBeDisabled();
  model.preflightStatus = 0;
  await page
    .getByRole("button", { name: "Retry original bulk action" })
    .click();
  await expect(
    page.getByRole("button", { name: "Retry original bulk action" }),
  ).toHaveCount(0);
  expect(model.writes[2]).toEqual(model.writes[0]);
  expect(model.items[0].version).toBe(2);
});

test("a retained bulk selection cannot follow a reused repository name", async ({
  page,
}) => {
  const model = await fixture(page);
  await page.goto("/team/project/issues");
  await issue(page).check();
  await expect(
    page.getByRole("button", { name: "Mark as", exact: true }),
  ).toBeEnabled();
  const original = model.repository_id;
  model.repository_id = "00000000-0000-4000-8000-000000000100";
  await page.getByRole("button", { name: "Mark as", exact: true }).click();
  await page
    .getByRole("menuitem", { name: "Closed as completed", exact: true })
    .click();
  await expect(
    page.getByRole("button", { name: "Retry original bulk action" }),
  ).toBeVisible();
  expect(model.writes[0].repository_id).toBe(original);
  expect(model.items[0].state).toBe("open");
  model.repository_id = original;
  await page
    .getByRole("button", { name: "Retry original bulk action" })
    .click();
  await expect(
    page.getByRole("button", { name: "Retry original bulk action" }),
  ).toHaveCount(0);
  expect(model.items[0].state).toBe("closed");
});

test("a pending bulk reply refreshes the current query instead of restoring its old page", async ({
  page,
}) => {
  await fixture(page);
  let release!: () => void;
  const waiting = new Promise<void>((resolve) => {
    release = resolve;
  });
  await page.route("**/discussions/bulk?**", async (route) => {
    await waiting;
    await route.fallback();
  });
  await page.goto("/team/project/issues?before=old-page");
  await issue(page).check();
  await page.getByRole("button", { name: "Mark as", exact: true }).click();
  await page
    .getByRole("menuitem", { name: "Closed as completed", exact: true })
    .click();
  await expect(
    page.getByRole("button", { name: "Clear selection" }),
  ).toBeDisabled();
  // Keep the component mounted while a different list query becomes current.
  await page.evaluate(() => {
    history.pushState({}, "", "/team/project/issues?q=is%3Aclosed");
    dispatchEvent(new PopStateEvent("popstate"));
  });
  await expect(page).toHaveURL(/q=is%3Aclosed/);
  release();
  await expect(
    page.getByRole("region", { name: "Bulk discussion actions" }),
  ).toHaveCount(0);
  await expect(page).toHaveURL(/q=is%3Aclosed/);
});
