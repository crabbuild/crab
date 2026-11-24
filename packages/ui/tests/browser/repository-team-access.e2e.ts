import { expect, test } from "@playwright/test";
import { mockPullRequest } from "./pulls/fixture";
import {
  expectNoAccessibilityViolations,
  selectDarkTheme,
} from "./accessibility";
import type {
  TeamAccessState,
  TeamAccessUpdate,
  TeamListing,
} from "@/lib/contracts";

test("organization owners can search teams and reconcile a stale grant edit", async ({
  page,
}) => {
  const repository = await mockPullRequest(page);
  const id = repository.lifecycle.repositoryId;
  const organization = "01900000-0000-7000-8000-000000000010";
  const engineers = "01900000-0000-7000-8000-000000000011";
  const deleted = "01900000-0000-7000-8000-000000000012";
  const orgPath = `/api/organizations/${organization}`;
  const accessPath = "/api/repos/team/project/team-access";
  let owner = true;
  const state: TeamAccessState = {
    revision: 10,
    organization: {
      id: organization,
      teams: [{ team: deleted, access: "read" }],
    },
    team_names: {},
  };
  const writes: TeamAccessUpdate[] = [];
  await page.setViewportSize({ width: 390, height: 844 });
  await page.route(
    (url) => url.pathname === "/api/repos",
    (route) =>
      route.fulfill({
        json: {
          repositories: [
            {
              id,
              owner: "team",
              name: "project",
              description: "Team grants test",
              access: "write",
              can_admin: true,
              organization_id: organization,
              archive_version: 0,
              archived: false,
              protection_version: 0,
              protected_branches: [],
            },
          ],
        },
      }),
  );
  await page.route("**/api/session", (route) =>
    route.fulfill({
      json: {
        authenticated: true,
        mode: "oidc",
        csrf: "owner-csrf",
        user: {
          issuer: "https://issuer.test",
          subject: "alice-id",
          name: "Alice",
        },
      },
    }),
  );
  await page.route(
    (url) => url.pathname === orgPath,
    (route) => route.fulfill({ json: { role: owner ? "owner" : "member" } }),
  );
  await page.route(
    (url) => url.pathname === `${orgPath}/teams`,
    (route) => {
      const params = new URL(route.request().url()).searchParams;
      const filtered = params.get("search") === "Release";
      const listing: TeamListing = {
        can_create: true,
        items: filtered
          ? [
              {
                team: {
                  id: engineers,
                  name: "Release engineers",
                  description: "",
                  visibility: "secret",
                  version: 1,
                  created_at_ms: 1,
                  updated_at_ms: 1,
                },
                members: 1,
                role: null,
                can_manage: true,
              },
            ]
          : [],
        next: null,
        revision: 1,
      };
      return route.fulfill({ json: listing });
    },
  );
  await page.route(
    (url) => url.pathname === accessPath,
    (route) => {
      if (!owner) return route.fulfill({ status: 404, json: {} });
      const request = route.request();
      if (request.method() !== "PUT") return route.fulfill({ json: state });
      expect(request.headers()["x-csrf-token"]).toBe("owner-csrf");
      expect(request.headers()["x-crab-repository-id"]).toBe(id);
      const input: TeamAccessUpdate = request.postDataJSON();
      writes.push(input);
      if (input.expected_revision !== state.revision)
        return route.fulfill({ status: 409, json: {} });
      state.revision++;
      state.organization.teams = input.teams;
      state.team_names = { [engineers]: "Release engineers" };
      return route.fulfill({ json: state });
    },
  );

  await page.goto("/team/project/settings");
  await page.getByRole("link", { name: "Team access", exact: true }).click();
  const save = page.getByRole("button", { name: "Save team access" });
  const add = page.getByRole("button", { name: "Add team", exact: true });
  const removeDeleted = page.getByRole("button", {
    name: `Remove Unavailable team ${deleted}`,
  });
  await expect(
    page.getByText("Remove grants for unavailable teams before saving."),
  ).toBeVisible();
  await expect(save).toBeDisabled();
  await expectNoAccessibilityViolations(page);
  await selectDarkTheme(page);
  await add.click();
  await page.getByLabel("Find a team").fill("Release");
  await page.getByRole("button", { name: "Search", exact: true }).click();
  await expectNoAccessibilityViolations(page);
  await page.getByRole("button", { name: "Add Release engineers" }).click();
  await expect(add).toBeFocused();
  await expect(save).toBeDisabled();
  await removeDeleted.click();
  await page
    .getByRole("combobox", { name: "Role for Release engineers" })
    .selectOption("admin");

  state.revision++;
  await save.click();
  await expect(page.getByRole("alert")).toContainText(
    "Repository access changed",
  );
  await expect(save).toBeDisabled();
  await page.getByRole("button", { name: "Reload grants" }).click();
  await expect(removeDeleted).toBeVisible();
  await expect(
    page.getByRole("combobox", { name: "Role for Release engineers" }),
  ).toHaveCount(0);
  await removeDeleted.click();
  await save.click();
  await expect(
    page.getByText("Team access saved.", { exact: true }),
  ).toBeVisible();
  expect(writes).toEqual([
    { expected_revision: 10, teams: [{ team: engineers, access: "admin" }] },
    { expected_revision: 11, teams: [] },
  ]);

  owner = false;
  await page.reload();
  await expect(page.getByRole("alert")).toContainText(
    "Unable to load this view",
  );
  await expect(
    page.getByRole("link", { name: "Team access", exact: true }),
  ).toHaveCount(0);
  await expect(add).toHaveCount(0);
});
