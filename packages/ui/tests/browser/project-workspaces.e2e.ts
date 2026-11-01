import { test, expect, type Page } from "@playwright/test";
import { pickDate } from "./date-picker";
import {
  expectNoAccessibilityViolations,
  selectDarkTheme,
} from "./accessibility";
import type {
  WorkspaceProject,
  WorkspaceCommand,
} from "../../src/features/projects/workspace/model";
import type { Item, Outcome, Work } from "../../src/features/projects/model";

const organization = "01900000-0000-7000-8000-000000000001";
const actor = "01900000-0000-7000-8000-000000000002";
const projectId = "01900000-0000-7000-8000-000000000003";
const root = `/api/organizations/${organization}/projects`;
const home = `/-/organizations/${organization}/projects`;
const repositories = [
  { id: "01900000-0000-7000-8000-000000000011", owner: "team", name: "server" },
  { id: "01900000-0000-7000-8000-000000000012", owner: "team", name: "client" },
];
const views = [
  {
    id: "01900000-0000-7000-8000-000000000021",
    name: "Board",
    layout: "board" as const,
    query: "",
    status: null,
    hide_closed: false,
  },
  {
    id: "01900000-0000-7000-8000-000000000022",
    name: "Table",
    layout: "table" as const,
    query: "",
    status: null,
    hide_closed: false,
  },
  {
    id: "01900000-0000-7000-8000-000000000023",
    name: "Roadmap",
    layout: "roadmap" as const,
    query: "",
    status: null,
    hide_closed: false,
  },
];
async function fixture(page: Page) {
  let project: WorkspaceProject = {
    id: projectId,
    definition: {
      name: "Platform release",
      description: "Ship the API and client together",
      readme:
        "# Release scope\n\nOne coordinated release across two repositories.",
      statuses: ["Todo", "In progress", "Done"],
      fields: [],
      views,
      is_template: false,
    },
    properties: {
      status: "planned",
      health: "on_track",
      lead: actor,
      start: null,
      target: null,
    },
    version: 1,
    archived: false,
    created_at: Date.now(),
    updated_at: Date.now(),
  };
  const linked = new Set<string>();
  const items: { item: Item; repository: string | null }[] = [];
  const decisions = new Map<string, Outcome>();
  const submissions: { request_id: string; command: WorkspaceCommand }[] = [];
  let lose = false;
  function work(repository: string): Work {
    return {
      subject: { kind: "issue", number: 1 },
      title: repository === repositories[0].id ? "Ship API" : "Ship client",
      state: "open",
    };
  }
  await page.route("**/api/**", async (route) => {
    const request = route.request();
    const url = new URL(request.url());
    const path = url.pathname;
    if (path === "/api/session")
      return route.fulfill({
        json: {
          authenticated: true,
          mode: "oidc",
          user: { issuer: "test", subject: "alice", name: "Alice" },
          csrf: "test-csrf",
        },
      });
    if (path === "/api/repos")
      return route.fulfill({
        json: {
          repositories: repositories.map((repo) => ({
            ...repo,
            access: "write",
            archived: false,
            can_admin: true,
            description: "",
            archive_version: 0,
            protection_version: 0,
          })),
        },
      });
    if (path === `/api/organizations/${organization}`)
      return route.fulfill({
        json: {
          organization: {
            id: organization,
            name: "Engineering",
            description: "Product engineering",
            version: 1,
          },
          actor,
          role: "owner",
        },
      });
    if (path === `/api/organizations/${organization}/members`)
      return route.fulfill({
        json: {
          items: [
            {
              actor,
              role: "owner",
              version: 1,
              profile: { id: actor, handle: "alice", name: "Alice" },
            },
          ],
          next: null,
          revision: 1,
        },
      });
    if (path === root)
      return route.fulfill({
        json: { items: [project], next: null, revision: 1, can_create: true },
      });
    if (path === `${root}/commands` || path === `${root}/cancel`) {
      expect(request.headers()["x-csrf-token"]).toBe("test-csrf");
      expect(request.headers()["x-crab-repository-id"]).toBeUndefined();
      const body = request.postDataJSON() as {
        request_id: string;
        command: WorkspaceCommand;
      };
      submissions.push(body);
      let outcome = decisions.get(body.request_id);
      if (!outcome) {
        const command = body.command;
        if (path.endsWith("/cancel")) outcome = { outcome: "cancelled" };
        else if (command.kind === "create") {
          project = {
            ...project,
            id: command.id,
            version: 1,
            definition: { ...project.definition, name: command.name },
          };
          outcome = { outcome: "applied", project: project.id, version: 1 };
        } else if (command.version !== project.version)
          outcome = {
            outcome: "rejected",
            reason:
              "The project changed. Review its current version before saving your draft.",
          };
        else {
          project = { ...project, version: project.version + 1 };
          if (command.kind === "repository") {
            if (command.linked) linked.add(command.repository);
            else linked.delete(command.repository);
          }
          if (command.kind === "item") {
            const row = { item: command.item, repository: command.repository };
            const index = items.findIndex(
              (item) => item.item.id === command.item.id,
            );
            if (index < 0) items.push(row);
            else items[index] = row;
          }
          if (command.kind === "remove") {
            const index = items.findIndex(
              (item) => item.item.id === command.item,
            );
            if (index >= 0) items.splice(index, 1);
          }
          if (command.kind === "planning")
            project = { ...project, properties: command.properties };
          if (command.kind === "edit")
            project = {
              ...project,
              definition: command.definition,
              archived: command.archived,
            };
          outcome = {
            outcome: "applied",
            project: project.id,
            version: project.version,
          };
        }
        decisions.set(body.request_id, outcome);
      }
      if (lose) {
        lose = false;
        return route.abort("failed");
      }
      return route.fulfill({ json: outcome });
    }
    if (path === `${root}/${project.id}`)
      return route.fulfill({
        json: {
          project,
          can_manage: true,
          repositories: repositories.filter((repo) => linked.has(repo.id)),
          restricted_repositories: 0,
          lead: { id: actor, name: "Alice", handle: "alice" },
        },
      });
    if (path === `${root}/${project.id}/candidates`)
      return route.fulfill({
        json: { items: [work(url.searchParams.get("repository")!)] },
      });
    if (path === `${root}/${project.id}/items`) {
      const repository = url.searchParams.get("repository");
      const status = url.searchParams.get("status");
      const matching = items.filter(
        (row) =>
          (!repository || row.repository === repository) &&
          (!status || row.item.status === status),
      );
      const counts: Record<string, number> = {};
      for (const row of items.filter(
        (row) => !repository || row.repository === repository,
      ))
        counts[row.item.status] = (counts[row.item.status] ?? 0) + 1;
      return route.fulfill({
        json: {
          page: {
            project,
            items: matching.map((row) => ({
              item: row.item,
              repository:
                repositories.find((repo) => repo.id === row.repository) ?? null,
              work: row.repository ? work(row.repository) : null,
              assignees: [],
              parent: null,
              progress: null,
              links: [],
              more_links: false,
            })),
            counts,
            total: matching.length,
            next: null,
            revision: project.version,
          },
          can_manage: true,
        },
      });
    }
    return route.fulfill({
      status: 404,
      json: { error: { message: "Not found" } },
    });
  });
  return {
    submissions,
    items,
    addDateField() {
      project.definition.fields.push({
        id: "01900000-0000-7000-8000-000000000031",
        name: "Review date",
        definition: { kind: "date" },
      });
    },
    loseNext() {
      lose = true;
    },
  };
}
test("one project coordinates matching issue numbers across repositories with planning views and properties", async ({
  page,
}) => {
  const state = await fixture(page);
  await page.goto(`${home}/${projectId}`);
  await page.getByRole("button", { name: "Manage", exact: true }).click();
  const linking = page.getByRole("dialog", { name: "Link repositories" });
  await linking.getByLabel("team/server", { exact: true }).click();
  await expect(
    linking.getByLabel("team/server", { exact: true }),
  ).toBeChecked();
  await linking.getByLabel("team/client", { exact: true }).click();
  await expect(
    linking.getByLabel("team/client", { exact: true }),
  ).toBeChecked();
  await linking.getByRole("button", { name: "Done", exact: true }).click();
  for (const [repo, title] of [
    ["team/server", "Ship API"],
    ["team/client", "Ship client"],
  ]) {
    await page
      .getByRole("button", { name: "Add work", exact: true })
      .first()
      .click();
    const dialog = page.getByRole("dialog", { name: "Add work to project" });
    await dialog
      .getByRole("combobox", { name: "Source repository", exact: true })
      .click();
    await page.getByRole("option", { name: repo, exact: true }).click();
    await dialog
      .getByRole("button", { name: `${title} #1`, exact: true })
      .click();
    await dialog
      .getByRole("button", { name: "Add to project", exact: true })
      .click();
    await expect(dialog).toHaveCount(0);
  }
  await expect(
    page.getByRole("link", { name: "Ship API", exact: true }),
  ).toHaveAttribute("href", "/team/server/issues/1");
  await expect(
    page.getByRole("link", { name: "Ship client", exact: true }),
  ).toHaveAttribute("href", "/team/client/issues/1");
  expect(new Set(state.items.map((item) => item.repository)).size).toBe(2);
  await page.screenshot({
    path: test.info().outputPath("workspace-board.png"),
  });
  await page
    .getByRole("combobox", { name: "Filter by repository", exact: true })
    .click();
  await page.getByRole("option", { name: "team/client", exact: true }).click();
  await expect(
    page.getByRole("link", { name: "Ship API", exact: true }),
  ).toHaveCount(0);
  await expect(
    page.getByRole("link", { name: "Ship client", exact: true }),
  ).toBeVisible();
  await page.getByRole("tab", { name: "Table", exact: true }).click();
  await expect(
    page.getByRole("combobox", { name: "Status for Ship API", exact: true }),
  ).toBeVisible();
  await page.getByRole("combobox", { name: "Layout", exact: true }).click();
  await page.getByRole("option", { name: "Roadmap", exact: true }).click();
  await expect(
    page.getByRole("table", { name: "Project roadmap", exact: true }),
  ).toBeVisible();
  await page.getByRole("button", { name: "Edit", exact: true }).click();
  const properties = page.getByRole("dialog", { name: "Project properties" });
  await properties
    .getByRole("combobox", { name: "Project status", exact: true })
    .click();
  await page.getByRole("option", { name: "In progress", exact: true }).click();
  await pickDate(page, "Start date", "2026-11-01");
  await pickDate(page, "Target date", "2026-11-30");
  await properties
    .getByRole("button", { name: "Save properties", exact: true })
    .click();
  await expect(
    page.getByRole("complementary", { name: "Project details" }),
  ).toContainText("2026-11-30");
  await page.getByRole("tab", { name: "Overview", exact: true }).click();
  await expect(
    page.getByRole("heading", { name: "Release scope", exact: true }),
  ).toBeVisible();
  await expectNoAccessibilityViolations(page);
  await page.screenshot({
    path: test.info().outputPath("workspace-overview.png"),
  });
  await page.setViewportSize({ width: 390, height: 900 });
  await selectDarkTheme(page);
  await expectNoAccessibilityViolations(page);
  await page.screenshot({
    path: test.info().outputPath("workspace-mobile-dark.png"),
  });
  expect(
    await page.evaluate(() => document.documentElement.scrollWidth),
  ).toBeLessThanOrEqual(390);
});
test.describe("project date selection", () => {
  test.use({ timezoneId: "Pacific/Kiritimati" });
  test("calendar controls preserve civil dates, enforce ranges and clear optional values on mobile", async ({
    page,
  }) => {
    const state = await fixture(page);
    state.addDateField();
    await page.setViewportSize({ width: 390, height: 844 });
    await page.goto(`${home}/${projectId}`);
    await selectDarkTheme(page);
    await page
      .getByRole("button", { name: "Add work", exact: true })
      .first()
      .click();
    const editor = page.getByRole("dialog", {
      name: "Add work to project",
      exact: true,
    });
    await editor
      .getByLabel("Title", { exact: true })
      .fill("Plan release validation");
    await pickDate(page, "Start date", "2026-11-10");
    await editor.getByRole("button", { name: /^Target date:/ }).click();
    const target = page.getByRole("dialog", {
      name: "Target date calendar",
      exact: true,
    });
    await expect(
      target.getByRole("button", { name: /November 9th, 2026/ }),
    ).toBeDisabled();
    await page.keyboard.press("Escape");
    await expect(editor).toBeVisible();
    await expect(
      editor.getByRole("button", { name: /^Target date:/ }),
    ).toBeFocused();
    await pickDate(page, "Target date", "2026-11-20");
    await pickDate(page, "Review date", "2026-11-15");
    await editor.getByRole("button", { name: /^Review date:/ }).click();
    const calendar = page.getByRole("dialog", {
      name: "Review date calendar",
      exact: true,
    });
    await calendar.getByRole("button", { name: /November 15th, 2026/ }).hover();
    await expect(
      // Radix Select retains an aria-hidden native select for form events.
      editor
        .or(calendar)
        .locator('input[type="date"], select:not([aria-hidden="true"])'),
    ).toHaveCount(0);
    await expectNoAccessibilityViolations(page);
    expect(
      await page.evaluate(() => document.documentElement.scrollWidth),
    ).toBeLessThanOrEqual(390);
    await page.screenshot({
      path: test.info().outputPath("date-picker-mobile-dark.png"),
    });
    await page.keyboard.press("Escape");
    await editor
      .getByRole("button", { name: "Add to project", exact: true })
      .click();
    await expect(editor).toHaveCount(0);
    expect(state.items[0].item).toMatchObject({
      start: "2026-11-10",
      end: "2026-11-20",
      values: { "01900000-0000-7000-8000-000000000031": "2026-11-15" },
    });
    await page
      .getByRole("button", {
        name: "Edit Plan release validation",
        exact: true,
      })
      .click();
    await page.getByRole("button", { name: /^Start date:/ }).click();
    await page
      .getByRole("dialog", { name: "Start date calendar", exact: true })
      .getByRole("button", { name: "Clear date", exact: true })
      .click();
    await page.getByRole("button", { name: "Save item", exact: true }).click();
    await expect(
      page.getByRole("dialog", { name: "Project item", exact: true }),
    ).toHaveCount(0);
    expect(state.items[0].item).toMatchObject({
      start: null,
      end: "2026-11-20",
    });
  });
});

