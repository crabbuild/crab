import { expect, test } from "@playwright/test";
import { expectNoAccessibilityViolations } from "./accessibility";

test("email settings explain verification and recover from a stale preference", async ({
  page,
}) => {
  await page.route("**/api/session", (route) =>
    route.fulfill({
      json: {
        authenticated: true,
        mode: "oidc",
        csrf: "fixture-csrf",
        user: {
          issuer: "https://id.example.test",
          subject: "bob",
          name: "Bob",
        },
      },
    }),
  );
  await page.route("**/api/repos", (route) =>
    route.fulfill({ json: { repositories: [] } }),
  );
  await page.route("**/api/notifications/unread", (route) =>
    route.fulfill({ json: { unread: false } }),
  );
  await page.route(
    (url) => url.pathname === "/api/notifications",
    (route) => route.fulfill({ json: { items: [], next: null } }),
  );

  let preference = {
    configured: true,
    address: null as string | null,
    enabled: false,
    version: 0,
  };
  await page.route("**/api/notifications/email", async (route) => {
    if (route.request().method() === "PUT") {
      expect(route.request().headers()["x-csrf-token"]).toBe("fixture-csrf");
      const { enabled, version } = route.request().postDataJSON();
      if (preference.version !== version) {
        await route.fulfill({
          status: 409,
          json: { error: "This list changed; reload from the first page" },
        });
        return;
      }
      preference = { ...preference, enabled, version: version + 1 };
    }
    await route.fulfill({ json: preference });
  });

  await page.goto("/-/notifications");
  const settings = page.getByRole("button", { name: "Email settings" });
  await settings.click();
  const dialog = page.getByRole("dialog", { name: "Email notifications" });
  await expect(
    dialog.getByRole("link", { name: "Sign in again" }),
  ).toHaveAttribute("href", "/auth/login?return_to=%2F-%2Fnotifications");
  await page.keyboard.press("Escape");

  preference = { ...preference, address: "bob@example.test", version: 1 };
  await settings.click();
  await expect(dialog.getByText("bob@example.test")).toBeVisible();
  const enabled = dialog.getByRole("checkbox", {
    name: "Send me notification emails",
  });
  await expect(enabled).not.toBeChecked();
  await expectNoAccessibilityViolations(page);

  preference = { ...preference, address: "new@example.test", version: 2 };
  await enabled.check();
  await dialog.getByRole("button", { name: "Save preference" }).click();
  await dialog.getByRole("button", { name: "Reload preference" }).click();
  await expect(dialog.getByText("new@example.test")).toBeVisible();
  await expect(enabled).not.toBeChecked();

  await enabled.check();
  await dialog.getByRole("button", { name: "Save preference" }).click();
  await expect(dialog.getByRole("status")).toHaveText("Email notifications on");
  await page.reload();
  await settings.click();
  await expect(enabled).toBeChecked();

  await enabled.uncheck();
  await dialog.getByRole("button", { name: "Save preference" }).click();
  await expect(dialog.getByRole("status")).toHaveText(
    "Email notifications off",
  );
});
