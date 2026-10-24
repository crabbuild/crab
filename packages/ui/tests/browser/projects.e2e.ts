import { test, expect, type Page } from "@playwright/test";
import {
  expectNoAccessibilityViolations,
  selectDarkTheme,
} from "./accessibility";
const repo = {
  id: "00000000-0000-4000-8000-000000000099",
  owner: "team",
  name: "project",
  description: "",
  access: "write",
  archived: false,
  can_admin: true,
  archive_version: 0,
  protection_version: 0,
};
const views = [
  {
    id: "00000000-0000-4000-8000-000000000001",
    name: "Table",
    layout: "table",
    query: "",
    status: null,
    hide_closed: false,
  },
  {
    id: "00000000-0000-4000-8000-000000000002",
    name: "Board",
    layout: "board",
    query: "",
    status: null,
    hide_closed: false,
  },
  {
    id: "00000000-0000-4000-8000-000000000003",
    name: "Roadmap",
    layout: "roadmap",
    query: "",
    status: null,
    hide_closed: false,
  },
];
const definition = {
  name: "Team planning",
  description: "Plan work together",
  readme: "",
  statuses: ["Todo", "In progress", "Done"],
  fields: [],
  views,
  is_template: false,
};
async function setup(page: Page) {
  await page.clock.setFixedTime(new Date("2026-10-23T12:00:00Z"));
  const projects = new Map<
    string,
    {
      id: string;
      definition: typeof definition;
      version: number;
      archived: boolean;
      created_at: number;
      updated_at: number;
    }
  >();
  type Entry = {
    id: string;
    subject: { kind: "issue" | "pull"; number: number } | null;
    title: string;
    body: string;
    status: string;
    priority: number;
    start: string | null;
    end: string | null;
    values: Record<string, string>;
  };
  const items = new Map<string, Entry[]>();
  const decisions = new Map<string, object>();
  const submissions: { request_id: string; command: object }[] = [];
  let lose = false;
  let reject = false;
  let writable = true;
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
      return route.fulfill({ json: { repositories: [repo] } });
    const root = "/api/repos/team/project/projects";
    if (path === `${root}/templates`)
      return route.fulfill({
        json: {
          items: [
            {
              id: "team-planning",
              description: "Plan work together",
              definition,
            },
          ],
        },
      });
    if (path === `${root}/candidates`)
      return route.fulfill({
        json: {
          items: [
            {
              subject: { kind: "issue", number: 1 },
              title: "Ship project management",
              state: "open",
            },
            {
              subject: { kind: "pull", number: 2 },
              title: "Implement workspace",
              state: "open",
            },
          ],
        },
      });
    if (path === root)
      return route.fulfill({
        json: {
          items: [...projects.values()],
          next: null,
          can_manage: writable,
        },
      });
    if (path === `${root}/commands` || path === `${root}/cancel`) {
      const body = request.postDataJSON() as {
        request_id: string;
        command: {
          kind: string;
          id: string;
          project: string;
          name: string;
          version: number;
          item: Entry;
          definition: typeof definition;
          archived: boolean;
        };
      };
      submissions.push(body);
      expect(request.headers()["x-csrf-token"]).toBe("test-csrf");
      expect(request.headers()["x-crab-repository-id"]).toBe(repo.id);
      if (reject) {
        reject = false;
        return route.fulfill({
          status: 413,
          headers: { "X-Crab-Write-Outcome": "rejected" },
          json: { error: { message: "Project input is too large" } },
        });
      }
      const saved = decisions.get(body.request_id);
      if (saved) return route.fulfill({ json: saved });
      const command = body.command;
      let id = command.project;
      if (command.kind === "create") {
        id = command.id;
        projects.set(id, {
          id,
          definition: { ...definition, name: command.name },
          version: 1,
          archived: false,
          created_at: 1700000000000,
          updated_at: 1700000000000,
        });
        items.set(id, []);
      } else {
        const project = projects.get(id)!;
        expect(project.version).toBe(command.version);
        project.version++;
        if (command.kind === "edit") {
          project.definition = command.definition;
          project.archived = command.archived;
        }
        if (command.kind === "item") {
          const current = items.get(id)!;
          const index = current.findIndex(
            (item) => item.id === command.item.id,
          );
          if (index < 0) current.push(command.item);
          else current[index] = command.item;
        }
      }
      const result = {
        outcome: "applied",
        project: id,
        version: projects.get(id)!.version,
      };
      decisions.set(body.request_id, result);
      if (lose) {
        lose = false;
        return route.abort("failed");
      }
      return route.fulfill({ json: result });
    }
    const match = new RegExp(`${root}/([^/]+)(/items)?$`).exec(path);
    if (match) {
      const project = projects.get(match[1])!;
      if (!match[2])
        return route.fulfill({ json: { project, can_manage: writable } });
      const entries = items.get(project.id)!;
      const counts: Record<string, number> = {};
      for (const item of entries)
        counts[item.status] = (counts[item.status] ?? 0) + 1;
      return route.fulfill({
        json: {
          can_manage: writable,
          page: {
            project,
            revision: project.version,
            total: entries.length,
            counts,
            next: null,
            items: entries.map((item) => ({
              item,
              work: item.subject
                ? {
                    subject: item.subject,
                    title:
                      item.subject.kind === "issue"
                        ? "Ship project management"
                        : "Implement workspace",
                    state: "open",
                  }
                : null,
              assignees: [],
              parent: null,
              progress:
                item.subject?.kind === "issue" ? { total: 2, closed: 1 } : null,
              links:
                item.subject?.kind === "issue"
                  ? [
                      {
                        subject: { kind: "pull", number: 2 },
                        title: "Implement workspace",
                        state: "open",
                      },
                    ]
                  : [],
              more_links: false,
            })),
          },
        },
      });
    }
    return route.fulfill({
      status: 404,
      json: { error: { message: `Unexpected API ${path}` } },
    });
  });
  return {
    submissions,
    rejectNext: () => {
      reject = true;
    },
    loseNext: () => {
      lose = true;
    },
    readOnly: () => {
      writable = false;
    },
  };
}
async function create(page: Page) {
  await page.goto("/team/project/projects");
  await page.getByRole("button", { name: "New project", exact: true }).click();
  await page
    .getByLabel("Project name", { exact: true })
    .fill("Release planning");
  await page
    .getByRole("button", { name: "Create project", exact: true })
    .click();
  await expect(
    page.getByRole("heading", { name: "Release planning", exact: true }),
  ).toBeVisible();
}
test("templates, linked issue and PR items, board moves, roadmap dates, saved settings and responsive views", async ({
  page,
}) => {
  await setup(page);
  await create(page);
  await page.getByRole("button", { name: "Add item", exact: true }).click();
  await page.getByLabel("Add", { exact: true }).selectOption("existing");
  await page
    .getByRole("button", { name: "Ship project management #1" })
    .click();
  await page
    .getByRole("button", { name: "Add to project", exact: true })
    .click();
  await expect(
    page.getByRole("link", { name: "Ship project management", exact: true }),
  ).toBeVisible();
  await expect(page.getByText("1/2", { exact: true })).toBeVisible();
  await page
    .getByLabel("Status for Ship project management")
    .selectOption("In progress");
  await expect(
    page.getByLabel("Status for Ship project management"),
  ).toHaveValue("In progress");
  await page.getByRole("tab", { name: "Board", exact: true }).click();
  await expect(
    page
      .locator(".project-board-column")
      .filter({
        has: page.getByRole("heading", { name: "In progress", exact: true }),
      })
      .getByRole("link", { name: "Ship project management", exact: true }),
  ).toBeVisible();
  await page
    .getByRole("button", { name: "Edit Ship project management", exact: true })
    .click();
  await page.getByLabel("Start date", { exact: true }).fill("2026-10-01");
  await page.getByLabel("Target date", { exact: true }).fill("2026-10-31");
  await page.getByRole("button", { name: "Save item", exact: true }).click();
  await page.getByRole("tab", { name: "Roadmap", exact: true }).click();
  await expect(
    page.getByRole("table", { name: "Project roadmap" }),
  ).toBeVisible();
  await expect(
    page.getByRole("button", { name: /Schedule Ship project management/ }),
  ).toBeVisible();
  await page.getByRole("button", { name: "Settings", exact: true }).click();
  await page.getByLabel("Make available as a project template").check();
  await page
    .getByRole("button", { name: "Save project settings", exact: true })
    .click();
  await page.reload();
  await expect(
    page.getByRole("heading", { name: "Release planning", exact: true }),
  ).toBeVisible();
  await page.setViewportSize({ width: 390, height: 844 });
  await selectDarkTheme(page);
  await expectNoAccessibilityViolations(page);
  expect(
    await page.evaluate(() => document.documentElement.scrollWidth),
  ).toBeLessThanOrEqual(390);
});
test("an uncertain command retains its UUID and payload across navigation and recovers without duplicate projects", async ({
  page,
}) => {
  const fixture = await setup(page);
  fixture.loseNext();
  await page.goto("/team/project/projects");
  await page.getByRole("button", { name: "New project", exact: true }).click();
  await page.getByLabel("Project name", { exact: true }).fill("Recover me");
  await page
    .getByRole("button", { name: "Create project", exact: true })
    .click();
  await expect(page.getByRole("dialog").getByRole("alert")).toBeVisible();
  await page.evaluate(() => {
    history.pushState(null, "", "/team/project/issues");
    window.dispatchEvent(new PopStateEvent("popstate"));
  });
  // The open form is allowed to unmount; the account/repository-bound intent lives above routing.
  await page
    .getByRole("button", { name: "Retry submission", exact: true })
    .click();
  await expect(
    page.getByText("Project submission", { exact: true }),
  ).toHaveCount(0);
  expect(fixture.submissions).toHaveLength(2);
  expect(fixture.submissions[1]).toEqual(fixture.submissions[0]);
  await page.goto("/team/project/projects");
  await expect(
    page.getByRole("link", { name: "Recover me", exact: true }),
  ).toHaveCount(1);
});

