import { expect, test } from "@playwright/test";
import { expectNoAccessibilityViolations } from "./accessibility";
import { mockPullRequest } from "./pulls/fixture";

test("owners target repositories and repository admins see inherited rules as read-only", async ({
  page,
}) => {
  const { lifecycle } = await mockPullRequest(page);
  const id = "00000000-0000-4000-8000-000000000050";
  const repo = {
    id: lifecycle.repositoryId,
    organization: id,
    owner: "team",
    name: "project",
    description: "",
    can_admin: true,
    access: "write",
    archived: false,
    archive_version: 0,
    protection_version: 0,
    protected_branches: [],
  };
  let rulesets: {
    ruleset: { id: string; name: string };
    repositories: unknown;
  }[] = [];
  let version = 0;
  await page.route("**/api/session", (route) =>
    route.fulfill({
      json: {
        authenticated: true,
        mode: "local",
        user: null,
        csrf: "fixture-csrf",
      },
    }),
  );
  await page.route(
    (url) => url.pathname === "/api/repos",
    (route) => route.fulfill({ json: { repositories: [repo] } }),
  );
  await page.route(
    (url) => url.pathname === `/api/organizations/${id}`,
    (route) =>
      route.fulfill({
        json: {
          organization: {
            id,
            name: "Engineering",
            description: "Repository policy",
            version: 1,
          },
          role: "owner",
          actor: "owner",
        },
      }),
  );
  await page.route(
    (url) => url.pathname === `/api/organizations/${id}/teams`,
    (route) =>
      route.fulfill({
        json: { items: [], next: null, revision: 0, can_create: true },
      }),
  );
  await page.route(
    (url) => url.pathname === `/api/organizations/${id}/rulesets`,
    (route) => {
      if (route.request().method() === "PUT") {
        expect(route.request().headers()["x-csrf-token"]).toBe("fixture-csrf");
        const input = route.request().postDataJSON();
        expect(input.expected_version).toBe(version);
        rulesets = input.rulesets;
        version++;
      }
      return route.fulfill({ json: { version, rulesets } });
    },
  );
  await page.route(
    (url) => url.pathname === "/api/repos/team/project/rulesets",
    (route) =>
      route.fulfill({
        json: {
          version: 0,
          rulesets: [],
          can_manage: true,
          inherited: {
            organization: id,
            rulesets: rulesets.map(({ ruleset }) => ruleset),
          },
        },
      }),
  );

  await page.goto(`/-/organizations/${id}?tab=rulesets`);
  await page.getByRole("button", { name: "New ruleset" }).click();
  await page.getByLabel("Name", { exact: true }).fill("Release policy");
  await page.getByLabel("Repository targeting").selectOption("selected");
  await expect(
    page.getByRole("button", { name: "Save ruleset" }),
  ).toBeDisabled();
  await page.getByRole("checkbox", { name: "project", exact: true }).check();
  await page.getByLabel("Required approving reviews").fill("1");
  await page.getByLabel("Required check names").fill("ci/organization");
  await page
    .getByLabel("Require branches to be up to date before merging")
    .check();
  await page.getByLabel("Require merge queue", { exact: true }).check();
  await expectNoAccessibilityViolations(page);
  await page.getByRole("button", { name: "Save ruleset" }).click();
  await expect(
    page.getByRole("heading", { name: "Release policy" }),
  ).toBeVisible();
  expect(rulesets[0].repositories).toEqual({
    kind: "selected",
    ids: [repo.id],
  });
  expect(rulesets[0].ruleset).toMatchObject({
    require_up_to_date: true,
    require_merge_queue: true,
    required_checks: ["ci/organization"],
  });

  await page.getByRole("button", { name: "Edit Release policy" }).click();
  await page.getByLabel("Repository targeting").selectOption("name");
  await page.getByLabel("Include repository patterns").fill("project*");
  await page.getByLabel("Exclude repository patterns").fill("project-sandbox");
  await page.getByRole("button", { name: "Save ruleset" }).click();
  await expect(
    page.getByText("Repository names matching", { exact: false }),
  ).toBeVisible();
  expect(rulesets[0].repositories).toEqual({
    kind: "name",
    include: ["project*"],
    exclude: ["project-sandbox"],
  });

  await page.goto("/team/project/settings/rulesets");
  const inherited = page.getByRole("region", {
    name: "Inherited organization rulesets",
  });
  await expect(
    inherited.getByRole("heading", { name: "Release policy" }),
  ).toBeVisible();
  await expect(
    inherited.getByText("Require merge queue", { exact: true }),
  ).toBeVisible();
  await expect(
    inherited.getByRole("button", { name: "Edit Release policy" }),
  ).toHaveCount(0);
  await expect(page.getByRole("button", { name: "New ruleset" })).toBeVisible();
});
