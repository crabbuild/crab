import { expect, test, type Page } from "@playwright/test";
import { mockPullRequest } from "./pulls/fixture";
import {
  expectNoAccessibilityViolations,
  selectDarkTheme,
} from "./accessibility";

async function searchFixture(page: Page) {
  await mockPullRequest(page);
  const searches: string[] = [];
  await page.route("**/api/repos/team/project/issues?**", async (route) => {
    const query = new URL(route.request().url()).searchParams.get("q") ?? "";
    searches.push(query);
    if (query.endsWith(" OR"))
      return route.fulfill({
        status: 400,
        json: {
          error: { message: "Search query needs a term after its operator" },
        },
      });
    return route.fulfill({
      json: {
        items: [
          {
            number: 1,
            title: "Keyboard focus",
            state: "open",
            author: "Alice",
            created_at: 1700000000000,
            labels: [],
            assignees: [],
            comment_count: 3,
            reaction_count: 0,
          },
        ],
        next: null,
        counts: { open: 1, closed: 0, merged: 0 },
        matching: 1,
      },
    });
  });
  await page.route(
    "**/api/repos/team/project/discussion-search?**",
    async (route) => {
      const params = new URL(route.request().url()).searchParams;
      const source = params.get("query") ?? "";
      const cursor = Number(params.get("cursor"));
      const label = source.lastIndexOf("label:", cursor);
      const start = label >= 0 ? label : 0;
      const end = label >= 0 ? source.indexOf(" OR", label) : source.length;
      const items =
        label >= 0
          ? [
              {
                value: 'label:"Help wanted"',
                description: "First contribution",
              },
              { value: 'label:"Keyboard"', description: "Keyboard navigation" },
            ]
          : [
              { value: "is:open", description: "Open work" },
              { value: "no:assignee", description: "Unassigned work" },
              { value: "label:", description: "Filter by repository label" },
            ];
      return route.fulfill({
        headers: { "Server-Timing": "cell;dur=1.5" },
        json: {
          start,
          end: end < 0 ? source.length : end,
          items,
          error:
            source === "label:"
              ? "Search qualifier or phrase needs a value"
              : null,
        },
      });
    },
  );
  return searches;
}

test("search completes native suggestions with Unicode caret ranges and preserves URL queries", async ({
  page,
}) => {
  const searches = await searchFixture(page);
  await page.goto("/team/project/issues?powerbar=1");
  const search = page.getByRole("search", { name: "Search issues" });
  const input = search.getByRole("combobox");
  await input.focus();
  await expect(
    page.getByRole("listbox", { name: "Search suggestions" }),
  ).toBeVisible();
  await expectNoAccessibilityViolations(page);
  await input.fill("label:");
  await expect(search.getByRole("status")).toContainText("needs a value");
  await expect(
    page.getByRole("option", {
      name: 'label:"Help wanted" First contribution',
    }),
  ).toBeVisible();
  await input.press("ArrowDown");
  await input.press("Enter");
  await expect(input).toHaveValue('label:"Help wanted" ');
  await expect(input).toBeFocused();
  await expect(page.getByRole("listbox")).toHaveCount(0);
  await input.press("Enter");
  await expect(page).toHaveURL(/q=label%3A%22Help\+wanted%22/);
  expect(searches).toContain('label:"Help wanted"');
  await input.fill("🦀 AND label:he OR keyboard");
  for (let i = 0; i < 12; i++) await input.press("ArrowLeft");
  await expect(
    page.getByRole("option", {
      name: 'label:"Help wanted" First contribution',
    }),
  ).toBeVisible();
  await input.press("ArrowDown");
  await input.press("Enter");
  await expect(input).toHaveValue('🦀 AND label:"Help wanted" OR keyboard');
  await input.press("Enter");
  await expect
    .poll(() => new URL(page.url()).searchParams.get("q"))
    .toBe('🦀 AND label:"Help wanted" OR keyboard');
  await page.reload();
  await expect(input).toHaveValue('🦀 AND label:"Help wanted" OR keyboard');
  await selectDarkTheme(page);
  await input.focus();
  await expect(page.getByRole("listbox")).toBeVisible();
  await expectNoAccessibilityViolations(page);
  await input.press("Escape");
  await expect(page.getByRole("listbox")).toHaveCount(0);
  await expect(input).toBeFocused();
  await search.getByRole("button", { name: "Clear", exact: true }).click();
  await expect(page).not.toHaveURL(/q=/);
  await expect(input).toHaveValue("");
});

