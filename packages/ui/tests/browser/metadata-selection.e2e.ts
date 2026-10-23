import { expect, test, type Page } from "@playwright/test";
import type { RepositoryLabel } from "@/lib/api";
import type { Milestone } from "@/features/milestones/model";
import { mockPullRequest } from "./pulls/fixture";
import {
  expectNoAccessibilityViolations,
  selectDarkTheme,
} from "./accessibility";

async function fixture(page: Page, kind: "issues" | "pulls") {
  const base = await mockPullRequest(page);
  const labels: RepositoryLabel[] = ["bug", "documentation", "release"].map(
    (name, index) => ({
      id: index + 1,
      name,
      color: "1d76db",
      description: `${name} work`,
      version: 1,
      created_at: 1_700_000_000_000,
      updated_at: 1_700_000_000_000,
    }),
  );
  const milestones: Milestone[] = Array.from({ length: 55 }, (_, index) => ({
    number: index + 1,
    title: index === 54 ? "Release next" : `Planning ${index + 1}`,
    state: index === 53 ? "closed" : "open",
    description: "",
    due_on: null,
    author: "Alice",
    open_count: 0,
    closed_count: 0,
    version: 1,
    created_at: 1_700_000_000_000,
    updated_at: 1_700_000_000_000,
    closed_at: null,
  }));
  let version = 1;
  let selected = [1];
  let assigned: number | null = null;
  let assignmentVersion = 0;
  let loseReply = false;
  const writes: Array<{
    version: number;
    label_ids?: number[];
    milestone?: number | null;
  }> = [];
  const root = `/api/repos/team/project/${kind}/1`;
  const detail = () => ({
    ...base.pull(),
    title: "Review metadata",
    version,
    can_label: true,
    can_assign: false,
    can_close: true,
    labels: labels.filter((label) => selected.includes(label.id)),
    assignees: [],
  });
  const assignment = () => ({
    milestone: milestones.find((value) => value.number === assigned) ?? null,
    version: assignmentVersion,
    can_manage: true,
  });
  await page.route("**/api/repos/team/project/**", async (route) => {
    const request = route.request();
    const url = new URL(request.url());
    if (url.pathname === "/api/repos/team/project/labels")
      return route.fulfill({ json: { items: labels, can_manage: true } });
    if (
      url.pathname === "/api/repos/team/project/discussion-search/milestones"
    ) {
      const matches = milestones.filter(
        (value) =>
          value.state === url.searchParams.get("state") &&
          value.title
            .toLowerCase()
            .includes((url.searchParams.get("search") ?? "").toLowerCase()),
      );
      return route.fulfill({
        json: { items: matches.slice(0, 20), truncated: matches.length > 20 },
      });
    }
    if (url.pathname === root) {
      if (request.method() === "PATCH") {
        const input = request.postDataJSON() as {
          version: number;
          label_ids: number[];
        };
        writes.push(input);
        if (input.version !== version)
          return route.fulfill({
            status: 409,
            json: {
              error: { code: "conflict", message: "This discussion changed" },
            },
          });
        selected = input.label_ids;
        version++;
        if (loseReply) {
          loseReply = false;
          return route.abort("failed");
        }
      }
      return route.fulfill({ json: detail() });
    }
    if (url.pathname === `${root}/milestone`) {
      if (request.method() === "PUT") {
        const input = request.postDataJSON() as {
          version: number;
          milestone: number | null;
        };
        writes.push(input);
        if (assigned !== input.milestone && input.version !== assignmentVersion)
          return route.fulfill({
            status: 409,
            json: {
              error: { code: "conflict", message: "The milestone changed" },
            },
          });
        if (assigned !== input.milestone) assignmentVersion++;
        assigned = input.milestone;
        if (loseReply) {
          loseReply = false;
          return route.abort("failed");
        }
      }
      return route.fulfill({ json: assignment() });
    }
    return route.fallback();
  });
  return {
    writes,
    labels,
    milestones,
    root,
    concurrentLabels() {
      selected = [2];
      version++;
    },
    concurrentMilestone() {
      assigned = 1;
      assignmentVersion++;
      version++;
    },
    loseReply() {
      loseReply = true;
    },
  };
}

