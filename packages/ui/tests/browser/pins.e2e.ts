import { expect, test, type Page } from "@playwright/test";
import { mockPullRequest } from "./pulls/fixture";
import { emptyReactions } from "./fixtures";
import {
  expectNoAccessibilityViolations,
  selectDarkTheme,
} from "./accessibility";
import type { Issue } from "@/features/issues/model";

async function fixture(page: Page, numbers = [1, 2, 3], canManage = true) {
  const base = await mockPullRequest(page);
  const issues: Issue[] = Array.from({ length: 4 }, (_, i) => ({
    number: i + 1,
    title: [
      "Keyboard navigation across large repositories",
      "A completed improvement",
      "An intentionally declined proposal",
      "The next priority",
    ][i],
    body: "Issue context",
    state: i === 0 || i === 3 ? "open" : "closed",
    state_reason: i === 1 ? "completed" : i === 2 ? "not_planned" : null,
    closed_at: i === 1 || i === 2 ? 1700000010000 : null,
    author: { name: "Alice", profile: null },
    version: 1,
    created_at: 1700000000000,
    updated_at: 1700000000000,
    can_edit: canManage,
    can_close: canManage,
    can_label: false,
    can_assign: false,
    labels: [],
    assignees: [],
    reactions: emptyReactions(),
  }));
  type Intent = { request_id: string; version: number; numbers: number[] };
  const model = {
    numbers,
    version: numbers.length ? 1 : 0,
    canManage,
    failure: "" as "" | "lost" | "conflict" | "context",
    readFailure: false,
    writes: [] as Intent[],
  };
  const receipts = new Map<string, number>();
  const rejected = new Set<string>();
  await page.route(
    (url) => url.pathname === "/api/repos/team/project/issues/pins",
    async (route) => {
      if (route.request().method() === "GET") {
        if (model.readFailure)
          return route.fulfill({
            status: 503,
            json: { error: { message: "Pin collection unavailable" } },
          });
        return route.fulfill({
          json: {
            version: model.version,
            can_manage: model.canManage,
            items: model.numbers.map((number) => ({
              ...issues[number - 1],
              comment_count: number + 2,
            })),
          },
        });
      }
      expect(route.request().headers()["x-crab-repository-id"]).toBe(
        base.lifecycle.repositoryId,
      );
      const input = route.request().postDataJSON() as Intent;
      model.writes.push(input);
      if (model.failure === "context") {
        model.failure = "";
        return route.fulfill({
          status: 403,
          json: {
            error: { code: "forbidden", message: "Current access unavailable" },
          },
        });
      }
      const saved = receipts.get(input.request_id);
      if (saved !== undefined)
        return route.fulfill({ json: { version: saved } });
      if (
        model.failure === "conflict" ||
        input.version !== model.version ||
        rejected.has(input.request_id)
      ) {
        model.failure = "";
        rejected.add(input.request_id);
        return route.fulfill({
          status: 409,
          headers: { "X-Crab-Write-Outcome": "rejected" },
          json: {
            error: { code: "conflict", message: "Pins changed elsewhere" },
          },
        });
      }
      model.numbers = input.numbers;
      model.version++;
      receipts.set(input.request_id, model.version);
      if (model.failure === "lost") {
        model.failure = "";
        return route.abort("failed");
      }
      return route.fulfill({ json: { version: model.version } });
    },
  );
  await page.route(
    (url) => /^\/api\/repos\/team\/project\/issues\/\d+$/.test(url.pathname),
    (route) => {
      const number = Number(
        new URL(route.request().url()).pathname.split("/").at(-1),
      );
      return route.fulfill({ json: issues[number - 1] });
    },
  );
  await page.route("**/api/repos/team/project/issues?**", (route) =>
    route.fulfill({
      json: {
        items: issues
          .filter((issue) => issue.state === "open")
          .map((issue) => ({
            ...issue,
            kind: "issue",
            comment_count: 0,
            reaction_count: 0,
          })),
        next: null,
        matching: 2,
        counts: { open: 2, closed: 2, merged: 0 },
        authored_by_me: false,
        assigned_to_me: false,
        review_requested_from_me: false,
        navigation: {
          kind: "state",
          selected: "open",
          open: "is:open",
          closed: "is:closed",
        },
      },
    }),
  );
  return { model, issues };
}