test("an uncertain organization project creation recovers the original intent", async ({
  page,
}) => {
  const state = await fixture(page);
  await page.goto(home);
  state.loseNext();
  await page.getByRole("button", { name: "New project", exact: true }).click();
  const dialog = page.getByRole("dialog", { name: "New project", exact: true });
  await dialog
    .getByLabel("Project name", { exact: true })
    .fill("Recoverable release");
  await dialog
    .getByRole("button", { name: "Create project", exact: true })
    .click();
  await expect(
    dialog.getByRole("button", { name: "Retry submission", exact: true }),
  ).toBeVisible();
  await dialog
    .getByRole("button", { name: "Retry submission", exact: true })
    .click();
  await expect(
    page.getByRole("heading", { name: "Recoverable release", exact: true }),
  ).toBeVisible();
  expect(state.submissions[1]).toEqual(state.submissions[0]);
});

test("stale project filters can be reset without losing the workspace", async ({
  page,
}) => {
  await fixture(page);
  await page.route(
    (url) =>
      url.pathname.endsWith("/items") &&
      url.searchParams.get("status") === "Retired",
    (route) =>
      route.fulfill({
        status: 400,
        json: { error: { message: "Project status filter is invalid" } },
      }),
  );
  await page.goto(`${home}/${projectId}?status=Retired`);
  await expect(page.getByRole("alert")).toContainText(
    "Project status filter is invalid",
  );
  await page
    .getByRole("button", { name: "Reset filters", exact: true })
    .click();
  await expect(
    page.getByRole("combobox", { name: "Filter by status", exact: true }),
  ).toHaveText("All statuses");
});
