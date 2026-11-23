import { expect, test } from "@playwright/test";
import {
  expectNoAccessibilityViolations,
  selectDarkTheme,
} from "./accessibility";

test("OIDC sign-in explains registration and preserves the requested destination on retry", async ({
  page,
}) => {
  await page.route("**/api/session", (route) =>
    route.fulfill({
      json: { authenticated: false, mode: "oidc", user: null, csrf: null },
    }),
  );
  await page.route("**/auth/login?*", (route) =>
    route.fulfill({ contentType: "text/html", body: "<h1>Provider</h1>" }),
  );
  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto(
    "/demo/parity/issues?state=closed&auth_error=sign_in_failed#issuecomment-7",
  );
  await expect(
    page.getByRole("heading", { name: "Sign in to Crab" }),
  ).toBeVisible();
  await expect(page.getByRole("alert")).toContainText("Try again");
  await expect(
    page.getByText(/if your provider offers registration/i),
  ).toBeVisible();
  await expect(
    page.getByRole("link", { name: "Sign up", exact: true }),
  ).toHaveCount(0);
  await selectDarkTheme(page);
  await expectNoAccessibilityViolations(page);

  const request = page.waitForRequest(
    (request) => new URL(request.url()).pathname === "/auth/login",
  );
  const signIn = page.getByRole("link", { name: "Continue to sign in" });
  await signIn.focus();
  await signIn.press("Enter");
  expect(new URL((await request).url()).searchParams.get("return_to")).toBe(
    "/demo/parity/issues?state=closed#issuecomment-7",
  );
});

test("Sign up starts the provider registration flow and retains the destination", async ({
  page,
}) => {
  await page.route("**/api/session", (route) =>
    route.fulfill({
      json: {
        authenticated: false,
        mode: "oidc",
        user: null,
        csrf: null,
        can_sign_up: true,
      },
    }),
  );
  await page.route("**/auth/login?*", (route) =>
    route.fulfill({ contentType: "text/html", body: "<h1>Registration</h1>" }),
  );
  await page.goto("/demo/parity?tab=issues");
  const request = page.waitForRequest(
    (request) => new URL(request.url()).pathname === "/auth/login",
  );
  await page.getByRole("link", { name: "Sign up", exact: true }).click();
  const query = new URL((await request).url()).searchParams;
  expect(query.get("sign_up")).toBe("true");
  expect(query.get("return_to")).toBe("/demo/parity?tab=issues");
});

test("GitHub sign-in links to GitHub account creation", async ({ page }) => {
  await page.route("**/api/session", (route) =>
    route.fulfill({
      json: { authenticated: false, mode: "github", user: null, csrf: null },
    }),
  );
  await page.goto("/");
  await expect(
    page.getByRole("link", { name: "Continue with GitHub" }),
  ).toBeVisible();
  await expect(
    page.getByRole("link", { name: "Sign up with GitHub" }),
  ).toHaveAttribute("href", "https://github.com/signup");
  await expectNoAccessibilityViolations(page);
});
