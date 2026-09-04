import { expect, test } from "@playwright/test";

test("Git tokens require a repository and keep secrets tied to that selection", async ({
  page,
}) => {
  let fail = true;
  let issued = 0;
  let revoked = 0;
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
      access: "read",
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
        access: "read",
      },
    });
  });
  await page.goto("/");
  await page.getByText("Git access", { exact: true }).click();
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
  await repository.selectOption("team/second");
  await expect(token).toHaveCount(0);
  await page
    .getByRole("button", { name: "Revoke tokens", exact: true })
    .click();
  await expect(page.getByRole("status")).toContainText("have been revoked");
  expect({ issued, revoked }).toEqual({ issued: 1, revoked: 1 });
});
