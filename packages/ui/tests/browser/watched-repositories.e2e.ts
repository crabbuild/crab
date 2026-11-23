import { expect, test } from "@playwright/test";
import { expectNoAccessibilityViolations } from "./accessibility";
import { mockPullRequest } from "./pulls/fixture";

test("watched repositories can load past an empty page and save a release-only watch", async ({
  page,
}) => {
  const { lifecycle } = await mockPullRequest(page);
  const repository = {
    id: lifecycle.repositoryId,
    owner: "team",
    name: "project",
  };
  let preference: { mode: string; version: number; releases?: boolean } = {
    mode: "issues",
    version: 3,
  };
  let unavailable = true;
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
    (url) => url.pathname.startsWith("/api/notifications"),
    (route) => {
      const url = new URL(route.request().url());
      if (url.pathname === "/api/notifications/unread")
        return route.fulfill({ json: { unread: false } });
      if (url.pathname === "/api/notifications")
        return route.fulfill({ json: { items: [], next: null } });
      if (url.pathname !== "/api/notifications/watched")
        return route.fallback();
      if (!url.searchParams.has("after"))
        return route.fulfill({ json: { items: [], next: "team/library" } });
      expect(url.searchParams.get("after")).toBe("team/library");
      if (unavailable) {
        unavailable = false;
        return route.fulfill({
          status: 503,
          json: { error: { message: "Temporarily unavailable" } },
        });
      }
      const items =
        preference.mode === "direct" && !preference.releases
          ? []
          : [{ repository, preference }];
      return route.fulfill({ json: { items, next: null } });
    },
  );
  await page.route(
    (url) => url.pathname === "/api/repos/team/project/subscription",
    (route) => {
      const request = route.request();
      expect(request.headers()["x-crab-repository-id"]).toBe(repository.id);
      if (request.method() === "GET")
        return route.fulfill({ json: preference });
      expect(request.method()).toBe("PUT");
      expect(request.headers()["x-csrf-token"]).toBe("fixture-csrf");
      const desired = request.postDataJSON();
      expect(desired).toEqual({
        mode: "direct",
        version: preference.version,
        ...(preference.version === 3 ? { releases: true } : {}),
      });
      preference = { ...desired, version: preference.version + 1 };
      return route.fulfill({ json: preference });
    },
  );

  await page.goto("/-/notifications");
  await page.getByRole("link", { name: "Watched repositories" }).click();
  await expect(
    page.getByRole("heading", { name: "Watched repositories" }),
  ).toBeVisible();
  await expect(
    page.getByText("No watched repositories found so far."),
  ).toBeVisible();
  await page.getByRole("button", { name: "Load more repositories" }).click();
  await expect(page.getByRole("alert")).toContainText(
    "Temporarily unavailable",
  );
  await page.getByRole("button", { name: "Try again" }).click();
  const link = page.getByRole("link", { name: "team/project" });
  await expect(link).toHaveAttribute(
    "href",
    `/team/project?repository_id=${repository.id}`,
  );
  await expectNoAccessibilityViolations(page);

  const watch = page.getByRole("button", { name: "Watch team/project" });
  const dialog = page.getByRole("dialog", { name: "Repository notifications" });
  const releases = dialog.getByRole("checkbox", { name: "New releases" });
  await watch.click();
  await dialog.getByLabel("Notify me about").selectOption("direct");
  await releases.click();
  await dialog.getByRole("button", { name: "Save preference" }).click();
  await expect(dialog).toBeHidden();
  await expect(page.getByText("New releases", { exact: true })).toBeVisible();

  await watch.click();
  await expect(releases).toBeChecked();
  await releases.click();
  await dialog.getByRole("button", { name: "Save preference" }).click();
  await expect(link).toBeHidden();
  await expect(
    page.getByRole("heading", { name: "Watched repositories" }),
  ).toBeFocused();
  await expect(
    page.getByText("You aren’t watching any repositories."),
  ).toBeVisible();
});
