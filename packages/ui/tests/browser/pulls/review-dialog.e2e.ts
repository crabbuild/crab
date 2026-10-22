import { expect, test } from "@playwright/test";
import { mockPullRequest } from "./fixture";
import {
  expectNoAccessibilityViolations,
  selectDarkTheme,
} from "../accessibility";

test("review submission stays fixed while the diff finishes loading", async ({
  page,
}) => {
  await mockPullRequest(page);
  let release = () => {};
  const gate = new Promise<void>((resolve) => {
    release = resolve;
  });
  await page.route("**/api/repos/team/project/changes?**", async (route) => {
    await gate;
    await route.fallback();
  });
  try {
    await page.goto("/team/project/pull/1/files");
    await page
      .getByRole("button", { name: "Review changes", exact: true })
      .click();
    const dialog = page.getByRole("dialog", {
      name: "Submit your review",
      exact: true,
    });
    await dialog.evaluate(async (element) => {
      await Promise.allSettled(
        element.getAnimations().map((animation) => animation.finished),
      );
      await document.fonts.ready;
    });
    const submit = dialog.getByRole("button", {
      name: "Submit review",
      exact: true,
    });
    const before = await submit.boundingBox();
    release();
    await expect(page.locator(".diff-panel")).toContainText("New content");
    expect(await submit.boundingBox()).toEqual(before);
    await dialog
      .getByRole("textbox", { name: "Review summary" })
      .fill("A stable review submission");
    await expectNoAccessibilityViolations(page);
    await submit.click();
    await expect(page).toHaveURL(/\/pull\/1$/);
    await expect(page.locator(".review-event")).toContainText(
      "A stable review submission",
    );
  } finally {
    release();
  }
});

test("a changed head requires reload and explicit draft rebasing before a review can be submitted", async ({
  page,
}) => {
  const model = await mockPullRequest(page);
  const writes: unknown[] = [];
  page.on("request", (request) => {
    if (
      request.method() === "POST" &&
      new URL(request.url()).pathname.endsWith("/pulls/1/reviews")
    )
      writes.push(request.postDataJSON());
  });
  await page.goto("/team/project/pull/1/files?powerbar=1");
  await page
    .getByRole("button", { name: "Review changes", exact: true })
    .click();
  const dialog = page.getByRole("dialog", {
    name: "Submit your review",
    exact: true,
  });
  const editor = dialog.getByRole("textbox", { name: "Review summary" });
  await editor.fill("Keep this review draft while I inspect the new commit");
  model.lifecycle.head = "e".repeat(40);
  await dialog
    .getByRole("button", { name: "Submit review", exact: true })
    .click();
  await expect(dialog.getByRole("alert")).toContainText("head changed");
  await expect(editor).toHaveValue(
    "Keep this review draft while I inspect the new commit",
  );
  await expect(
    dialog.getByRole("button", { name: "Submit review", exact: true }),
  ).toBeDisabled();
  await dialog.getByRole("button", { name: "Reload comparison" }).click();
  await page
    .getByRole("button", { name: "Review changes", exact: true })
    .click();
  await expect(editor).toHaveValue(
    "Keep this review draft while I inspect the new commit",
  );
  await expect(dialog.getByRole("alert")).toContainText("earlier commit");
  await expect(
    dialog.getByRole("button", { name: "Submit review", exact: true }),
  ).toBeDisabled();
  await page.setViewportSize({ width: 390, height: 844 });
  await page.keyboard.press("Escape");
  await selectDarkTheme(page);
  await page
    .getByRole("button", { name: "Review changes", exact: true })
    .click();
  await expectNoAccessibilityViolations(page);
  await dialog.getByRole("button", { name: "Use current comparison" }).click();
  await dialog
    .getByRole("button", { name: "Submit review", exact: true })
    .click();
  await expect(page).toHaveURL(/\/pull\/1\?powerbar=1$/);
  await expect(page.locator(".review-event")).toContainText(
    "Keep this review draft",
  );
  expect(writes).toHaveLength(2);
  expect(writes[0]).toMatchObject({ commit_oid: "b".repeat(40) });
  expect(writes[1]).toMatchObject({ commit_oid: "e".repeat(40) });
  expect((writes[0] as { request_id: string }).request_id).not.toBe(
    (writes[1] as { request_id: string }).request_id,
  );
});
