import { expect, test, type Page } from "@playwright/test";
import type { RepositoryAssignee } from "@/lib/api";
import { emptyReactions } from "./fixtures";
import {
  expectNoAccessibilityViolations,
  selectDarkTheme,
} from "./accessibility";

async function fixture(page: Page) {
  const current: RepositoryAssignee = {
    id: "00000000-0000-4000-8000-000000000001",
    name: "Current collaborator",
    profile: null,
  };
  const former: RepositoryAssignee = {
    id: "00000000-0000-4000-8000-000000000002",
    name: "Former collaborator",
    profile: {
      id: "00000000-0000-4000-8000-000000000002",
      handle: "former-user",
      name: "Former collaborator",
    },
  };
  const state = {
    version: 1,
    assigned: [former],
    reads: 0,
    writes: [] as Array<{ version: number; assignee_ids: string[] }>,
  };
  await page.route("**/api/**", async (route) => {
    const request = route.request();
    const path = new URL(request.url()).pathname;
    if (path === "/api/session")
      return route.fulfill({
        json: {
          authenticated: true,
          mode: "local",
          user: null,
          csrf: "assignee-csrf",
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
    if (path === "/api/repos/team/project/assignees") {
      state.reads += 1;
      return route.fulfill({ json: { items: [current], can_manage: true } });
    }
    if (path === "/api/repos/team/project/issues/1") {
      if (request.method() === "PATCH") {
        const input = request.postDataJSON() as {
          version: number;
          assignee_ids: string[];
        };
        expect(Object.keys(input).sort()).toEqual(["assignee_ids", "version"]);
        expect(request.headers()["x-csrf-token"]).toBe("assignee-csrf");
        state.writes.push(input);
        if (input.version !== state.version)
          return route.fulfill({
            status: 409,
            json: {
              error: {
                code: "conflict",
                message: "This issue changed. Reload it before trying again.",
              },
            },
          });
        expect(input.assignee_ids.every((id) => id === current.id)).toBe(true);
        state.assigned = input.assignee_ids.map(() => current);
        state.version += 1;
      }
      return route.fulfill({
        json: {
          number: 1,
          title: "Preserve historical assignment",
          author: "Alice",
          body: "Stored description",
          state: "open",
          version: state.version,
          created_at: 1700000000000,
          updated_at: 1700000000000,
          can_edit: false,
          can_close: false,
          can_label: false,
          can_assign: true,
          labels: [],
          assignees: state.assigned,
          reactions: emptyReactions(),
        },
      });
    }
    if (path.endsWith("/milestone"))
      return route.fulfill({
        json: { milestone: null, version: 0, can_manage: false },
      });
    if (
      path.endsWith("/comments") ||
      path.endsWith("/activity") ||
      path.endsWith("/links")
    )
      return route.fulfill({
        json: { items: [], next: null, can_manage: false },
      });
    throw new Error(
      `Unexpected assignment request: ${request.method()} ${path}`,
    );
  });
  return { state, current };
}

for (const width of [1280, 390]) {
  test(`assignee picker removes unavailable people and searches current candidates at ${width}px`, async ({
    page,
  }) => {
    const { state, current } = await fixture(page);
    await page.setViewportSize({ width, height: 900 });
    await page.goto("/team/project/issues/1");
    await expect(
      page.getByRole("heading", { name: "Preserve historical assignment #1" }),
    ).toBeVisible();
    expect(state.reads).toBe(0);
    if (width === 390) await selectDarkTheme(page);
    const trigger = page.getByRole("button", {
      name: "Edit assignees",
      exact: true,
    });
    await trigger.click();
    const picker = page.getByRole("dialog", { name: "Assign people" });
    const search = picker.getByRole("textbox", { name: "Filter assignees" });
    await expect(search).toBeFocused();
    await search.fill("former");
    const former = picker.getByRole("checkbox", {
      name: /Former collaborator/,
    });
    await expect(former).toBeChecked();
    await former.uncheck();
    await expect(former).toBeDisabled();
    await search.fill("nobody-matches");
    await expect(picker.getByRole("status")).toHaveText(
      "No people match your search.",
    );
    await search.fill("current");
    await picker
      .getByRole("checkbox", { name: "Current collaborator", exact: true })
      .check();
    await expectNoAccessibilityViolations(page);
    expect(
      await picker.evaluate((element) => {
        const r = element.getBoundingClientRect();
        return r.left >= 0 && r.right <= innerWidth;
      }),
    ).toBe(true);
    await picker
      .getByRole("button", { name: "Apply assignees", exact: true })
      .click();
    await expect(picker).toBeHidden();
    await expect(
      page.getByRole("complementary", { name: "Metadata" }),
    ).toContainText("Current collaborator");
    expect(state.writes).toEqual([{ version: 1, assignee_ids: [current.id] }]);
    await trigger.click();
    await expect(
      picker.getByRole("checkbox", {
        name: "Current collaborator",
        exact: true,
      }),
    ).toBeChecked();
    await page.keyboard.press("Escape");
    await expect(trigger).toBeFocused();
    expect(state.reads).toBe(2);
  });
}

test("assignment conflict retains choices and requires a fresh record before retry", async ({
  page,
}) => {
  const { state, current } = await fixture(page);
  await page.goto("/team/project/issues/1");
  await page
    .getByRole("button", { name: "Edit assignees", exact: true })
    .click();
  const picker = page.getByRole("dialog", { name: "Assign people" });
  await picker.getByRole("checkbox", { name: /Former collaborator/ }).uncheck();
  const checkbox = picker.getByRole("checkbox", {
    name: "Current collaborator",
    exact: true,
  });
  await checkbox.check();
  state.version = 2;
  await picker
    .getByRole("button", { name: "Apply assignees", exact: true })
    .click();
  await expect(picker.getByRole("alert")).toContainText("This issue changed.");
  const inset = await picker.getByRole("alert").evaluate((element) => {
    const container = element.closest('[data-slot="popover-content"]');
    if (!container) throw new Error("Missing assignee popover");
    const outer = container.getBoundingClientRect();
    const inner = element.getBoundingClientRect();
    return { left: inner.left - outer.left, right: outer.right - inner.right };
  });
  expect(inset.left).toBeGreaterThanOrEqual(12);
  expect(inset.right).toBeGreaterThanOrEqual(12);
  await expect(checkbox).toBeChecked();
  expect(state.assigned[0]?.name).toBe("Former collaborator");
  await page.keyboard.press("Escape");
  await page.reload();
  await page
    .getByRole("button", { name: "Edit assignees", exact: true })
    .click();
  await picker.getByRole("checkbox", { name: /Former collaborator/ }).uncheck();
  await checkbox.check();
  await picker
    .getByRole("button", { name: "Apply assignees", exact: true })
    .click();
  await expect(picker).toBeHidden();
  expect(state.writes).toEqual([
    { version: 1, assignee_ids: [current.id] },
    { version: 2, assignee_ids: [current.id] },
  ]);
});
