import { expect, test } from "@playwright/test";
import type { Connection } from "@/features/settings/integrations";
import type { BranchProtection } from "@/lib/contracts";
import { expectNoAccessibilityViolations } from "./accessibility";
import { mockPullRequest } from "./pulls/fixture";

test("a workflow can be connected, required by a branch, rotated, and disconnected", async ({
  page,
}) => {
  const { lifecycle, pull } = await mockPullRequest(page);
  let rules: BranchProtection[] = [];
  let protectionVersion = 0;
  let connection: Connection | undefined;
  let loseSetupReply = true;
  let reported = false;
  let delivered = false;
  const actionsUrl = "https://github.com/example/mirror/actions/runs/789";
  await page.route(
    (url) => url.pathname.endsWith("/deliveries"),
    (route) => {
      expect(route.request().headers()["x-crab-repository-id"]).toBe(
        lifecycle.repositoryId,
      );
      expect(route.request().url()).toContain(`/${connection?.id}/deliveries`);
      return route.fulfill({
        json: delivered
          ? [
              {
                id: "01900000-0000-7000-8000-000000000101",
                received_at_ms: 1770000000000,
                oid: pull().head_oid,
                run_number: 10,
                attempt: 2,
                status: "completed",
                conclusion: "success",
                details_url: actionsUrl,
                updated_check: true,
              },
            ]
          : [],
      });
    },
  );
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
              protection_version: protectionVersion,
              protected_branches: rules,
            },
          ],
        },
      }),
  );
  await page.route(
    (url) =>
      url.pathname.startsWith("/api/repos/") &&
      url.pathname.endsWith("/settings/integrations"),
    (route) => {
      const request = route.request();
      if (request.method() === "GET")
        return route.fulfill({
          json: {
            available: true,
            items: connection ? [connection] : [],
          },
        });
      expect(request.headers()["x-csrf-token"]).toBe("fixture-csrf");
      expect(request.headers()["x-crab-repository-id"]).toBe(
        lifecycle.repositoryId,
      );
      const change = request.postDataJSON();
      switch (change.kind) {
        case "connect":
          expect(change).toMatchObject({
            name: "ci/test",
            github_repository: 123,
            workflow: 456,
          });
          if (connection) expect(change.id).toBe(connection.id);
          connection = {
            id: change.id,
            name: change.name,
            github_repository: change.github_repository,
            workflow: change.workflow,
            credential_version: change.id,
            app_managed: false,
            app_bound: false,
            url: `https://crab.example/webhooks/github-actions/${lifecycle.repositoryId}/${change.id}`,
          };
          if (loseSetupReply) {
            loseSetupReply = false;
            return route.abort("failed");
          }
          return route.fulfill({
            json: { connection, secret: "fixture-first-secret" },
          });
        case "rotate":
          expect(change.id).toBe(connection?.id);
          expect(change.expected).toBe(connection?.credential_version);
          connection!.credential_version = change.credential;
          return route.fulfill({
            json: { connection, fetch_token: "fixture-rotated-checkout-token" },
          });
        case "dispatch":
          expect(change.id).toBe(connection?.id);
          expect(change.expected).toBe(connection?.credential_version);
          expect(change.previous).toBeNull();
          expect(change.dispatch).toEqual({
            id: expect.any(String),
            installation: 42,
            repository: "example/mirror",
            branch: "main",
          });
          connection!.dispatch = change.dispatch;
          connection!.app_managed = true;
          connection!.url = "https://crab.example/webhooks/github-actions/app";
          return route.fulfill({
            json: { connection, fetch_token: "fixture-checkout-token" },
          });
        case "disconnect":
          expect(change.expected).toBe(connection?.credential_version);
          connection = undefined;
          return route.fulfill({ json: { disconnected: true } });
        default:
          throw new Error("Unexpected integration action");
      }
    },
  );
  await page.route(
    (url) => url.pathname.endsWith("/settings/branch-protections"),
    (route) => {
      const input = route.request().postDataJSON();
      expect(input).toEqual({
        expected_version: protectionVersion,
        rules: [
          {
            branch: "main",
            required_approvals: 0,
            require_conversation_resolution: true,
            require_linear_history: true,
            required_checks: [],
            required_integrations: [connection!.id],
          },
        ],
      });
      rules = input.rules;
      protectionVersion++;
      return route.fulfill({ json: { rules, version: protectionVersion } });
    },
  );
  await page.route(
    (url) => url.pathname === "/api/repos/team/project/pulls/1",
    (route) => {
      const base = pull();
      return route.fulfill({
        json: {
          ...base,
          merge_methods: ["merge_commit", "squash", "rebase", "fast_forward"],
          merge_requirements: {
            ...base.merge_requirements,
            checks: [
              {
                context: "ci/test",
                state: reported ? "success" : null,
                description: reported
                  ? "GitHub Actions workflow run #10"
                  : "Waiting for GitHub Actions on this commit.",
                target_url: reported ? actionsUrl : null,
                author: reported ? "GitHub Actions" : null,
                updated_at: null,
                run_id: reported ? 9 : null,
                provider: "github_actions",
                head_sha: base.head_oid,
                integration_id: connection?.id,
              },
            ],
          },
        },
      });
    },
  );
  const run = () => ({
    id: 9,
    head_sha: pull().head_oid,
    name: "ci/test",
    status: "completed",
    conclusion: "success",
    details_url: actionsUrl,
    output_title: "GitHub Actions workflow run #10",
    author: "GitHub Actions",
    provider: "github_actions",
    version: 1,
    output: {
      title: "Workflow report",
      summary: "Finished in GitHub Actions.",
      text: null,
      steps: [],
      annotations: [],
    },
  });
  await page.route(
    (url) =>
      /\/api\/repos\/team\/project\/commits\/[a-f\d]+\/check-runs(?:\/9)?$/.test(
        url.pathname,
      ),
    (route) =>
      route.fulfill({
        json: new URL(route.request().url()).pathname.endsWith("/9")
          ? run()
          : { items: [run()], sha: pull().head_oid, next: null },
      }),
  );

  await page.goto("/team/project/settings");
  await page.getByRole("link", { name: "Integrations", exact: true }).click();
  const connect = page.getByRole("form", { name: "Connect GitHub workflow" });
  await connect.getByLabel("Check name").fill("ci/test");
  await connect.getByLabel("GitHub repository ID").fill("123");
  await connect.getByLabel("GitHub workflow ID").fill("456");
  await connect.getByRole("button", { name: "Connect workflow" }).click();
  await expect(page.getByRole("alert")).toContainText("The response was lost");
  await connect.getByRole("button", { name: "Connect workflow" }).click();
  await expect(page.getByLabel("Webhook secret", { exact: true })).toHaveValue(
    "fixture-first-secret",
  );
  await expectNoAccessibilityViolations(page);
  await page.getByRole("button", { name: "Hide credentials" }).click();
  await expect(page.getByLabel("Webhook secret", { exact: true })).toHaveCount(
    0,
  );
  const deliveries = page.getByRole("button", {
    name: "Recent deliveries for ci/test",
  });
  await deliveries.click();
  const history = page.getByRole("dialog", { name: "Recent deliveries" });
  await expect(history.getByText("No accepted deliveries yet.")).toBeVisible();
  delivered = true;
  await history.getByRole("button", { name: "Refresh deliveries" }).click();
  await expect(history.getByText("Run #10 · Attempt 2")).toBeVisible();
  await expect(history.getByText("Check updated")).toBeVisible();
  await expect(
    history.getByRole("link", { name: pull().head_oid }),
  ).toHaveAttribute(
    "href",
    `/team/project/commit/${pull().head_oid}?repository_id=${lifecycle.repositoryId}`,
  );
  await expect(
    history.getByRole("link", { name: "Open GitHub Actions run" }),
  ).toHaveAttribute("href", actionsUrl);
  await expectNoAccessibilityViolations(page);
  await page.keyboard.press("Escape");
  await expect(deliveries).toBeFocused();
  // The operator authorizes a binding. Enabling queue dispatch switches the
  // connection to the operator's App.
  connection!.app_bound = true;
  await page.reload();
  await page.getByRole("button", { name: "Set up queue dispatch" }).click();
  const dispatch = page.getByRole("form", { name: "Automatic queue dispatch" });
  await dispatch
    .getByLabel("GitHub owner and repository")
    .fill("example/mirror");
  await dispatch.getByLabel("GitHub App installation ID").fill("42");
  await dispatch.getByLabel("GitHub workflow branch").fill("main");
  await dispatch.getByRole("button", { name: "Save queue dispatch" }).click();
  await expect(
    page.getByLabel("Queue checkout token", { exact: true }),
  ).toHaveValue("fixture-checkout-token");
  await page.getByRole("button", { name: "Hide credentials" }).click();
  await expect(
    page.getByText("Managed by the server GitHub App"),
  ).toBeVisible();
  await expect(
    page.getByRole("button", { name: "Rotate secret for ci/test" }),
  ).toHaveCount(0);
  await expect(
    page.getByText("Queue dispatch on example/mirror · main"),
  ).toBeVisible();

  const settings = page.getByRole("navigation", {
    name: "Repository settings",
  });
  await settings.getByRole("link", { name: "Branches" }).click();
  await page
    .getByRole("button", { name: "Add branch protection rule" })
    .click();
  await page.getByLabel("Branch name").fill("main");
  await page
    .getByRole("checkbox", { name: "Require conversation resolution" })
    .check();
  await page.getByRole("checkbox", { name: "Require linear history" }).check();
  await page
    .getByRole("button", { name: "Require a GitHub Actions workflow" })
    .click();
  await page.getByRole("checkbox", { name: "ci/test · workflow 456" }).check();
  await page.getByRole("button", { name: "Save changes" }).click();
  await expect(
    page.getByText("GitHub Actions · ci/test", { exact: true }),
  ).toBeVisible();
  await expect(
    page.getByText("Conversation resolution required", { exact: true }),
  ).toBeVisible();
  await expect(
    page.getByText("Linear history required", { exact: true }),
  ).toBeVisible();
  await page.goto("/team/project/pull/1");
  await expect(
    page.getByText("Required provider · GitHub Actions"),
  ).toBeVisible();
  await expect(
    page.getByText("Waiting for GitHub Actions on this commit."),
  ).toBeVisible();
  reported = true;
  await page.getByRole("button", { name: "Refresh checks" }).click();
  await page.getByRole("link", { name: "Details", exact: true }).click();
  await expect(
    page.getByText("Provider · GitHub Actions", { exact: true }),
  ).toBeVisible();
  await expect(
    page.getByRole("link", { name: "External details" }),
  ).toHaveAttribute("href", actionsUrl);

  await page.goto("/team/project/settings/integrations");
  await page
    .getByRole("button", { name: "Rotate queue checkout token for ci/test" })
    .click();
  await page.getByRole("button", { name: "Confirm rotation" }).click();
  await expect(
    page.getByLabel("Queue checkout token", { exact: true }),
  ).toHaveValue("fixture-rotated-checkout-token");
  await expect(page.getByLabel("Webhook URL", { exact: true })).toHaveValue(
    connection!.url,
  );
  await expect(page.getByLabel("Webhook secret", { exact: true })).toHaveCount(
    0,
  );
  await page.getByRole("button", { name: "Disconnect ci/test" }).click();
  await expect(
    page.getByText("Branch rules that require this workflow", { exact: false }),
  ).toBeVisible();
  await page.getByRole("button", { name: "Confirm disconnect" }).click();
  await expect(page.getByText("No workflows connected.")).toBeVisible();
  await settings.getByRole("link", { name: "Branches" }).click();
  await page.getByRole("button", { name: "Edit main" }).click();
  await expect(
    page.getByRole("checkbox", { name: /Disconnected workflow/ }),
  ).toBeChecked();
});
