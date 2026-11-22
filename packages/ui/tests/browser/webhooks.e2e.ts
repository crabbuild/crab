import { expect, test } from "@playwright/test";
import { expectNoAccessibilityViolations } from "./accessibility";
import { mockPullRequest } from "./pulls/fixture";

test("webhook setup recovers its secret and redelivery keeps the delivery ID", async ({
  page,
}) => {
  const { lifecycle } = await mockPullRequest(page);
  const deliveryId = "01900000-0000-7000-8000-000000000101";
  let subscription: {
    id: string;
    push: boolean;
    pull_request: boolean;
  } | null = null;
  let setup: unknown;
  let status = "failed";
  await page.route("**/api/session", (route) =>
    route.fulfill({
      json: {
        authenticated: true,
        mode: "local",
        user: null,
        csrf: "fixture-csrf",
      },
    }),
  );
  await page.route(
    (url) => url.pathname === "/api/repos",
    (route) =>
      route.fulfill({
        json: {
          repositories: [
            {
              id: lifecycle.repositoryId,
              owner: "team",
              name: "project",
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
      }),
  );
  await page.route(
    (url) => url.pathname === "/api/repos/team/project/settings/webhooks",
    async (route) => {
      const request = route.request();
      expect(request.headers()["x-crab-repository-id"]).toBe(
        lifecycle.repositoryId,
      );
      if (request.method() === "GET")
        return route.fulfill({
          json: {
            destination: "https://ci.example.test/events",
            subscription,
            active: subscription !== null,
            deliveries: subscription
              ? [
                  {
                    id: deliveryId,
                    event: "pull_request",
                    action: "opened",
                    pull: 1,
                    queued_at: 1770000000000,
                    status,
                    attempts: 3,
                    version: 7,
                    can_redeliver: status !== "queued",
                    recent_attempts: [{ at: 1770000000000, http_status: 503 }],
                  },
                ]
              : [],
          },
        });
      expect(request.headers()["x-csrf-token"]).toBe("fixture-csrf");
      const change = request.postDataJSON();
      if (change.kind === "configure") {
        if (!setup) {
          setup = change;
          expect(change).toMatchObject({
            expected: null,
            push: false,
            pull_request: true,
            id: expect.any(String),
          });
          subscription = {
            id: change.id,
            push: change.push,
            pull_request: change.pull_request,
          };
          return route.abort("failed");
        }
        expect(change).toEqual(setup);
        return route.fulfill({ json: { secret: "fixture-outbound-secret" } });
      }
      if (change.kind === "redeliver") {
        expect(change).toEqual({
          kind: "redeliver",
          id: deliveryId,
          version: 7,
        });
        status = "queued";
        return route.fulfill({ json: { queued: true } });
      }
      expect(change).toEqual({ kind: "disable", expected: subscription?.id });
      subscription = null;
      return route.fulfill({ json: { secret: null } });
    },
  );

  await page.goto("/team/project/settings/webhooks");
  await expect(page.getByRole("heading", { name: "Webhooks" })).toBeVisible();
  await page.getByRole("checkbox", { name: "Git pushes" }).uncheck();
  const enable = page.getByRole("button", { name: "Enable webhook" });
  await enable.click();
  await expect(page.getByRole("alert")).toContainText("The response was lost.");
  await enable.click();
  await expect(
    page.getByRole("textbox", { name: "Copy this secret to your receiver." }),
  ).toHaveValue("fixture-outbound-secret");
  await page.getByRole("button", { name: "Hide secret" }).click();
  await page.getByText("Delivery details · 3 attempts").click();
  await expect(page.getByText(deliveryId)).toBeVisible();
  await expect(page.getByText("HTTP 503", { exact: false })).toBeVisible();
  await expectNoAccessibilityViolations(page);
  await page.getByRole("button", { name: "Redeliver" }).click();
  await expect(page.getByText("queued", { exact: true })).toBeVisible();
  await page.reload();
  await expect(page.getByText("queued", { exact: true })).toBeVisible();
  await page.getByRole("button", { name: "Disable webhook" }).click();
  await expect(enable).toBeVisible();
});