function pins(page: Page) {
  return page.getByRole("region", { name: "Pinned issues", exact: true });
}
async function order(page: Page) {
  return pins(page).locator("[data-slot=card-title] a").allTextContents();
}

test("cards precede search, retain closed issues, reorder by keyboard and drag, and unpin without losing focus", async ({
  page,
}) => {
  const { model, issues } = await fixture(page);
  await page.goto("/team/project/issues?q=is%3Aopen&powerbar=1");
  await expect(pins(page).getByRole("listitem")).toHaveCount(3);
  expect(await order(page)).toEqual(issues.slice(0, 3).map((i) => i.title));
  await expect(
    pins(page).getByRole("img", { name: "Closed as not planned" }),
  ).toBeVisible();
  await expect(
    pins(page).getByRole("link", { name: "5 comments on issue #3" }),
  ).toBeVisible();
  const bounds = (await pins(page).boundingBox())!;
  expect(bounds.y + bounds.height).toBeLessThan(
    (await page.getByRole("search", { name: "Search issues" }).boundingBox())!
      .y,
  );
  await expect(
    page.getByRole("group", { name: "Pinned issues request widget" }),
  ).toContainText(issues[0].title);
  await page
    .getByRole("button", { name: "Reorder pinned issue #3", exact: true })
    .focus();
  await page.keyboard.press("Enter");
  await page
    .getByRole("menuitem", { name: "Move earlier", exact: true })
    .click();
  await expect
    .poll(() => order(page))
    .toEqual([issues[0].title, issues[2].title, issues[1].title]);
  expect(model.numbers).toEqual([1, 3, 2]);
  const data = await page.evaluateHandle(() => new DataTransfer());
  await page
    .getByRole("button", { name: "Reorder pinned issue #2", exact: true })
    .dispatchEvent("dragstart", { dataTransfer: data });
  await pins(page)
    .getByRole("listitem")
    .first()
    .dispatchEvent("drop", { dataTransfer: data });
  await expect
    .poll(() => order(page))
    .toEqual([issues[1].title, issues[0].title, issues[2].title]);
  await page
    .getByRole("button", { name: "Pinned issue #2 actions", exact: true })
    .click();
  await page
    .getByRole("menuitem", { name: "Unpin issue", exact: true })
    .click();
  await expect(pins(page).getByRole("listitem")).toHaveCount(2);
  await expect(pins(page)).toBeFocused();
  expect(new URL(page.url()).searchParams.get("q")).toBe("is:open");
  await expectNoAccessibilityViolations(page);
});

test("sidebar pins and unpins, preserves the issue version, and explains the three-card limit", async ({
  page,
}) => {
  const { model, issues } = await fixture(page, []);
  await page.goto("/team/project/issues/1");
  await page.getByRole("button", { name: "Pin issue", exact: true }).click();
  await expect(
    page.getByRole("button", { name: "Unpin issue", exact: true }),
  ).toBeEnabled();
  expect(model.numbers).toEqual([1]);
  expect(issues[0].version).toBe(1);
  await expect(
    page.getByRole("button", { name: "Unpin issue", exact: true }),
  ).toBeFocused();
  await page.getByRole("button", { name: "Unpin issue", exact: true }).click();
  await expect(
    page.getByRole("button", { name: "Pin issue", exact: true }),
  ).toBeEnabled();
  model.numbers = [1, 2, 3];
  model.version++;
  await page.goto("/team/project/issues/4");
  await expect(
    page.getByRole("button", { name: "Pin issue", exact: true }),
  ).toBeDisabled();
  await expect(
    page.getByText(
      "Three issues are already pinned. Unpin one before adding another.",
    ),
  ).toBeVisible();
  await expectNoAccessibilityViolations(page);
});

