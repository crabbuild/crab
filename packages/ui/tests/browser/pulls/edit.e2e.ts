import { expect, test } from "@playwright/test";
import type { Review } from "@/components/discussion/conversation/model";
import { expectNoAccessibilityViolations } from "../accessibility";
import { conversationPage } from "../conversation";
import { emptyReactions } from "../fixtures";
import { mockPullRequest } from "./fixture";

test("a pull title edit preserves a description saved in another tab", async ({
  page,
}) => {
  const fixture = await mockPullRequest(page);
  const pull = fixture.pull();
  const path = "/api/repos/team/project/pulls/1";
  await page.route(
    (url) => url.pathname === path,
    async (route) => {
      const request = route.request();
      if (request.method() === "GET") return route.fulfill({ json: pull });
      expect(request.method()).toBe("PATCH");
      expect(request.headers()["x-crab-repository-id"]).toBe(
        fixture.lifecycle.repositoryId,
      );
      const edit = request.postDataJSON();
      if (edit.version !== pull.version)
        return route.fulfill({
          status: 409,
          json: { error: { code: "conflict" } },
        });
      expect(edit).toEqual({ version: 2, title: "Revised title" });
      pull.title = edit.title;
      pull.version++;
      return route.fulfill({ json: pull });
    },
  );
  await page.goto("/team/project/pull/1");
  const trigger = page.getByRole("button", { name: "Edit pull request" });
  await trigger.click();
  const editor = page.getByRole("dialog", { name: "Edit pull request" });
  const title = editor.getByRole("textbox", { name: "Title" });
  const body = editor.getByRole("textbox", { name: "Description" });
  const save = editor.getByRole("button", { name: "Save changes" });
  await expect(title).toBeFocused();
  await expect(save).toBeDisabled();
  await expectNoAccessibilityViolations(page);
  await title.fill("Revised title");
  pull.version = 2;
  pull.body = "Description from another tab";
  await save.click();
  await expect(editor.getByText("Description from another tab")).toBeVisible();
  await expect(save).toBeDisabled();
  await editor.getByRole("button", { name: "Continue with my draft" }).click();
  await expect(title).toHaveValue("Revised title");
  await expect(body).toHaveValue(pull.body);
  await save.click();
  await expect(editor).toBeHidden();
  await expect(trigger).toBeFocused();
  await expect(
    page.getByRole("heading", { name: "Revised title #1" }),
  ).toBeVisible();
  await expect(page.getByText(pull.body)).toBeVisible();
});

test("review summary conflict preserves the draft and a lock prevents further edits", async ({
  page,
}) => {
  const fixture = await mockPullRequest(page);
  const path = "/api/repos/team/project/pulls/1";
  const review: Review = {
    number: 1,
    author: { name: "Alice", profile: null },
    body: "Initial summary",
    reactions: emptyReactions(),
    state: "approved",
    dismissal: null,
    can_dismiss: false,
    current: true,
    commit_oid: fixture.lifecycle.head,
    version: 1,
    created_at: 1_700_000_050_000,
    updated_at: 1_700_000_050_000,
    can_edit: true,
    minimization: { version: 1, reason: "off_topic" },
  };
  let locked = false;
  await page.route(
    (url) => url.pathname === `${path}/conversation`,
    (route) =>
      conversationPage(route, { reviews: [review], head: review.commit_oid }),
  );
  await page.route(
    (url) => url.pathname === `${path}/lock`,
    (route) =>
      route.fulfill({
        json: {
          version: 1,
          locked,
          reason: null,
          can_manage: false,
          can_post: !locked,
        },
      }),
  );
  await page.route(
    (url) => url.pathname === `${path}/reviews/1`,
    async (route) => {
      const request = route.request();
      expect(request.headers()["x-crab-repository-id"]).toBe(
        fixture.lifecycle.repositoryId,
      );
      if (request.method() === "GET") return route.fulfill({ json: review });
      expect(request.method()).toBe("PATCH");
      const edit = request.postDataJSON();
      if (edit.version !== review.version)
        return route.fulfill({
          status: 409,
          json: { error: { code: "conflict" } },
        });
      expect(edit).toEqual({ version: 2, body: "" });
      review.version++;
      review.body = edit.body;
      review.has_edit_history = true;
      return route.fulfill({ json: review });
    },
  );
  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto("/team/project/pull/1#pullrequestreview-1");
  const card = page.locator("#pullrequestreview-1");
  const show = card.getByRole("button", { name: "Show comment" });
  const trigger = card.getByRole("button", { name: "Edit review summary" });
  await show.click();
  await trigger.click();
  const summary = card.getByRole("textbox", { name: "Review summary" });
  const save = card.getByRole("button", { name: "Save changes" });
  await expect(summary).toBeFocused();
  await expect(save).toBeDisabled();
  await expectNoAccessibilityViolations(page);
  await summary.fill("");
  await card.getByRole("button", { name: "Collapse comment" }).click();
  await expect(summary).toBeHidden();
  await show.click();
  await expect(summary).toHaveValue("");
  review.version = 2;
  review.body = "Summary from another tab";
  await save.click();
  await expect(card.getByText("Summary from another tab")).toBeVisible();
  await expect(summary).toHaveValue("");
  await expect(save).toBeDisabled();
  await card.getByRole("button", { name: "Continue with my draft" }).click();
  await save.click();
  await expect(trigger).toBeFocused();
  await expect(
    card.getByRole("button", { name: "View edit history" }),
  ).toBeVisible();
  await expect(card.getByText("approved these changes")).toBeVisible();

  locked = true;
  await page.reload();
  await expect(card.getByText("approved these changes")).toBeVisible();
  await show.click();
  await expect(trigger).toHaveCount(0);
});
