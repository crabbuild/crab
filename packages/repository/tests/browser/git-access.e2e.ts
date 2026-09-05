import { expect, test } from "@playwright/test";
import {
  expectNoAccessibilityViolations,
  selectDarkTheme,
} from "./accessibility";

test("Git tokens require a repository and keep secrets tied to that selection", async ({
  page,
}) => {
  let fail = true;
  let issued = 0;
  let revoked = 0;
  let requestedAccess = "read";
  await page.route("**/api/**", async (route) => {
    const request = route.request();
    const path = new URL(request.url()).pathname;
    if (path === "/api/session")
      return route.fulfill({
        json: {
          authenticated: true,
          mode: "oidc",
          csrf: "test-csrf",
          user: { issuer: "test", subject: "alice", name: "Alice" },
        },
      });
    if (path === "/api/repos")
      return route.fulfill({
        json: {
          repositories: ["first", "second"].map((name) => ({
            owner: "team",
            name,
            description: "",
            access: name === "first" ? "write" : "read",
            archive_version: 0,
            archived: false,
            protection_version: 0,
            protected_branches: [],
          })),
        },
      });
    expect(path).toBe("/api/git-token");
    expect(request.headers()["x-csrf-token"]).toBe("test-csrf");
    if (request.method() === "DELETE") {
      revoked++;
      return route.fulfill({ status: 204 });
    }
    expect(request.postDataJSON()).toEqual({
      owner: "team",
      repository: "first",
      access: requestedAccess,
    });
    if (fail) return route.fulfill({ status: 403 });
    issued++;
    return route.fulfill({
      json: {
        username: "crab",
        token: "fixture-token",
        expires_in: 600,
        owner: "team",
        repository: "first",
        access: requestedAccess,
      },
    });
  });
  await page.goto("/");
  await page.getByText("Git access", { exact: true }).click();
  await expectNoAccessibilityViolations(page);
  await selectDarkTheme(page);
  await page.getByText("Credential manager setup", { exact: true }).click();
  await expect(page.locator(".credential-setup code")).toHaveText(
    "git config --global 'credential.http://127.0.0.1:5175.useHttpPath' true",
  );
  const generate = page.getByRole("button", {
    name: "Generate token",
    exact: true,
  });
  await expect(generate).toBeDisabled();
  const repository = page.getByLabel("Repository", { exact: true });
  await repository.selectOption("team/first");
  await generate.click();
  await expect(page.getByRole("status")).toContainText(
    "Could not update Git access",
  );
  await expect(generate).toBeEnabled();
  fail = false;
  await generate.click();
  const token = page.getByLabel("Read token for team/first", { exact: false });
  await expect(token).toHaveValue("fixture-token");
  await expect(token).toHaveAttribute("type", "password");
  await expectNoAccessibilityViolations(page);
  requestedAccess = "write";
  await page.getByLabel("Access", { exact: true }).selectOption("write");
  await expect(token).toHaveCount(0);
  await generate.click();
  await expect(
    page.getByLabel("Write token for team/first", { exact: false }),
  ).toHaveValue("fixture-token");
  await repository.selectOption("team/second");
  await expect(page.getByLabel("Access", { exact: true })).toHaveValue("read");
  await expect(
    page.locator("#git-token-access option[value=write]"),
  ).toHaveCount(0);
  await expect(token).toHaveCount(0);
  await page
    .getByRole("button", { name: "Revoke tokens", exact: true })
    .click();
  await expect(page.getByRole("status")).toContainText("have been revoked");
  expect({ issued, revoked }).toEqual({ issued: 2, revoked: 1 });
});
