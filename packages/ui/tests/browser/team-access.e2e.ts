import { expect, test } from "@playwright/test";
import { expectNoAccessibilityViolations } from "./accessibility";
import { mockPullRequest } from "./pulls/fixture";
import type { RepositoryAccess } from "@/lib/contracts";

test("repository team grants share the access revision with individual grants", async ({
  page,
}) => {
  const fixture = await mockPullRequest(page);
  const repository = fixture.lifecycle.repositoryId;
  const organization = "01900000-0000-7000-8000-000000000020";
  const team = "01900000-0000-7000-8000-000000000021";
  const root = "/api/repos/team/project";
  const endpoint = `${root}/settings/team-access`;
  let revision = 3;
  let access: RepositoryAccess | undefined;
  let savedMembers = false;
  await page.route(
    (url) => url.pathname === "/api/repos",
    (route) =>
      route.fulfill({
        json: {
          repositories: [
            {
              id: repository,
              organization,
              owner: "team",
              name: "project",
              description: "",
              access: "write",
              can_admin: true,
              archive_version: 0,
              archived: false,
              protection_version: 0,
              protected_branches: [],
            },
          ],
        },
      }),
  );
  await page.route(
    (url) => url.pathname === `${root}/members`,
    (route) => {
      if (route.request().method() === "PUT") {
        expect(route.request().postDataJSON().expected_revision).toBe(revision);
        savedMembers = true;
        revision++;
      }
      return route.fulfill({
        json: {
          revision,
          can_admin: true,
          groups_supported: false,
          groups: [],
          members: [{ subject: "alice", name: "Alice", access: "admin" }],
        },
      });
    },
  );
  await page.route(
    (url) => url.pathname === `/api/organizations/${organization}/teams`,
    (route) => {
      const search = new URL(route.request().url()).searchParams.get("search");
      return route.fulfill({
        json: {
          revision: 1,
          next: null,
          can_create: true,
          items: "engineering".includes(search?.toLowerCase() ?? "")
            ? [
                {
                  team: {
                    id: team,
                    name: "Engineering",
                    visibility: "secret",
                    version: 1,
                  },
                  members: 2,
                  role: "maintainer",
                  can_manage: true,
                },
              ]
            : [],
        },
      });
    },
  );
  await page.route(
    (url) => url.pathname === endpoint,
    (route) =>
      route.fulfill({
        json: {
          revision,
          grants: access ? [{ team, name: "Engineering", access }] : [],
        },
      }),
  );
  await page.route(
    (url) => url.pathname === `${endpoint}/${team}`,
    (route) => {
      const request = route.request();
      expect(request.headers()["x-crab-repository-id"]).toBe(repository);
      const body = request.postDataJSON();
      if (body.expected_revision !== revision)
        return route.fulfill({
          status: 409,
          json: { error: { message: "Repository access changed." } },
        });
      if (request.method() === "DELETE") access = undefined;
      else access = body.access;
      revision++;
      return route.fulfill({ json: { revision } });
    },
  );

  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto("/team/project/settings/access");
  const panel = page.getByRole("region", { name: "Team access", exact: true });
  await panel.getByRole("button", { name: "Add team", exact: true }).click();
  await panel.getByRole("searchbox", { name: "Find a team" }).fill("Eng");
  await panel.getByRole("button", { name: "Search teams" }).click();
  await panel.getByRole("button", { name: "Choose Engineering" }).click();
  await panel.getByLabel("Repository role").selectOption("write");
  await panel.getByRole("button", { name: "Save team role" }).click();
  const engineering = panel
    .getByRole("listitem")
    .filter({ hasText: "Engineering" });
  await expect(engineering).toContainText("Write");

  await page.getByRole("button", { name: "Save access", exact: true }).click();
  await expect.poll(() => savedMembers).toBe(true);
  await page.reload();
  await expect(engineering).toContainText("Write");
  await expectNoAccessibilityViolations(page);

  await panel
    .getByRole("button", { name: "Change access for Engineering" })
    .click();
  await panel.getByLabel("Repository role").selectOption("admin");
  revision++;
  await panel.getByRole("button", { name: "Save team role" }).click();
  await expect(panel.getByRole("alert")).toContainText(
    "Repository access changed.",
  );
  await panel.getByRole("button", { name: "Reload team access" }).click();
  await expect(engineering).toContainText("Write");

  await panel
    .getByRole("button", { name: "Remove access for Engineering" })
    .click();
  await panel
    .getByRole("button", { name: "Remove team access", exact: true })
    .click();
  await expect(panel.getByText("No team grants are visible.")).toBeVisible();
});
