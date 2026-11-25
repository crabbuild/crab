import { expect, test } from "@playwright/test";
import { mockPullRequest } from "./fixture";

test("a saved commit keeps its PR context and offers a way back when its objects expire", async ({
  page,
}) => {
  const fixture = await mockPullRequest(page);
  fixture.lifecycle.state = "merged";
  fixture.lifecycle.branchesAvailable = false;
  const { base_oid: base, head_oid: oid } = fixture.pull();
  const history = "/api/repos/team/project/pulls/1/merged-comparison/commits";
  const detail = `${history}/${oid}`;
  const metadata = {
    oid,
    parents: [base],
    author: "Alice",
    author_seconds: 1_700_000_040,
    message: "Finish reviewed feature\n\nDescribe the deployment workflow.",
  };
  const path = "README.md";
  const path_hex = "524541444d452e6d64";
  const text = "Deploy the reviewed feature.\n";
  let available = true;
  await page.route(
    (url) => url.pathname === history,
    (route) => route.fulfill({ json: { items: [metadata], next: null } }),
  );
  await page.route(
    (url) => url.pathname === detail,
    (route) => {
      expect(route.request().headers()["x-crab-repository-id"]).toBe(
        fixture.lifecycle.repositoryId,
      );
      return available
        ? route.fulfill({
            json: {
              metadata,
              base,
              commit: oid,
              changes: [
                { path, path_hex, kind: "Added", old: null, new: null },
              ],
            },
          })
        : route.fulfill({
            status: 404,
            json: {
              error: {
                message:
                  "The Git objects for this saved comparison are no longer available.",
              },
            },
          });
    },
  );
  await page.route(
    (url) => url.pathname === `${detail}/file`,
    (route) => {
      expect(route.request().headers()["x-crab-repository-id"]).toBe(
        fixture.lifecycle.repositoryId,
      );
      expect(new URL(route.request().url()).searchParams.get("path_hex")).toBe(
        path_hex,
      );
      return route.fulfill({
        json: {
          base,
          commit: oid,
          path,
          old: null,
          new: { oid, size: text.length, text, text_truncated: false },
        },
      });
    },
  );

  await page.goto("/team/project/pull/1/commits");
  const commit = page.getByRole("link", { name: "Finish reviewed feature" });
  await commit.press("Enter");
  await expect(page).toHaveURL(new RegExp(`/pull/1/commits/${oid}\\?`));
  await expect(page.getByText("Deploy the reviewed feature.")).toBeVisible();
  await page.screenshot({ path: test.info().outputPath("commit-desktop.png") });
  await page.setViewportSize({ width: 390, height: 844 });
  await page.reload();
  await expect(
    page.getByText("Describe the deployment workflow."),
  ).toBeVisible();
  await expect(page.getByText("Deploy the reviewed feature.")).toBeVisible();
  expect(await page.evaluate(() => document.documentElement.scrollWidth)).toBe(
    390,
  );
  await page.screenshot({ path: test.info().outputPath("commit-mobile.png") });

  available = false;
  await page.reload();
  await expect(page.getByRole("alert")).toContainText(
    "The Git objects for this saved comparison are no longer available.",
  );
  await page.getByRole("link", { name: "Back to commits" }).click();
  await expect(commit).toBeVisible();
});
