import { chooseSelectOption } from "../select";
import { expect, test } from "@playwright/test";
import { mockPullRequest } from "./fixture";

test("base changes use the previewed tips and recover a missing response", async ({
  page,
}) => {
  const fixture = await mockPullRequest(page);
  const pull = fixture.pull();
  const stable = "refs/heads/stable";
  const tip = "c".repeat(40);
  let saved = false;
  const attempts: { request_id: string }[] = [];
  const root = "/api/repos/team/project";
  await page.route(
    (url) => url.pathname === `${root}/pulls/1`,
    (route) =>
      route.fulfill({
        json: {
          ...pull,
          can_change_base: true,
          base_ref: saved ? stable : pull.base_ref,
          base_oid: saved ? tip : pull.base_oid,
        },
      }),
  );
  await page.route(
    (url) => url.pathname === `${root}/refs`,
    (route) =>
      route.fulfill({
        json: {
          head: { name: pull.base_ref, oid: pull.base_oid },
          unborn_head: null,
          generation: 1,
          refs: [
            { name: pull.base_ref, oid: pull.base_oid },
            { name: pull.head_ref, oid: pull.head_oid },
            { name: stable, oid: tip },
          ],
        },
      }),
  );
  await page.route(
    (url) => url.pathname === `${root}/changes`,
    (route) =>
      route.fulfill({
        json: {
          base: pull.base_oid,
          base_tip: tip,
          commit: pull.head_oid,
          changes: [],
        },
      }),
  );
  await page.route(
    (url) => url.pathname === `${root}/pulls/1/base`,
    async (route) => {
      attempts.push(route.request().postDataJSON());
      if (attempts.length === 1) {
        await route.fulfill({
          status: 200,
          contentType: "application/json",
          body: "",
        });
        return;
      }
      saved = true;
      await route.fulfill({ json: { saved: true } });
    },
  );
  await page.goto("/team/project/pull/1");
  await page.getByRole("button", { name: "Change base", exact: true }).click();
  const dialog = page.getByRole("dialog", { name: "Change the base branch" });
  const choice = dialog.getByLabel("New base branch");
  await expect(choice.locator(`option[value="${pull.head_ref}"]`)).toHaveCount(
    0,
  );
  await chooseSelectOption(choice, stable);
  await expect(
    dialog.getByRole("region", { name: "New comparison" }),
  ).toContainText(tip.slice(0, 7));
  await dialog.getByRole("button", { name: "Confirm base change" }).click();
  await expect(dialog.getByRole("alert")).toContainText("The reply was lost");
  await dialog.getByRole("button", { name: "Retry base change" }).click();
  await expect(dialog).toBeHidden();
  await expect(
    page.locator(".pull-summary").getByText("stable", { exact: true }),
  ).toBeVisible();
  expect(attempts).toEqual([
    {
      request_id: expect.any(String),
      version: pull.version,
      base_ref: stable,
      base_oid: tip,
      head_oid: pull.head_oid,
    },
    attempts[0],
  ]);
});
