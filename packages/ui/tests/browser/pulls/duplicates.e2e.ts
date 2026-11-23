import { expect, test } from "@playwright/test";
import type { ActivityEvent } from "@/components/discussion/conversation/model";
import { expectNoAccessibilityViolations } from "../accessibility";
import { conversationPage } from "../conversation";
import { emptyReactions } from "../fixtures";
import { mockPullRequest } from "./fixture";

test("duplicate undo retries the original event and leaves the comment in place", async ({
  page,
}) => {
  const fixture = await mockPullRequest(page);
  const root = "/api/repos/team/project";
  const actor = { name: "Alice", profile: null };
  const original: ActivityEvent = {
    number: 9,
    target: { kind: "pull", number: 1 },
    title: "Improve the README",
    actor,
    action: {
      kind: "duplicate_marked",
      subject_kind: "pull",
      number: 2,
      title: "Canonical change",
    },
    created_at: 1_700_000_010_000,
  };
  const comment = {
    number: 1,
    author: actor,
    body: "Duplicate of #2",
    version: 1,
    created_at: 1_700_000_000_000,
    updated_at: 1_700_000_000_000,
    can_edit: true,
    reactions: emptyReactions(),
  };
  let active = true;
  const attempts: unknown[] = [];
  await page.route(
    (url) => url.pathname === `${root}/pulls/1/conversation`,
    (route) =>
      conversationPage(route, {
        comments: [comment],
        activity: active
          ? [original]
          : [
              original,
              {
                ...original,
                number: 10,
                action: { ...original.action, kind: "duplicate_unmarked" },
                created_at: original.created_at + 1000,
              },
            ],
        undoDuplicateEvent: active ? original.number : null,
        head: fixture.lifecycle.head,
      }),
  );
  await page.route(
    (url) => url.pathname === `${root}/duplicates/pull/1/undo`,
    (route) => {
      const request = route.request();
      expect(request.headers()["x-crab-repository-id"]).toBe(
        fixture.lifecycle.repositoryId,
      );
      expect(request.method()).toBe("PUT");
      attempts.push(request.postDataJSON());
      active = false;
      return attempts.length === 1
        ? route.fulfill({
            status: 503,
            json: { error: { message: "Reply lost" } },
          })
        : route.fulfill({ json: { undone: true } });
    },
  );
  await page.goto("/team/project/pull/1");
  const entry = page.locator("#event-9");
  await expect(
    entry.getByRole("link", { name: "#2 Canonical change" }),
  ).toHaveAttribute("href", "/team/project/pull/2");
  await expectNoAccessibilityViolations(page);
  const undo = entry.getByRole("button", { name: "Undo duplicate mark" });
  await undo.click();
  await expect(entry.getByRole("alert")).toHaveText("Reply lost");
  await undo.click();
  await expect(page.locator("#event-10")).toContainText(
    "removed the duplicate link",
  );
  await expect(undo).toHaveCount(0);
  await expect(page.locator("#issuecomment-1")).toContainText("Duplicate of");
  expect(attempts).toEqual([{ event: 9 }, { event: 9 }]);
});
