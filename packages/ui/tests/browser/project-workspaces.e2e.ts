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
  const revisions = new Map<string, number>();
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
        } else if (
          (command.kind === "item" || command.kind === "remove") &&
          command.version !==
            (revisions.get(
              typeof command.item === "string" ? command.item : command.item.id,
            ) ?? 0)
        )
          outcome = {
            outcome: "item_conflict",
            project: project.id,
            item:
              typeof command.item === "string" ? command.item : command.item.id,
            version:
              revisions.get(
                typeof command.item === "string"
                  ? command.item
                  : command.item.id,
              ) ?? 0,
          };
        else if (
          command.kind !== "item" &&
          command.kind !== "remove" &&
          command.version !== project.version
        )
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
            revisions.set(command.item.id, command.version + 1);
            const row = { item: command.item, repository: command.repository };
            const index = items.findIndex(
              (item) => item.item.id === command.item.id,
            );
            if (index < 0) items.push(row);
            else items[index] = row;
          }
          if (command.kind === "remove") {
            revisions.set(command.item, command.version + 1);
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
          (!url.searchParams.has("item") ||
            row.item.id === url.searchParams.get("item")) &&
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
              version: revisions.get(row.item.id),
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
    editFirstItem(title: string) {
      const row = items[0];
      row.item = { ...row.item, title };
      revisions.set(row.item.id, (revisions.get(row.item.id) ?? 0) + 1);
      project = { ...project, version: project.version + 1 };
    },
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

test("a same-item conflict shows the saved edit and keeps the local draft for an explicit retry", async ({
  page,
}) => {
  const state = await fixture(page);
  await page.goto(`${home}/${projectId}`);
  await page
    .getByRole("button", { name: "Add work", exact: true })
    .first()
    .click();
  const editor = page.getByRole("dialog");
  await editor.getByLabel("Title", { exact: true }).fill("Original plan");
  await editor
    .getByRole("button", { name: "Add to project", exact: true })
    .click();
  await expect(editor).toHaveCount(0);
  await page
    .getByRole("button", { name: "Edit Original plan", exact: true })
    .click();
  await editor.getByLabel("Title", { exact: true }).fill("My retained draft");
  state.editFirstItem("Teammate's saved plan");
  await editor.getByRole("button", { name: "Save item", exact: true }).click();
  await expect(editor.getByRole("alert")).toContainText(
    "Your draft is preserved",
  );
  await editor
    .getByRole("button", { name: "Review saved item", exact: true })
    .click();
  await expect(
    editor.getByRole("cell", { name: "Teammate's saved plan", exact: true }),
  ).toBeVisible();
  await expect(
    editor.getByRole("cell", { name: "My retained draft", exact: true }),
  ).toBeVisible();
  await editor
    .getByRole("button", {
      name: "Keep my draft against this revision",
      exact: true,
    })
    .click();
  await editor.getByRole("button", { name: "Save item", exact: true }).click();
  await expect(editor).toHaveCount(0);
  expect(state.items[0].item.title).toBe("My retained draft");
  expect(state.submissions.at(-1)?.command).toMatchObject({
    kind: "item",
    version: 2,
  });
  expect(state.submissions.at(-1)?.request_id).not.toBe(
    state.submissions.at(-2)?.request_id,
  );
});

test("project history presents attributed field changes without overflowing a mobile viewport", async ({
  page,
}) => {
  await fixture(page);
  await page.route(
    `**/api/organizations/${organization}/projects/${projectId}/history*`,
    (route) =>
      route.fulfill({
        json: {
          items: [
            {
              version: 3,
              at_ms: Date.UTC(2026, 10, 1),
              actor: { id: actor, profile: { name: "Alice" } },
              target: {
                kind: "item",
                id: "01900000-0000-7000-8000-000000000088",
                source: null,
                restricted: false,
              },
              changes: [
                { field: "/status", before: "Todo", after: "In progress" },
              ],
            },
          ],
          next: null,
        },
      }),
  );
  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto(`${home}/${projectId}`);
  await page.getByRole("tab", { name: "History", exact: true }).click();
  const history = page.getByRole("region", {
    name: "Project history",
    exact: true,
  });
  await expect(history).toContainText("Alice");
  await expect(history).toContainText("Revision 3");
  await history
    .getByRole("button", { name: "View 1 field changes", exact: true })
    .click();
  await expect(
    history.getByRole("cell", { name: "Todo", exact: true }),
  ).toBeVisible();
  await expect(
    history.getByRole("cell", { name: "In progress", exact: true }),
  ).toBeVisible();
  expect(
    await page.evaluate(() => document.documentElement.scrollWidth),
  ).toBeLessThanOrEqual(390);
  await expectNoAccessibilityViolations(page);
  await page.screenshot({
    path: test.info().outputPath("project-history-mobile.png"),
  });
});

test("an unfinished item draft survives reload and account changes without adopting a newer item revision", async ({
  page,
}) => {
  const state = await fixture(page);
  await page.goto(`${home}/${projectId}`);
  await page
    .getByRole("button", { name: "Add work", exact: true })
    .first()
    .click();
  const editor = page.getByRole("dialog");
  await editor.getByLabel("Title", { exact: true }).fill("Original task");
  await editor
    .getByRole("button", { name: "Add to project", exact: true })
    .click();
  await expect(editor).toHaveCount(0);
  const originalId = state.items[0].item.id;
  await page
    .getByRole("button", { name: "Edit Original task", exact: true })
    .click();
  await editor
    .getByLabel("Title", { exact: true })
    .fill("Alice's unfinished draft");
  await editor
    .getByLabel("Description", { exact: true })
    .fill("Retain this text through reload.");
  state.editFirstItem("Bob's committed edit");
  await page.route("**/api/session", (route) =>
    route.fulfill({
      json: {
        authenticated: true,
        mode: "oidc",
        csrf: "bob-csrf",
        user: { issuer: "test", subject: "bob", name: "Bob" },
      },
    }),
  );
  await page.reload();
  await expect(
    page.getByRole("heading", { name: "Platform release", exact: true }),
  ).toBeVisible();
  await expect(
    page.getByRole("button", { name: "Resume item draft", exact: true }),
  ).toHaveCount(0);
  await expect(
    page.getByText("Alice's unfinished draft", { exact: true }),
  ).toHaveCount(0);
  await page.unroute("**/api/session");
  await page.reload();
  await page
    .getByRole("button", { name: "Resume item draft", exact: true })
    .click();
  await expect(editor.getByLabel("Title", { exact: true })).toHaveValue(
    "Alice's unfinished draft",
  );
  await expect(editor.getByLabel("Description", { exact: true })).toHaveValue(
    "Retain this text through reload.",
  );
  await editor.getByRole("button", { name: "Save item", exact: true }).click();
  expect(state.submissions.at(-1)?.command).toMatchObject({
    kind: "item",
    version: 1,
    item: { id: originalId },
  });
  await expect(editor.getByRole("alert")).toContainText(
    "Your draft is preserved",
  );
  await editor
    .getByRole("button", { name: "Review saved item", exact: true })
    .click();
  await expect(
    editor.getByRole("cell", { name: "Bob's committed edit", exact: true }),
  ).toBeVisible();
  await editor
    .getByRole("button", {
      name: "Keep my draft against this revision",
      exact: true,
    })
    .click();
  await editor.getByRole("button", { name: "Save item", exact: true }).click();
  await expect(editor).toHaveCount(0);
  await page.reload();
  await expect(
    page.getByRole("button", { name: "Resume item draft", exact: true }),
  ).toHaveCount(0);
});

test("unfinished project creation and planning properties restore their captured values", async ({
  page,
}) => {
  await fixture(page);
  await page.goto(home);
  await page.getByRole("button", { name: "New project", exact: true }).click();
  await page
    .getByLabel("Project name", { exact: true })
    .fill("Unsubmitted launch");
  await page.reload();
  await page
    .getByRole("button", { name: "Resume new project draft", exact: true })
    .click();
  await expect(page.getByLabel("Project name", { exact: true })).toHaveValue(
    "Unsubmitted launch",
  );
  await page
    .getByRole("dialog")
    .getByRole("button", { name: "Cancel", exact: true })
    .click();
  await page.goto(`${home}/${projectId}?view=overview`);
  await page
    .getByRole("complementary", { name: "Project details", exact: true })
    .getByRole("button", { name: "Edit", exact: true })
    .click();
  const editor = page.getByRole("dialog", {
    name: "Project properties",
    exact: true,
  });
  await pickDate(page, "Target date", "2026-11-20");
  await page.reload();
  await page
    .getByRole("button", { name: "Resume properties draft", exact: true })
    .click();
  await expect(
    editor.getByRole("button", { name: /^Target date:/ }),
  ).toContainText("Nov 20, 2026");
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

test("a lost submission survives reload and stays bound to its original account", async ({
  page,
}) => {
  const state = await fixture(page);
  await page.goto(home);
  state.loseNext();
  await page.getByRole("button", { name: "New project", exact: true }).click();
  await page
    .getByLabel("Project name", { exact: true })
    .fill("Reload recovery");
  await page
    .getByRole("button", { name: "Create project", exact: true })
    .click();
  await expect(
    page
      .getByRole("dialog")
      .getByRole("button", { name: "Retry submission", exact: true }),
  ).toBeVisible();
  const original = structuredClone(state.submissions[0]);
  await page.route("**/api/session", (route) =>
    route.fulfill({
      json: {
        authenticated: true,
        mode: "oidc",
        csrf: "bob-csrf",
        user: { issuer: "test", subject: "bob", name: "Bob" },
      },
    }),
  );
  await page.reload();
  await expect(
    page.getByRole("heading", { name: "Projects", exact: true }),
  ).toBeVisible();
  await expect(
    page.getByRole("button", { name: "Retry submission", exact: true }),
  ).toHaveCount(0);
  expect(state.submissions).toHaveLength(1);
  await page.unroute("**/api/session");
  await page.reload();
  await page
    .getByRole("button", { name: "Retry submission", exact: true })
    .click();
  await expect(
    page.getByText("Project submission", { exact: true }),
  ).toHaveCount(0);
  expect(state.submissions[1]).toEqual(original);
  await page.reload();
  await expect(
    page.getByRole("button", { name: "Retry submission", exact: true }),
  ).toHaveCount(0);
  await expect(
    page.getByRole("button", { name: "Resume new project draft", exact: true }),
  ).toHaveCount(0);
  await page.getByRole("button", { name: "New project", exact: true }).click();
  await expect(page.getByLabel("Project name", { exact: true })).toHaveValue(
    "",
  );
});

test("a failed recovery checkpoint prevents the new native request", async ({
  page,
}) => {
  const state = await fixture(page);
  await page.addInitScript(() => {
    const setItem = Storage.prototype.setItem;
    let full = true;
    window.addEventListener("qualification-storage-available", () => {
      full = false;
    });
    Storage.prototype.setItem = function (key: string, value: string) {
      if (full && key.startsWith("crab.projects.submission."))
        throw new DOMException("Storage full", "QuotaExceededError");
      setItem.call(this, key, value);
    };
  });
  await page.goto(home);
  await page.getByRole("button", { name: "New project", exact: true }).click();
  await page.getByLabel("Project name", { exact: true }).fill("Unsent project");
  await page
    .getByRole("button", { name: "Create project", exact: true })
    .click();
  await expect(page.getByRole("dialog").getByRole("alert")).toBeVisible();
  expect(state.submissions).toHaveLength(0);
  await page.evaluate(() =>
    window.dispatchEvent(new Event("qualification-storage-available")),
  );
  await page
    .getByRole("dialog")
    .getByRole("button", { name: "Retry recovery storage", exact: true })
    .click();
  await page
    .getByRole("dialog")
    .getByRole("button", { name: "Create project", exact: true })
    .click();
  await expect(
    page.getByRole("heading", { name: "Unsent project", exact: true }),
  ).toBeVisible();
  expect(state.submissions).toHaveLength(1);
});

test("draft storage failure retains the local text and prevents an uncheckpointed submission", async ({
  page,
}) => {
  const state = await fixture(page);
  await page.addInitScript(() => {
    const write = Storage.prototype.setItem;
    Storage.prototype.setItem = function (key: string, value: string) {
      if (key.startsWith("crab.projects.draft."))
        throw new DOMException("Storage unavailable", "QuotaExceededError");
      write.call(this, key, value);
    };
  });
  await page.goto(home);
  await page.getByRole("button", { name: "New project", exact: true }).click();
  const editor = page.getByRole("dialog");
  await editor
    .getByLabel("Project name", { exact: true })
    .fill("Keep this local text");
  await expect(editor.getByRole("alert")).toContainText("could not be saved");
  await editor
    .getByRole("button", { name: "Create project", exact: true })
    .click();
  await expect(editor.getByRole("alert")).toContainText(
    "No submission was sent",
  );
  await expect(editor.getByLabel("Project name", { exact: true })).toHaveValue(
    "Keep this local text",
  );
  expect(state.submissions).toHaveLength(0);
});

test("an unsupported draft stays exportable and cannot be silently overwritten", async ({
  page,
}) => {
  const state = await fixture(page);
  await page.goto(home);
  await page.getByRole("button", { name: "New project", exact: true }).click();
  await page
    .getByLabel("Project name", { exact: true })
    .fill("Retained format");
  const original = await page.evaluate(() => {
    const key = Object.keys(sessionStorage).find((key) =>
      key.startsWith("crab.projects.draft."),
    )!;
    const saved = JSON.parse(sessionStorage.getItem(key)!);
    saved.format = 999;
    const content = JSON.stringify(saved);
    sessionStorage.setItem(key, content);
    return { key, content };
  });
  await page.reload();
  await expect(
    page.getByRole("link", { name: "Export create draft", exact: true }),
  ).toBeVisible();
  await page
    .getByRole("button", { name: "Resume new project draft", exact: true })
    .click();
  const editor = page.getByRole("dialog");
  await expect(editor.getByRole("alert")).toContainText(
    "saved record has been retained",
  );
  await editor
    .getByLabel("Project name", { exact: true })
    .fill("Replacement must not erase it");
  await editor
    .getByRole("button", { name: "Create project", exact: true })
    .click();
  expect(state.submissions).toHaveLength(0);
  expect(
    await page.evaluate((key) => sessionStorage.getItem(key), original.key),
  ).toBe(original.content);
  await editor.getByRole("button", { name: "Cancel", exact: true }).click();
  await page
    .getByRole("button", { name: "Discard create draft", exact: true })
    .click();
  await expect(
    page.getByRole("button", { name: "Resume new project draft", exact: true }),
  ).toHaveCount(0);
});
