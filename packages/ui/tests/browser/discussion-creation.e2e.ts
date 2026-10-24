import { expect, test, type Page } from "@playwright/test";
import { mockPullRequest } from "./pulls/fixture";
import {
  expectNoAccessibilityViolations,
  selectDarkTheme,
} from "./accessibility";
import type { CreationSelection } from "@/components/discussion/create/metadata";

interface Input {
  request_id: string;
  title: string;
  body: string;
  draft?: boolean;
  metadata: CreationSelection;
}
async function fixture(
  page: Page,
  kind: "issues" | "pulls",
  failures: "lost" | "rejected",
) {
  const base = await mockPullRequest(page);
  const writes: Input[] = [];
  let created: Input | undefined;
  let effects = 0;
  const label = {
    id: 1,
    name: "bug",
    color: "1d76db",
    description: "Confirmed defect",
    version: 1,
    created_at: 1_700_000_000_000,
    updated_at: 1_700_000_000_000,
  };
  const person = {
    id: "00000000-0000-4000-8000-000000000001",
    name: "Local operator",
    profile: null,
  };
  const milestone = {
    number: 1,
    title: "Release",
    description: "",
    state: "open",
    due_on: null,
    author: "Alice",
    open_count: 1,
    closed_count: 0,
    version: 1,
    created_at: 1_700_000_000_000,
    updated_at: 1_700_000_000_000,
    closed_at: null,
  };
  const detail = () => ({
    ...base.pull(),
    number: 2,
    title: created!.title,
    body: created!.body,
    draft: created!.draft ?? false,
    version: 1,
    can_label: true,
    can_assign: true,
    can_close: true,
    labels: created!.metadata.label_ids.length ? [label] : [],
    assignees: created!.metadata.assignee_ids.length ? [person] : [],
  });
  await page.route("**/api/repos/team/project/**", async (route) => {
    const request = route.request();
    const url = new URL(request.url());
    const root = "/api/repos/team/project/";
    if (url.pathname === root + "assignees")
      return route.fulfill({ json: { items: [person], can_manage: true } });
    if (url.pathname === root + "labels")
      return route.fulfill({ json: { items: [label], can_manage: true } });
    if (url.pathname === root + "discussion-search/milestones")
      return route.fulfill({ json: { items: [milestone], truncated: false } });
    if (url.pathname === root + kind && request.method() === "POST") {
      const input = request.postDataJSON() as Input;
      writes.push(input);
      if (failures === "rejected" && writes.length === 1)
        return route.fulfill({
          status: 400,
          headers: { "X-Crab-Write-Outcome": "rejected" },
          json: {
            error: {
              code: "creation_rejected",
              message: "An initial label is no longer available",
            },
          },
        });
      if (!created) {
        created = input;
        effects++;
      }
      if (failures === "lost" && writes.length === 1)
        return route.abort("failed");
      if (failures === "lost" && writes.length === 2)
        return route.fulfill({
          status: 403,
          json: {
            error: {
              code: "forbidden",
              message: "Current context is unavailable",
            },
          },
        });
      return route.fulfill({ status: 201, json: detail() });
    }
    if (url.pathname === root + kind + "/2")
      return route.fulfill({ json: detail() });
    if (url.pathname === root + kind + "/2/milestone")
      return route.fulfill({
        json: {
          milestone: created!.metadata.milestone ? milestone : null,
          version: 1,
          can_manage: true,
        },
      });
    return route.fallback();
  });
  return { writes, effects: () => effects };
}
async function chooseMetadata(page: Page) {
  await page
    .getByRole("button", { name: "Choose labels", exact: true })
    .click();
  await page.getByRole("checkbox", { name: "bug", exact: true }).check();
  await page.getByRole("button", { name: "Done", exact: true }).click();
  await page
    .getByRole("button", { name: "Choose assignees", exact: true })
    .click();
  await page
    .getByRole("checkbox", { name: "Local operator", exact: true })
    .check();
  await page.getByRole("button", { name: "Done", exact: true }).click();
  await page
    .getByRole("button", { name: "Choose milestone", exact: true })
    .click();
  await page.getByRole("radio", { name: "Release", exact: true }).check();
  await page.getByRole("button", { name: "Done", exact: true }).click();
}
for (const kind of ["issues", "pulls"] as const) {
  test(`${kind}: creation includes metadata and recovers the original frozen intent after lost replies`, async ({
    page,
  }) => {
    const state = await fixture(page, kind, "lost");
    if (kind === "pulls")
      await page.setViewportSize({ width: 390, height: 844 });
    await page.goto(
      `/team/project/${kind === "issues" ? "issues/new" : "compare"}`,
    );
    if (kind === "pulls") await selectDarkTheme(page);
    await page
      .getByRole("textbox", { name: "Title", exact: true })
      .fill("Create with metadata");
    await page
      .getByRole("textbox", { name: "Description", exact: true })
      .fill("Original creation body");
    await chooseMetadata(page);
    const submit = page.getByRole("button", {
      name: kind === "issues" ? "Create issue" : "Create pull request",
      exact: true,
    });
    await expectNoAccessibilityViolations(page);
    await submit.click();
    const retry = page.getByRole("button", {
      name: "Retry original submission",
      exact: true,
    });
    await expect(retry).toBeVisible();
    await expect(
      page.getByRole("textbox", { name: "Title", exact: true }),
    ).toBeDisabled();
    await expect(
      page.getByRole("button", { name: "Choose labels", exact: true }),
    ).toBeDisabled();
    await retry.click();
    await expect(
      page
        .getByRole("alert")
        .filter({ hasText: "Current context is unavailable" }),
    ).toBeVisible();
    await expect(
      page.getByRole("button", { name: "Review inputs", exact: true }),
    ).toHaveCount(0);
    await retry.click();
    await expect(page).toHaveURL(
      new RegExp(`/team/project/${kind === "issues" ? "issues" : "pull"}/2$`),
    );
    const metadata = page.getByRole("complementary", {
      name: "Metadata",
      exact: true,
    });
    await expect(metadata).toContainText("bug");
    await expect(metadata).toContainText("Local operator");
    await expect(metadata).toContainText("Release");
    expect(state.writes).toHaveLength(3);
    expect(state.writes[1]).toEqual(state.writes[0]);
    expect(state.writes[2]).toEqual(state.writes[0]);
    expect(state.effects()).toBe(1);
    await page.reload();
    await expect(metadata).toContainText("Release");
  });
  test(`${kind}: a proven rejection requires input review and a fresh creation ID`, async ({
    page,
  }) => {
    const state = await fixture(page, kind, "rejected");
    await page.goto(
      `/team/project/${kind === "issues" ? "issues/new" : "compare"}`,
    );
    await page
      .getByRole("textbox", { name: "Title", exact: true })
      .fill("Review initial choices");
    await chooseMetadata(page);
    const submit = page.getByRole("button", {
      name: kind === "issues" ? "Create issue" : "Create pull request",
      exact: true,
    });
    await submit.click();
    await page
      .getByRole("button", { name: "Review inputs", exact: true })
      .click();
    await expect(
      page.getByRole("textbox", { name: "Title", exact: true }),
    ).toBeEnabled();
    await submit.click();
    await expect(page).toHaveURL(/\/2$/);
    expect(state.writes[0].request_id).not.toBe(state.writes[1].request_id);
    expect(state.effects()).toBe(1);
  });
}
