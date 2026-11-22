import { expect, test } from "@playwright/test";
import { expectNoAccessibilityViolations } from "./accessibility";
import { mockPullRequest } from "./pulls/fixture";

test("repository watch preferences recover a lost reply and refresh the unread badge", async ({
  page,
}) => {
  const { lifecycle } = await mockPullRequest(page);
  const csrf = "fixture-csrf";
  let preference = { mode: "direct", version: 0 };
  let loseReply = true;
  await page.route("**/api/session", (route) =>
    route.fulfill({
      json: {
        authenticated: true,
        mode: "oidc",
        user: { name: "Bob", subject: "bob", issuer: "https://issuer.test" },
        csrf,
      },
    }),
  );
  await page.route("**/api/notifications/unread", (route) =>
    route.fulfill({ json: { unread: preference.mode !== "muted" } }),
  );
  await page.route(
    (url) => url.pathname === "/api/repos/team/project/subscription",
    (route) => {
      const request = route.request();
      if (request.method() === "GET")
        return route.fulfill({ json: preference });
      expect(request.method()).toBe("PUT");
      expect(request.headers()["x-csrf-token"]).toBe(csrf);
      expect(request.headers()["x-crab-repository-id"]).toBe(
        lifecycle.repositoryId,
      );
      expect(request.postDataJSON()).toEqual({ mode: "muted", version: 0 });
      preference = { mode: "muted", version: 1 };
      if (loseReply) {
        loseReply = false;
        return route.abort("failed");
      }
      return route.fulfill({ json: preference });
    },
  );
  await page.goto("/team/project/pull/1");
  await expect(
    page.getByRole("link", { name: "Notifications, unread" }),
  ).toBeVisible();
  const watch = page.getByRole("button", { name: "Watch", exact: true });
  await watch.click();
  const dialog = page.getByRole("dialog", { name: "Repository notifications" });
  const choice = dialog.getByLabel("Notify me about");
  const save = dialog.getByRole("button", { name: "Save preference" });
  await expect(choice).toHaveValue("direct");
  await expect(save).toBeDisabled();
  await expectNoAccessibilityViolations(page);
  await choice.selectOption("muted");
  await save.click();
  await expect(dialog.getByRole("alert")).toContainText("response was lost");
  await save.click();
  await expect(dialog).toBeHidden();
  await expect(
    page.getByRole("link", { name: "Notifications", exact: true }),
  ).toBeVisible();
  await page.reload();
  await watch.click();
  await expect(choice).toHaveValue("muted");
});
