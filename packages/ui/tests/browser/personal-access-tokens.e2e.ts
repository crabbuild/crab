import { expect, test } from "@playwright/test";
import {
  expectNoAccessibilityViolations,
  selectDarkTheme,
} from "./accessibility";

test("Personal access tokens show their secret once and revoke from the list", async ({
  page,
}) => {
  const id = "0190f3c1-0000-7000-8000-000000000001";
  const tokens: object[] = [];
  let created: unknown;
  let revoked = 0;
  await page.route("**/api/**", async (route) => {
    const request = route.request();
    const path = new URL(request.url()).pathname;
    if (path === "/api/notifications/unread")
      return route.fulfill({ json: { unread: false } });
    if (path === "/api/profile")
      return route.fulfill({ json: { profile: null } });
    if (path === "/api/session")
      return route.fulfill({
        json: {
          authenticated: true,
          mode: "oidc",
          csrf: "test-csrf",
          user: { issuer: "test", subject: "alice", name: "Alice" },
        },
      });
    if (request.method() === "GET") {
      expect(path).toBe("/api/personal-access-tokens");
      return route.fulfill({ json: tokens });
    }
    expect(request.headers()["x-csrf-token"]).toBe("test-csrf");
    if (request.method() === "POST") {
      expect(path).toBe("/api/personal-access-tokens");
      created = request.postDataJSON();
      const summary = {
        id,
        name: "GitHub CLI",
        scopes: ["repo", "read:org", "gist"],
        created_at: 1795000000,
        expires_at: 1797592000,
        expired: false,
      };
      tokens.push(summary);
      return route.fulfill({
        status: 201,
        json: { ...summary, token: "crab_pat_fixture" },
      });
    }
    expect(request.method()).toBe("DELETE");
    expect(path).toBe(`/api/personal-access-tokens/${id}`);
    revoked++;
    tokens.length = 0;
    return route.fulfill({ status: 204 });
  });

  await page.goto("/-/settings/tokens");
  await expect(
    page.getByRole("heading", { name: "Personal access tokens", exact: true }),
  ).toBeVisible();
  await expect(
    page.getByText("You have no personal access tokens.", { exact: true }),
  ).toBeVisible();
  await expectNoAccessibilityViolations(page);

  await page
    .getByRole("button", { name: "Generate token", exact: true })
    .click();
  const token = page.getByLabel("Personal access token", { exact: true });
  await expect(token).toHaveValue("crab_pat_fixture");
  await expect(token).toHaveAttribute("type", "password");
  expect(created).toEqual({
    name: "GitHub CLI",
    scopes: ["repo", "read:org", "gist"],
    expires_in_days: 30,
  });
  const host = new URL(page.url()).host;
  await expect(
    page.getByText(`gh auth setup-git --hostname ${host}`, { exact: false }),
  ).toBeVisible();
  await expect(
    page.getByText("repo, read:org, gist", { exact: false }),
  ).toBeVisible();
  await selectDarkTheme(page);
  await expectNoAccessibilityViolations(page);

  await page.getByRole("button", { name: "Revoke", exact: true }).click();
  await expect(page.getByRole("status")).toHaveText("Revoked “GitHub CLI”.");
  await expect(token).toHaveCount(0);
  await expect(
    page.getByText("You have no personal access tokens.", { exact: true }),
  ).toBeVisible();
  expect(revoked).toBe(1);
});