test("a failed creation can be recovered from inside its modal", async ({
  page,
}) => {
  const fixture = await setup(page);
  fixture.loseNext();
  await page.goto("/team/project/projects");
  await page.getByRole("button", { name: "New project", exact: true }).click();
  await page
    .getByLabel("Project name", { exact: true })
    .fill("Recovered from dialog");
  await page
    .getByRole("button", { name: "Create project", exact: true })
    .click();
  const dialog = page.getByRole("dialog");
  await expect(dialog.getByRole("alert")).toBeVisible();
  await dialog
    .getByRole("button", { name: "Retry submission", exact: true })
    .click();
  await expect(
    page.getByRole("heading", { name: "Recovered from dialog", exact: true }),
  ).toBeVisible();
  expect(fixture.submissions[1]).toEqual(fixture.submissions[0]);
});
test("editing a draft keeps it a draft and read-only users have no project writes", async ({
  page,
}) => {
  const fixture = await setup(page);
  await create(page);
  await page.getByRole("button", { name: "Add item", exact: true }).click();
  await page.getByLabel("Add", { exact: true }).selectOption("draft");
  await page.getByLabel("Title", { exact: true }).fill("Explore an idea");
  await page
    .getByRole("button", { name: "Add to project", exact: true })
    .click();
  await page
    .getByRole("button", { name: "Edit Explore an idea", exact: true })
    .click();
  await page.getByLabel("Description", { exact: true }).fill("Notes to keep");
  await page.getByRole("button", { name: "Save item", exact: true }).click();
  await expect(page.getByText("Draft note", { exact: true })).toBeVisible();
  expect(fixture.submissions.at(-1)?.command).toMatchObject({
    kind: "item",
    item: { subject: null, body: "Notes to keep" },
  });
  fixture.readOnly();
  await page.reload();
  await expect(
    page.getByText("Explore an idea", { exact: true }),
  ).toBeVisible();
  await expect(
    page.getByRole("button", { name: "Add item", exact: true }),
  ).toHaveCount(0);
  await expect(
    page.getByRole("button", { name: "Settings", exact: true }),
  ).toHaveCount(0);
  await expect(
    page.getByRole("combobox", { name: "Status for Explore an idea" }),
  ).toHaveCount(0);
});

test("a proven input rejection releases the form so it can be corrected", async ({
  page,
}) => {
  const fixture = await setup(page);
  fixture.rejectNext();
  await page.goto("/team/project/projects");
  await page.getByRole("button", { name: "New project", exact: true }).click();
  await page.getByLabel("Project name", { exact: true }).fill("Rejected input");
  await page
    .getByRole("button", { name: "Create project", exact: true })
    .click();
  await expect(page.getByRole("dialog").getByRole("alert")).toHaveText(
    "Project input is too large",
  );
  await expect(
    page
      .getByRole("dialog")
      .getByRole("button", { name: "Retry submission", exact: true }),
  ).toHaveCount(0);
  await page
    .getByLabel("Project name", { exact: true })
    .fill("Corrected input");
  await page
    .getByRole("button", { name: "Create project", exact: true })
    .click();
  await expect(
    page.getByRole("heading", { name: "Corrected input", exact: true }),
  ).toBeVisible();
  expect(fixture.submissions[1].request_id).not.toBe(
    fixture.submissions[0].request_id,
  );
});
