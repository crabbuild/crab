import { expect, test } from "@playwright/test";
import { expectNoAccessibilityViolations } from "../accessibility";
import { mockPullRequest } from "./fixture";

test("explicitly confirm a ruleset bypass and recover the recorded merge", async ({
  page,
}) => {
  const fixture = await mockPullRequest(page);
  const id = "00000000-0000-4000-8000-0000000000aa";
  interface Request {
    request_id: string;
    version: number;
    method: string;
    base_oid: string;
    head_oid: string;
    message: string;
    bypass_rulesets: string[];
  }
  let saved: Request | undefined;
  let completed = false;
  function view() {
    const pull = fixture.pull();
    return {
      ...pull,
      state: completed ? "merged" : "open",
      can_merge: Boolean(saved) && !completed,
      merge_requirements: {
        ...pull.merge_requirements,
        checks: [],
        rulesets: ["Release approval"],
        bypass_options: saved ? [] : [{ id, name: "Release approval" }],
      },
      merge_pending:
        saved && !completed
          ? {
              ...saved,
              pull_version: saved.version,
              author: { name: "Alice", profile: null },
              created_at: 1700000000000,
            }
          : null,
      merge: completed
        ? {
            ...saved,
            commit_oid: pull.head_oid,
            author: { name: "Alice", profile: null },
            created_at: 1700000000000,
          }
        : null,
      merged_at: completed ? 1700000000000 : null,
    };
  }
  await page.route(
    (url) => url.pathname === "/api/repos/team/project/pulls/1",
    (route) => route.fulfill({ json: view() }),
  );
  await page.route(
    (url) => url.pathname === "/api/repos/team/project/pulls/1/merge",
    (route) => {
      const submitted: Request = route.request().postDataJSON();
      expect(submitted.bypass_rulesets).toEqual([id]);
      if (!saved) {
        saved = submitted;
        return route.fulfill({
          status: 503,
          json: {
            error: {
              message: "Merge outcome is uncertain. Retry the original merge.",
            },
          },
        });
      }
      expect(submitted).toEqual(saved);
      completed = true;
      return route.fulfill({ json: view() });
    },
  );

  await page.goto("/team/project/pull/1");
  const panel = page.getByRole("region", { name: "Merge status" });
  await expect(panel).toContainText("Merging is blocked");
  await panel
    .getByRole("button", { name: "Merge with ruleset bypass" })
    .click();
  await expect(panel).toContainText("Bypass Release approval for this merge.");
  expect(saved).toBeUndefined();
  await expectNoAccessibilityViolations(page);
  await panel.getByRole("button", { name: "Confirm bypass and merge" }).click();
  await panel.getByRole("button", { name: "Retry merge" }).click();
  await expect(panel).toContainText("Pull request merged");
  await expect(panel).toContainText("Ruleset bypass used.");
});
