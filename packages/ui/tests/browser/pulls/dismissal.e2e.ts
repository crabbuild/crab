import { expect, test, type Page } from "@playwright/test";
import { mockPullRequest } from "./fixture";
import { conversationPage } from "../conversation";
import { emptyReactions } from "../fixtures";
import {
  expectNoAccessibilityViolations,
  selectDarkTheme,
} from "../accessibility";
import type {
  Review,
  ActivityEvent,
} from "@/components/discussion/conversation/model";

async function setup(page: Page) {
  await mockPullRequest(page);
  const model: {
    review: Review;
    writes: Array<{ request_id: string; version: number; body: string }>;
    events: ActivityEvent[];
    lose: boolean;
    conflict: boolean;
  } = {
    review: {
      number: 1,
      author: { name: "Bob", profile: null },
      state: "changes_requested",
      body: "Please update the error handling.",
      commit_oid: "b".repeat(40),
      current: true,
      version: 1,
      created_at: 1700000000000,
      updated_at: 1700000000000,
      can_edit: false,
      reactions: emptyReactions(),
      can_dismiss: true,
      dismissal: null,
    },
    writes: [],
    events: [],
    lose: false,
    conflict: false,
  };
  const submissions = new Map<string, object>();
  const root = "/api/repos/team/project/pulls/1";
  await page.route(
    (url) => url.pathname === root + "/conversation",
    (route) =>
      conversationPage(route, {
        reviews: [model.review],
        activity: model.events,
        head: "b".repeat(40),
      }),
  );
  await page.route(
    (url) => url.pathname === root + "/reviews/1",
    (route) => {
      expect(route.request().method()).toBe("GET");
      return route.fulfill({ json: model.review });
    },
  );
  await page.route(
    (url) => url.pathname === root + "/reviews/1/dismissals",
    async (route) => {
      const input = route
        .request()
        .postDataJSON() as (typeof model.writes)[number];
      expect(route.request().headers()["x-crab-repository-id"]).toBe(
        "00000000-0000-4000-8000-000000000099",
      );
      model.writes.push(input);
      if (model.conflict) {
        model.conflict = false;
        model.review = {
          ...model.review,
          version: 2,
          body: "Updated review context",
        };
        return route.fulfill({
          status: 409,
          headers: { "X-Crab-Write-Outcome": "rejected" },
          json: {
            error: { code: "conflict", message: "Review changed elsewhere" },
          },
        });
      }
      const original = submissions.get(input.request_id);
      if (original) expect(input).toEqual(original);
      else {
        expect(input.version).toBe(model.review.version);
        submissions.set(input.request_id, input);
        model.review = {
          ...model.review,
          state: "dismissed",
          version: model.review.version + 1,
          can_dismiss: false,
          dismissal: {
            author: { name: "Alice", profile: null },
            body: input.body,
            created_at: 1700000060000,
          },
        };
        model.events.push({
          number: 2,
          target: { kind: "pull_review", pull: 1, number: 1 },
          title: "Improve the README",
          actor: { name: "Alice", profile: null },
          action: { kind: "review_dismissed" },
          created_at: 1700000060000,
        });
      }
      if (model.lose) {
        model.lose = false;
        return route.fulfill({
          status: 503,
          json: { error: { message: "The reply was lost after saving" } },
        });
      }
      return route.fulfill({ json: { dismissed: true } });
    },
  );
  await page.goto("/team/project/pull/1");
  await expect(page.locator("#pullrequestreview-1")).toContainText(
    "requested changes",
  );
  return model;
}
async function open(page: Page) {
  await page.getByRole("button", { name: "Review #1 actions" }).click();
  await page
    .getByRole("menuitem", { name: /Dismiss review|Recover review dismissal/ })
    .click();
  await expect(
    page.getByRole("dialog", { name: "Dismiss review" }),
  ).toBeVisible();
}

test("dismissal requires a reason, records it in history, and returns focus to the review", async ({
  page,
}) => {
  const model = await setup(page);
  await open(page);
  const submit = page
    .getByRole("dialog")
    .getByRole("button", { name: "Dismiss review", exact: true });
  await expect(submit).toBeDisabled();
  await page
    .getByLabel("Reason for dismissal")
    .fill("The requested changes were completed.");
  await submit.click();
  await expect(page.getByRole("dialog")).toHaveCount(0);
  const review = page.locator("#pullrequestreview-1");
  await expect(review).toContainText("Dismissed");
  await expect(review).toContainText("The requested changes were completed.");
  await expect(review).toBeFocused();
  await expect(page.locator(".conversation-activity")).toContainText(
    "Alice dismissed a review on",
  );
  expect(model.writes).toHaveLength(1);
  await page.reload();
  await expect(review).toContainText("The requested changes were completed.");
  await expect(
    page.getByRole("button", { name: "Review #1 actions" }),
  ).toHaveCount(0);
  await expectNoAccessibilityViolations(page);
});

test("a lost dismissal reply retains its exact intent after write permission disappears", async ({
  page,
}) => {
  const model = await setup(page);
  model.lose = true;
  await open(page);
  await page
    .getByLabel("Reason for dismissal")
    .fill("Confirmed with the author.");
  await page
    .getByRole("dialog")
    .getByRole("button", { name: "Dismiss review", exact: true })
    .click();
  await expect(page.getByText("The reply was lost after saving")).toBeVisible();
  await expect(page.getByLabel("Reason for dismissal")).toBeDisabled();
  await page.getByRole("button", { name: "Retry original dismissal" }).click();
  await expect(page.getByRole("dialog")).toHaveCount(0);
  expect(model.writes).toHaveLength(2);
  expect(model.writes[1]).toEqual(model.writes[0]);
  await expect(page.locator("#pullrequestreview-1")).toContainText(
    "Confirmed with the author.",
  );
});

test("a stale review preserves the reason until current content is inspected", async ({
  page,
}) => {
  const model = await setup(page);
  model.conflict = true;
  await open(page);
  await page
    .getByLabel("Reason for dismissal")
    .fill("Keep this explanation through the conflict");
  await page
    .getByRole("dialog")
    .getByRole("button", { name: "Dismiss review", exact: true })
    .click();
  await page.getByRole("button", { name: "Review current version" }).click();
  await expect(page.getByRole("dialog")).toContainText(
    "Updated review context",
  );
  await page
    .getByRole("button", { name: "Continue with current review" })
    .click();
  await expect(page.getByLabel("Reason for dismissal")).toHaveValue(
    "Keep this explanation through the conflict",
  );
  await page
    .getByRole("dialog")
    .getByRole("button", { name: "Dismiss review", exact: true })
    .click();
  await expect(page.getByRole("dialog")).toHaveCount(0);
  expect(model.writes[1].version).toBe(2);
  expect(model.writes[1].request_id).not.toBe(model.writes[0].request_id);
});

test("dismissal dialogs preserve a cancelled draft and fit narrow dark screens", async ({
  page,
}) => {
  await setup(page);
  await page.setViewportSize({ width: 390, height: 844 });
  await selectDarkTheme(page);
  await open(page);
  await page.getByLabel("Reason for dismissal").fill("A useful explanation");
  await page.keyboard.press("Escape");
  await expect(
    page.getByRole("button", { name: "Review #1 actions" }),
  ).toBeFocused();
  await open(page);
  await expect(page.getByLabel("Reason for dismissal")).toHaveValue(
    "A useful explanation",
  );
  await expectNoAccessibilityViolations(page);
  expect(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= innerWidth,
    ),
  ).toBe(true);
});
