import { chooseSelectOption } from "./select";
import { expect, test } from "@playwright/test";
import { expectNoAccessibilityViolations } from "./accessibility";
import { mockPullRequest } from "./pulls/fixture";

test("repository watch preferences recover a lost reply and refresh the unread badge", async ({
  page,
}) => {
  const { lifecycle } = await mockPullRequest(page);
  const csrf = "fixture-csrf";
  let preference: { mode: string; version: number; releases?: boolean } = {
    mode: "direct",
    version: 0,
  };
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
      const input = request.postDataJSON();
      if (input.mode === "issues") {
        expect(input).toEqual({ mode: "issues", version: 0, releases: true });
        preference = { mode: "issues", version: 1, releases: true };
        if (loseReply) {
          loseReply = false;
          return route.abort("failed");
        }
      } else {
        expect(input).toEqual({ mode: "muted", version: 1 });
        preference = { mode: "muted", version: 2 };
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
  await expect(choice).toHaveText("Direct notifications");
  await expect(save).toBeDisabled();
  await expectNoAccessibilityViolations(page);
  const releases = dialog.getByRole("checkbox", { name: "New releases" });
  await chooseSelectOption(choice, "issues");
  await releases.click();
  await save.click();
  await expect(dialog.getByRole("alert")).toContainText("response was lost");
  await save.click();
  await expect(dialog).toBeHidden();
  await watch.click();
  await expect(choice).toHaveText("Issue updates");
  await expect(releases).toBeChecked();
  await chooseSelectOption(choice, "muted");
  await expect(releases).not.toBeChecked();
  await expect(releases).toBeDisabled();
  await save.click();
  await expect(dialog).toBeHidden();
  await expect(
    page.getByRole("link", { name: "Notifications", exact: true }),
  ).toBeVisible();
  await page.reload();
  await watch.click();
  await expect(choice).toHaveText("Muted");
});

test("pull request notification settings reload a newer choice before retrying", async ({
  page,
}) => {
  const { lifecycle } = await mockPullRequest(page);
  let preference = { mode: "inherit", version: 0 };
  let reads = 0;
  await page.route("**/api/session", (route) =>
    route.fulfill({
      json: {
        authenticated: true,
        mode: "oidc",
        user: { name: "Bob", subject: "bob", issuer: "https://issuer.test" },
        csrf: "fixture-csrf",
      },
    }),
  );
  await page.route(
    (url) => url.pathname === "/api/repos/team/project/pulls/1/subscription",
    (route) => {
      const request = route.request();
      if (request.method() === "GET") {
        reads += 1;
        return route.fulfill({ json: preference });
      }
      expect(request.method()).toBe("PUT");
      expect(request.headers()["x-crab-repository-id"]).toBe(
        lifecycle.repositoryId,
      );
      expect(request.headers()["x-csrf-token"]).toBe("fixture-csrf");
      const update = request.postDataJSON();
      expect(update.mode).toBe("state_changes");
      if (update.version === 0 && preference.version === 1) {
        return route.fulfill({
          status: 409,
          json: {
            error: { code: "conflict", message: "Newer preference saved." },
          },
        });
      }
      expect(update.version).toBe(1);
      preference = { mode: "state_changes", version: 2 };
      return route.fulfill({ json: preference });
    },
  );
  await page.goto("/team/project/pull/1");
  const trigger = page.getByRole("button", { name: "Notification settings" });
  await expect(trigger).toBeVisible();
  expect(reads).toBe(0);
  await trigger.click();
  const dialog = page.getByRole("dialog", {
    name: "Pull request notifications",
  });
  const choice = dialog.getByLabel("Notify me about");
  const save = dialog.getByRole("button", { name: "Save preference" });
  await expect(choice).toHaveText("Use repository setting");
  await expectNoAccessibilityViolations(page);

  preference = { mode: "follow", version: 1 };
  await chooseSelectOption(choice, "state_changes");
  await save.click();
  await expect(dialog.getByRole("alert")).toHaveText("Newer preference saved.");
  await expect(save).toBeDisabled();
  await dialog.getByRole("button", { name: "Reload preference" }).click();
  await expect(choice).toHaveText("All discussion activity");
  await chooseSelectOption(choice, "state_changes");
  await save.click();
  await expect(dialog).toBeHidden();

  await page.reload();
  await trigger.click();
  await expect(choice).toHaveText("State changes only");
});