test("a lost reply retains its original intent across a context failure and recovers the current order", async ({
  page,
}) => {
  const { model, issues } = await fixture(page);
  await page.goto("/team/project/issues");
  model.failure = "lost";
  await page
    .getByRole("button", { name: "Pinned issue #1 actions", exact: true })
    .click();
  await page
    .getByRole("menuitem", { name: "Unpin issue", exact: true })
    .click();
  await expect(
    page.getByRole("button", {
      name: "Retry original pin change",
      exact: true,
    }),
  ).toBeVisible();
  await expect(pins(page).getByRole("listitem")).toHaveCount(3);
  model.failure = "context";
  await page
    .getByRole("button", { name: "Retry original pin change", exact: true })
    .click();
  await expect(page.getByRole("alert")).toContainText(
    "Current access unavailable",
  );
  await expect(
    page.getByRole("button", {
      name: "Retry original pin change",
      exact: true,
    }),
  ).toBeEnabled();
  model.numbers = [3, 2];
  model.version++;
  await page
    .getByRole("button", { name: "Retry original pin change", exact: true })
    .click();
  await expect
    .poll(() => order(page))
    .toEqual([issues[2].title, issues[1].title]);
  expect(model.writes).toHaveLength(3);
  expect(model.writes[1]).toEqual(model.writes[0]);
  expect(model.writes[2]).toEqual(model.writes[0]);
});

test("a proven conflict requires reload before a fresh pin change; read failures withdraw stale cards", async ({
  page,
}) => {
  const { model } = await fixture(page);
  await page.goto("/team/project/issues");
  model.failure = "conflict";
  await page
    .getByRole("button", { name: "Pinned issue #1 actions", exact: true })
    .click();
  await page
    .getByRole("menuitem", { name: "Unpin issue", exact: true })
    .click();
  await expect(page.getByRole("alert")).toContainText("Pins changed elsewhere");
  await expect(
    page.getByRole("button", { name: "Pinned issue #1 actions", exact: true }),
  ).toBeDisabled();
  await expect(
    page.getByRole("button", {
      name: "Retry original pin change",
      exact: true,
    }),
  ).toHaveCount(0);
  model.readFailure = true;
  await page
    .getByRole("button", { name: "Reload pinned issues", exact: true })
    .click();
  await expect(page.getByRole("alert")).toContainText(
    "Pin collection unavailable",
  );
  await expect(pins(page)).toHaveCount(0);
  model.readFailure = false;
  model.numbers = [3, 2];
  model.version++;
  await page.getByRole("button", { name: "Try again", exact: true }).click();
  await expect(pins(page).getByRole("listitem")).toHaveCount(2);
  await page
    .getByRole("button", { name: "Pinned issue #2 actions", exact: true })
    .click();
  await page
    .getByRole("menuitem", { name: "Unpin issue", exact: true })
    .click();
  await expect(pins(page).getByRole("listitem")).toHaveCount(1);
  expect(model.writes[1].request_id).not.toBe(model.writes[0].request_id);
  expect(model.writes[1].version).toBe(model.version - 1);
});

test("readers see cards without pin controls; mobile and dark cards stay accessible and within the page", async ({
  page,
}) => {
  await fixture(page, [1, 2, 3], false);
  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto("/team/project/issues");
  await expect(pins(page).getByRole("listitem")).toHaveCount(3);
  await expect(pins(page).getByRole("button")).toHaveCount(0);
  await expect(
    page.getByRole("group", { name: "Pinned issues request widget" }),
  ).toHaveCount(0);
  expect(
    await page.evaluate(() => document.documentElement.scrollWidth),
  ).toBeLessThanOrEqual(390);
  await expectNoAccessibilityViolations(page);
  await selectDarkTheme(page);
  await expectNoAccessibilityViolations(page);
  await page.goto("/team/project/issues/1");
  await expect(
    page.getByRole("heading", { name: /Keyboard navigation/ }),
  ).toBeVisible();
  await expect(
    page.getByRole("button", { name: /^(Pin|Unpin) issue$/ }),
  ).toHaveCount(0);
});