test("syntax and suggestion errors remain visible without losing the query", async ({
  page,
}) => {
  await searchFixture(page);
  await page.goto("/team/project/issues");
  const search = page.getByRole("search", { name: "Search issues" });
  const input = search.getByRole("combobox");
  await input.fill("keyboard OR");
  await search.getByRole("button", { name: "Search", exact: true }).click();
  await expect(page.getByRole("alert")).toContainText(
    "needs a term after its operator",
  );
  await expect(input).toHaveValue("keyboard OR");
  let unavailable = true;
  await page.route("**/discussion-search?**", (route) => {
    if (unavailable) {
      unavailable = false;
      return route.fulfill({
        status: 503,
        json: { error: { message: "Repository owner unavailable" } },
      });
    }
    return route.fulfill({
      json: { start: 0, end: 6, items: [], error: null },
    });
  });
  await input.fill("label:");
  await expect(search.getByRole("status")).toContainText(
    "Repository owner unavailable",
  );
  await expect(input).toHaveValue("label:");
  await expect(input).toBeFocused();
  await search.getByRole("button", { name: "Retry suggestions" }).click();
  await expect(
    search.getByRole("button", { name: "Retry suggestions" }),
  ).toHaveCount(0);
  await expect(input).toHaveValue("label:");
  await expect(input).toBeFocused();
});

for (const view of ["issues", "pulls"] as const) {
  test(`${view} counts, ordering and opaque pagination preserve the query`, async ({
    page,
  }) => {
    await mockPullRequest(page);
    const requests: URLSearchParams[] = [];
    let stale = false;
    await page.route(`**/api/repos/team/project/${view}?**`, async (route) => {
      const params = new URL(route.request().url()).searchParams;
      requests.push(params);
      if (params.has("before") && stale)
        return route.fulfill({
          status: 409,
          json: {
            error: {
              message:
                "This discussion list has changed. Return to the first page to continue.",
            },
          },
        });
      return route.fulfill({
        json: {
          items: [
            {
              number: 1,
              title: "Indexed discussion",
              author: "Alice",
              state: params.get("state") === "closed" ? "closed" : "open",
              created_at: 1700000000000,
              labels: [],
              assignees: [],
              comment_count: 7,
              reaction_count: 2,
              draft: false,
              base_ref: "refs/heads/main",
              head_ref: "refs/heads/topic",
            },
          ],
          next: params.has("before") ? null : "opaque-native-cursor",
          counts: { open: 1234, closed: 432, merged: 12 },
          matching: 1234,
        },
      });
    });
    await page.goto(`/team/project/${view}?q=keyboard&powerbar=1`);
    const states = page.getByRole("navigation", {
      name: view === "issues" ? "Issue state" : "Pull request state",
    });
    await expect(
      states.getByRole("link", { name: "Open 1,234" }),
    ).toHaveAttribute("aria-current", "page");
    await expect(
      states.getByRole("link", { name: "Closed 432" }),
    ).toBeVisible();
    await expect(page.getByRole("link", { name: "7 comments" })).toBeVisible();
    await page.getByRole("button", { name: "Sort discussions" }).click();
    await expectNoAccessibilityViolations(page);
    await page
      .getByRole("menuitemradio", { name: "Comments", exact: true })
      .click();
    await expect
      .poll(() => new URL(page.url()).searchParams.get("sort"))
      .toBe("comments");
    await page.getByRole("button", { name: "Sort discussions" }).click();
    await page.getByRole("menuitemradio", { name: "Lowest first" }).click();
    await expect
      .poll(() => new URL(page.url()).searchParams.get("direction"))
      .toBe("asc");
    const older = page.getByRole("link", { name: `Older ${view} →` });
    await older.click();
    await expect
      .poll(() => new URL(page.url()).searchParams.get("before"))
      .toBe("opaque-native-cursor");
    await expect.poll(() => requests.at(-1)?.get("direction")).toBe("asc");
    expect(requests.at(-1)?.get("sort")).toBe("comments");
    expect(requests.at(-1)?.get("q")).toBe("keyboard");
    await page.reload();
    await expect(
      page.getByRole("link", { name: "Back to first page" }),
    ).toBeVisible();
    stale = true;
    await page.reload();
    await expect(page.getByRole("alert")).toContainText(
      "This discussion list has changed",
    );
    await page.getByRole("link", { name: "Back to first page" }).click();
    await expect(page).not.toHaveURL(/before=/);
    await states.getByRole("link", { name: "Closed 432" }).click();
    await expect
      .poll(() => new URL(page.url()).searchParams.get("state"))
      .toBe("closed");
    expect(new URL(page.url()).searchParams.get("q")).toBe("keyboard");
    expect(new URL(page.url()).searchParams.get("sort")).toBe("comments");
    await selectDarkTheme(page);
    await page.setViewportSize({ width: 390, height: 844 });
    await expectNoAccessibilityViolations(page);
    expect(
      await page.evaluate(
        () => document.documentElement.scrollWidth <= innerWidth,
      ),
    ).toBe(true);
  });
}
