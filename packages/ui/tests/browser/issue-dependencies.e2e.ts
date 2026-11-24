import { expect, test } from "@playwright/test";
import type { Issue } from "@/features/issues/model";
import { expectNoAccessibilityViolations } from "./accessibility";
import { emptyReactions } from "./fixtures";
import { mockPullRequest } from "./pulls/fixture";

test("a dependency retries a lost reply and can be removed from the opposite issue", async ({
  page,
}) => {
  const fixture = await mockPullRequest(page);
  const root = "/api/repos/team/project";
  const issue = (number: number): Issue => ({
    number,
    title: number === 1 ? "Release the app" : "Approve release notes",
    body: "Release preparation",
    version: 1,
    state: "open",
    state_reason: null,
    created_at: 1700000000000,
    updated_at: 1700000000000,
    closed_at: null,
    author: { name: "Alice", profile: null },
    labels: [],
    assignees: [],
    can_edit: false,
    can_close: false,
    can_assign: false,
    can_label: false,
    reactions: emptyReactions(),
  });
  const work = (number: number) => ({
    subject: { kind: "issue", number },
    state: "open",
    title: issue(number).title,
  });
  const writes: unknown[] = [];
  let active = false;
  let version = 0;
  await page.route(
    (url) => /^\/api\/repos\/team\/project\/issues\/[12]$/.test(url.pathname),
    (route) =>
      route.fulfill({
        json: issue(
          Number(new URL(route.request().url()).pathname.split("/").at(-1)),
        ),
      }),
  );
  await page.route(
    (url) => url.pathname === `${root}/projects/candidates`,
    (route) => route.fulfill({ json: { items: [work(1), work(2)] } }),
  );
  await page.route("**/issues/*/dependencies/**", (route) => {
    const request = route.request();
    const segments = new URL(request.url()).pathname.split("/");
    const [number, , direction, pair] = segments.slice(6);
    if (request.method() === "PUT") {
      expect(request.headers()["x-crab-repository-id"]).toBe(
        fixture.lifecycle.repositoryId,
      );
      const change = request.postDataJSON();
      writes.push(change);
      expect(Number(pair)).toBe(number === "1" ? 2 : 1);
      if (change.active !== active) {
        expect(change.version).toBe(version);
        active = change.active;
        version++;
      }
      if (writes.length === 1)
        return route.fulfill({
          status: 503,
          json: { error: { message: "Reply lost" } },
        });
      return route.fulfill({ json: { active, version } });
    }
    if (pair) return route.fulfill({ json: { active, version } });
    const show =
      active &&
      (number === "1" ? direction === "blocked_by" : direction === "blocking");
    return route.fulfill({
      json: {
        items: show ? [{ work: work(number === "1" ? 2 : 1), version }] : [],
        next: null,
        can_manage: true,
      },
    });
  });

  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto("/team/project/issues/1");
  await page.getByText("Issue dependencies", { exact: true }).click();
  const blocked = page.getByRole("region", { name: "Blocked by", exact: true });
  await blocked.getByRole("button", { name: "Add blocker" }).click();
  const chooser = page.getByRole("dialog", { name: "Add blocker" });
  await expect(
    chooser.getByRole("button", { name: /Release the app/ }),
  ).toHaveCount(0);
  await chooser.getByRole("button", { name: /Approve release notes/ }).click();
  await expectNoAccessibilityViolations(page);
  const submit = chooser.getByRole("button", { name: "Add dependency" });
  await submit.click();
  await expect(chooser.getByRole("alert")).toContainText("Reply lost");
  await submit.click();
  await expect(chooser).toBeHidden();
  const blocker = blocked.getByRole("link", {
    name: "#2 Approve release notes",
  });
  await blocker.click();
  await expect(page).toHaveURL(/\/issues\/2$/);
  await page.getByText("Issue dependencies", { exact: true }).click();
  const blocking = page.getByRole("region", { name: "Blocking", exact: true });
  await blocking.getByRole("button", { name: "Remove dependency #1" }).click();
  await expect(blocking.getByText("No issues.")).toBeVisible();
  expect(writes).toEqual([
    { active: true, version: 0 },
    { active: true, version: 0 },
    { active: false, version: 1 },
  ]);
});
