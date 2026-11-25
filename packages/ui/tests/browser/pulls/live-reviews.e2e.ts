import { expect, test } from "@playwright/test";
import { conversationPage } from "../conversation";
import { emptyReactions, emptyReviewers } from "../fixtures";
import { mockPullRequest } from "./fixture";

test("live review decisions and head changes preserve drafts and disable an outdated merge", async ({
  page,
}) => {
  const fixture = await mockPullRequest(page);
  const original = fixture.pull();
  const root = "/api/repos/team/project/pulls/1";
  const bob = { name: "Bob", profile: null };
  let submitted = false;
  let eligible = true;
  let head = original.head_oid;
  const version = () => original.version + Number(submitted);
  const review = () => ({
    number: 1,
    author: bob,
    state: "approved",
    body: "The current change looks good.",
    commit_oid: original.head_oid,
    current: head === original.head_oid,
    version: 1,
    created_at: 1_700_000_060_000,
    updated_at: 1_700_000_060_000,
    can_edit: false,
    reactions: emptyReactions(),
    can_dismiss: false,
    dismissal: null,
  });
  await page.route(
    (url) => url.pathname === root,
    (route) => {
      const approved = submitted && eligible && head === original.head_oid;
      return route.fulfill({
        json: {
          ...original,
          version: version(),
          head_oid: head,
          can_merge: approved,
          merge_requirements: {
            ...original.merge_requirements,
            approvals: Number(approved),
            checks: [],
            checks_satisfied: true,
            satisfied: approved,
          },
        },
      });
    },
  );
  await page.route(
    (url) => url.pathname === root + "/reviewers",
    (route) => {
      expect(route.request().headers()["x-crab-repository-id"]).toBe(
        fixture.lifecycle.repositoryId,
      );
      return route.fulfill({
        json: {
          ...emptyReviewers(),
          version: version(),
          can_manage: true,
          items: submitted
            ? [
                {
                  id: "bob",
                  reviewer: bob,
                  candidate_id: "bob",
                  request: null,
                  review: review(),
                  decision: review(),
                  merge_eligible: eligible,
                },
              ]
            : [],
        },
      });
    },
  );
  await page.route(
    (url) => url.pathname === root + "/conversation",
    (route) =>
      conversationPage(route, { reviews: submitted ? [review()] : [], head }),
  );

  await page.clock.install();
  await page.goto("/team/project/pull/1");
  const merge = page.getByRole("region", { name: "Merge status" });
  const reviewers = page.getByRole("region", { name: "Reviewers" });
  const history = page.getByRole("region", { name: "Conversation history" });
  const editor = page.getByRole("textbox", { name: "Comment", exact: true });
  await expect(merge).toContainText("1 more approving review is required");
  await editor.fill("I am still writing this comment.");
  await reviewers.getByRole("button", { name: "Request reviewers" }).click();
  const search = page.getByRole("textbox", { name: "Find a reviewer" });
  await search.fill("Bob");

  submitted = true;
  await page.clock.fastForward(15_000);
  await expect(merge).toContainText("Merge requirements are satisfied");
  await expect(history).toContainText("The current change looks good.");
  await expect(reviewers).toContainText("Eligible approval");
  await expect(search).toHaveValue("Bob");
  await expect(search).toBeFocused();
  await page.keyboard.press("Escape");
  await expect(editor).toHaveValue("I am still writing this comment.");

  await merge.getByRole("button", { name: "Merge pull request" }).click();
  const message = merge.getByRole("textbox", { name: "Merge commit message" });
  const confirm = merge.getByRole("button", { name: "Confirm merge" });
  await message.fill("Document the reviewed work");
  await expect(confirm).toBeEnabled();

  eligible = false;
  await page.clock.fastForward(15_000);
  await expect(reviewers).toContainText("Write access required");
  await expect(confirm).toBeDisabled();
  await expect(message).toHaveValue("Document the reviewed work");

  eligible = true;
  head = "e".repeat(40);
  await page.clock.fastForward(15_000);
  await expect(reviewers).toContainText("Outdated review");
  await expect(confirm).toBeDisabled();
  await expect(editor).toHaveValue("I am still writing this comment.");
});