for (const kind of ["issues", "pulls"] as const) {
  test(`${kind}: label draft survives refresh and requires review before replacing concurrent labels`, async ({
    page,
  }) => {
    const state = await fixture(page, kind);
    await page.goto(`/team/project/${kind === "pulls" ? "pull" : "issues"}/1`);
    await page.getByLabel("Edit labels").click();
    await expect(
      page.getByRole("heading", { name: "Apply labels", exact: true }),
    ).toHaveCSS("font-size", "14px");
    await page.getByRole("checkbox", { name: "release", exact: true }).check();
    state.concurrentLabels();
    if (kind === "issues") {
      const refreshed = page.waitForResponse(
        (response) => new URL(response.url()).pathname === state.root,
      );
      // A pending page action can refresh the parent while its picker is open.
      await page
        .getByRole("button", { name: "Refresh", exact: true })
        .evaluate((button: HTMLButtonElement) => button.click());
      await refreshed;
      await expect(
        page.getByRole("complementary", { name: "Metadata" }),
      ).toContainText("documentation");
    }
    await expect(
      page.getByRole("checkbox", { name: "release", exact: true }),
    ).toBeChecked();
    const search = page.getByRole("textbox", { name: "Filter labels" });
    await search.fill("release");
    await expect(
      page.getByRole("checkbox", { name: "bug", exact: true }),
    ).toHaveCount(0);
    await search.clear();
    await page
      .getByRole("button", { name: "Apply labels", exact: true })
      .click();
    await expect(
      page.getByRole("region", { name: "Review current labels" }),
    ).toContainText("documentation");
    const choices = page.getByRole("region", {
      name: "Label choices",
      exact: true,
    });
    await choices.scrollIntoViewIfNeeded();
    await expect(choices).toBeInViewport();
    expect(state.writes[0].version).toBe(1);
    await expectNoAccessibilityViolations(page);
    await page.getByRole("button", { name: "Keep my selection" }).click();
    await page
      .getByRole("button", { name: "Apply labels", exact: true })
      .click();
    await expect(
      page.getByRole("button", { name: "Apply labels", exact: true }),
    ).toHaveCount(0);
    expect(state.writes).toEqual([
      { version: 1, label_ids: [1, 3] },
      { version: 2, label_ids: [1, 3] },
    ]);
    const metadata = page.getByRole("complementary", { name: "Metadata" });
    await expect(metadata).toContainText("release");
    await expect(metadata).not.toContainText("documentation");
    await page.getByLabel("Edit labels").click();
    await expect(search).toBeFocused();
    await search.press("Escape");
    await expect(page.getByLabel("Edit labels")).toBeFocused();
  });

  test(`${kind}: milestone search reaches all records and preserves a draft through conflict and lost reply review`, async ({
    page,
  }) => {
    const state = await fixture(page, kind);
    if (kind === "pulls")
      await page.setViewportSize({ width: 390, height: 844 });
    await page.goto(`/team/project/${kind === "pulls" ? "pull" : "issues"}/1`);
    await selectDarkTheme(page);
    await page.getByLabel("Edit milestone").click();
    await expect(
      page.getByRole("heading", { name: "Assign a milestone", exact: true }),
    ).toHaveCSS("font-size", "14px");
    const search = page.getByRole("textbox", { name: "Filter milestones" });
    await expect(search).toBeFocused();
    await expect(
      page.getByText("More milestones match. Refine your search."),
    ).toBeVisible();
    await page.getByRole("radio", { name: "Closed milestones" }).click();
    await expect(
      page.getByRole("radio", { name: "Planning 54", exact: true }),
    ).toBeVisible();
    await page.getByRole("radio", { name: "Open milestones" }).click();
    await search.fill("Release next");
    await page
      .getByRole("radio", { name: "Release next", exact: true })
      .check();
    state.concurrentMilestone();
    if (kind === "issues") {
      const refreshed = page.waitForResponse(
        (response) => new URL(response.url()).pathname === state.root,
      );
      await page
        .getByRole("button", { name: "Refresh", exact: true })
        .evaluate((button: HTMLButtonElement) => button.click());
      await refreshed;
    }
    await expect(
      page.getByRole("radio", { name: "Release next", exact: true }),
    ).toBeChecked();
    await page
      .getByRole("button", { name: "Apply milestone", exact: true })
      .click();
    await expect(
      page.getByRole("region", { name: "Review current milestone" }),
    ).toContainText("Planning 1");
    expect(state.writes[0].version).toBe(0);
    await page.getByRole("button", { name: "Keep my selection" }).click();
    state.loseReply();
    await page
      .getByRole("button", { name: "Apply milestone", exact: true })
      .click();
    const review = page.getByRole("region", {
      name: "Review current milestone",
    });
    await expect(review).toContainText("Release next");
    await expect(
      page.getByRole("button", { name: "Apply milestone", exact: true }),
    ).toBeDisabled();
    await expectNoAccessibilityViolations(page);
    await review.getByRole("button", { name: "Use saved selection" }).click();
    await page.getByRole("button", { name: "Cancel", exact: true }).click();
    await expect(
      page
        .getByRole("complementary", { name: "Metadata" })
        .getByRole("link", { name: "Release next", exact: true }),
    ).toBeVisible();
    expect(state.writes).toEqual([
      { version: 0, milestone: 55 },
      { version: 1, milestone: 55 },
    ]);
  });
}

test("lost label reply is reconciled without another write on mobile", async ({
  page,
}) => {
  const state = await fixture(page, "pulls");
  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto("/team/project/pull/1");
  await page.getByLabel("Edit labels").click();
  await page.getByRole("checkbox", { name: "release", exact: true }).check();
  state.loseReply();
  await page.getByRole("button", { name: "Apply labels", exact: true }).click();
  const review = page.getByRole("region", { name: "Review current labels" });
  await expect(review).toContainText("release");
  await expect(
    page.getByRole("button", { name: "Apply labels", exact: true }),
  ).toBeDisabled();
  await expectNoAccessibilityViolations(page);
  await review.getByRole("button", { name: "Use saved selection" }).click();
  await page.getByRole("button", { name: "Close label picker" }).click();
  await expect(
    page.getByRole("complementary", { name: "Metadata" }),
  ).toContainText("release");
  expect(state.writes).toEqual([{ version: 1, label_ids: [1, 3] }]);
});
