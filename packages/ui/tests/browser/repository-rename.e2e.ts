import { expect, test } from "@playwright/test";
import { expectNoAccessibilityViolations } from "./accessibility";

test("a lost rename reply keeps its submission and reopens the same repository", async ({
  page,
}) => {
  const id = "01900000-0000-7000-8000-000000000021";
  let name = "before";
  let version = 0;
  let submission: unknown;
  await page.route("**/api/**", (route) => {
    const { pathname, searchParams } = new URL(route.request().url());
    if (pathname === "/api/session")
      return route.fulfill({
        json: {
          authenticated: true,
          mode: "oidc",
          csrf: "fixture-csrf",
          user: {
            issuer: "https://example.test",
            subject: "alice",
            name: "Alice",
          },
        },
      });
    if (pathname === "/api/notifications/unread")
      return route.fulfill({ json: { unread: false } });
    if (pathname === "/api/repos") {
      const requestedName = searchParams.get("name");
      if (requestedName && requestedName !== name)
        return route.fulfill({
          json: { repositories: [], redirect: { id, owner: "team", name } },
        });
      return route.fulfill({
        json: {
          repositories: [
            {
              id,
              owner: "team",
              name,
              description: "",
              can_admin: true,
              access: "write",
              archived: false,
              archive_version: 0,
              protection_version: 0,
              protected_branches: [],
            },
          ],
        },
      });
    }
    if (pathname === `/api/repositories/${id}/name`) {
      if (route.request().method() === "PUT") {
        expect(route.request().headers()["x-csrf-token"]).toBe("fixture-csrf");
        expect(route.request().headers()["x-crab-repository-id"]).toBe(id);
        if (!submission) {
          submission = route.request().postDataJSON();
          expect(submission).toMatchObject({
            name: "after",
            version: 0,
            request_id: expect.any(String),
          });
          name = "after";
          version = 1;
          return route.abort("failed");
        }
        expect(route.request().postDataJSON()).toEqual(submission);
      }
      return route.fulfill({ json: { id, owner: "team", name, version } });
    }
    if (pathname.endsWith("/refs"))
      return route.fulfill({
        json: { head: null, default_branch: "main", refs: [] },
      });
    if (pathname.endsWith("/settings/merge-methods"))
      return route.fulfill({
        json: { version: 0, enabled: ["merge_commit", "squash", "rebase"] },
      });
    return route.fulfill({
      status: 404,
      json: { error: { message: "Fixture route unavailable" } },
    });
  });

  await page.goto("/team/before/settings");
  await page.getByRole("button", { name: "Rename repository" }).click();
  await expectNoAccessibilityViolations(page);
  const input = page.getByRole("textbox", { name: "New repository name" });
  await expect(input).toHaveValue("before");
  await input.fill("bad/name");
  await expect(page.getByRole("button", { name: "Save name" })).toBeDisabled();
  await input.fill("after");
  await page.getByRole("button", { name: "Save name" }).click();
  await expect(page.getByRole("dialog")).toContainText(
    "The response was lost.",
  );
  await page.getByRole("button", { name: "Save name" }).click();
  await expect(page).toHaveURL(/\/team\/after\/settings$/);
  await expect(
    page.getByRole("heading", { name: "Repository name" }),
  ).toBeVisible();

  await page.goto(
    `/team/before/settings?repository_id=${id}#repository-name-heading`,
  );
  await expect(page).toHaveURL(
    new RegExp(
      `/team/after/settings\\?repository_id=${id}#repository-name-heading$`,
    ),
  );
});
