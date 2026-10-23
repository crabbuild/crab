import { test, expect } from "@playwright/test";
import { mockPullRequest } from "./pulls/fixture";
import {
  expectNoAccessibilityViolations,
  selectDarkTheme,
} from "./accessibility";
import type { RepositoryGroup } from "@/lib/api";

test("SSO group access edits are revisioned, persist after reload and preserve group administrator access", async ({
  page,
}) => {
  const repository = await mockPullRequest(page);
  await page.setViewportSize({ width: 390, height: 844 });
  let revision = 1;
  let groups: RepositoryGroup[] = [{ group: "team-admins", access: "admin" }];
  let members = [
    { subject: "alice-id", name: "Alice", access: "admin" },
    { subject: "recovery-id", name: "Recovery", access: "admin" },
  ];
  const writes: unknown[] = [];
  await page.route(
    (url) => url.pathname === "/api/repos",
    (route) =>
      route.fulfill({
        json: {
          repositories: [
            {
              id: repository.lifecycle.repositoryId,
              owner: "team",
              name: "project",
              description: "ACL test",
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
  await page.route("**/api/session", (route) =>
    route.fulfill({
      json: {
        authenticated: true,
        mode: "oidc",
        csrf: "test-csrf",
        user: {
          issuer: "https://issuer.test",
          subject: "alice-id",
          name: "Alice",
        },
      },
    }),
  );
  await page.route(
    (url) => url.pathname === "/api/repos/team/project/members",
    (route) => {
      if (route.request().method() === "PUT") {
        const input = route.request().postDataJSON();
        writes.push(input);
        expect(input.expected_revision).toBe(revision);
        members = input.members;
        groups = input.groups;
        revision++;
      }
      return route.fulfill({
        json: {
          revision,
          members,
          groups,
          can_admin: true,
          groups_supported: true,
        },
      });
    },
  );
  await page.goto("/team/project/settings/access");
  await selectDarkTheme(page);
  await page.getByRole("button", { name: "Add SSO group" }).click();
  await page
    .getByLabel("Group identifier 2", { exact: true })
    .fill("team-developers");
  await page
    .getByRole("combobox", { name: "Role", exact: true })
    .nth(1)
    .selectOption("write");
  await expectNoAccessibilityViolations(page);
  await page.getByRole("button", { name: "Save access" }).click();
  await expect(page.getByRole("status")).toContainText("Access updated.");
  expect(groups).toEqual([
    { group: "team-admins", access: "admin" },
    { group: "team-developers", access: "write" },
  ]);
  await page.reload();
  await expect(
    page.getByLabel("Group identifier 2", { exact: true }),
  ).toHaveValue("team-developers");
  await page.getByRole("button", { name: "Edit Alice", exact: true }).click();
  await page.getByRole("button", { name: "Access", exact: true }).click();
  await page.getByRole("menuitemradio", { name: /^Read/ }).click();
  await page.getByRole("button", { name: "Save member", exact: true }).click();
  await page.getByRole("button", { name: "Save access" }).click();
  await expect(page.getByRole("status")).toContainText("Access updated.");
  await expect(page).toHaveURL(/\/settings\/access$/);
  expect(writes).toHaveLength(2);
  await page
    .getByRole("button", { name: "Remove SSO group 2", exact: true })
    .click();
  await page.getByRole("button", { name: "Save access" }).click();
  await expect(page.getByRole("status")).toContainText("Access updated.");
  await page.reload();
  await expect(
    page.getByRole("textbox", { name: /Group identifier/ }),
  ).toHaveCount(1);
});
