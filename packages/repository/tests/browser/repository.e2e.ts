import { expect, test } from "@playwright/test";

const oid = "a".repeat(40);
const pathHex = (path: string) =>
  Array.from(new TextEncoder().encode(path), (byte) =>
    byte.toString(16).padStart(2, "0"),
  ).join("");

test.beforeEach(async ({ page }) => {
  await page.route("**/api/**", async (route) => {
    const url = new URL(route.request().url());
    if (url.pathname === "/api/session")
      return route.fulfill({
        json: { authenticated: true, mode: "local", user: null, csrf: null },
      });
    if (url.pathname === "/api/repos")
      return route.fulfill({
        json: {
          repositories: [
            {
              owner: "team",
              name: "project",
              description: "A repository for our team.",
              access: "write",
            },
          ],
        },
      });
    if (url.pathname.endsWith("/refs"))
      return route.fulfill({
        json: {
          head: { name: "refs/heads/main", oid },
          refs: [{ name: "refs/heads/main", oid }],
          generation: 1,
        },
      });
    if (url.pathname.endsWith("/commit"))
      return route.fulfill({
        json: {
          oid,
          tree: "b".repeat(40),
          parents: [],
          author: "Alice",
          author_seconds: 1_700_000_000,
          message: "Make the repository easier to browse",
        },
      });
    if (url.pathname.endsWith("/tree"))
      return route.fulfill({
        json: {
          items: (url.searchParams.get("path_hex")
            ? []
            : [
                ["README.md", "Blob"],
                ["src", "Tree"],
              ]
          ).map(([path, kind]) => ({
            path,
            path_hex: pathHex(path),
            kind,
            oid,
            mode: kind === "Tree" ? "040000" : "100644",
          })),
          next: null,
          commit: oid,
        },
      });
    if (url.pathname.endsWith("/file"))
      return route.fulfill({
        json: {
          oid,
          size: 12,
          mode: "100644",
          classification: "OrdinaryGit",
          text: "Hello, team!",
        },
      });
    return route.fulfill({
      status: 404,
      json: { error: { message: "Fixture route unavailable" } },
    });
  });
});

test("overview groups files with their commit and opens the tree when navigating", async ({
  page,
}) => {
  await page.goto("/team/project");
  await expect(
    page.getByRole("complementary", { name: "About this repository" }),
  ).toBeVisible();
  await expect(page.locator(".tree-sidebar")).toHaveCount(0);
  const panel = page.getByRole("region", { name: "Folders and files" });
  await expect(
    panel.getByText("Make the repository easier to browse"),
  ).toBeVisible();
  await expect(panel.locator("tbody tr").first()).toContainText("src");
  await expect(
    panel.getByRole("button", { name: "Next", exact: true }),
  ).toHaveCount(0);
  await panel.getByRole("link", { name: "README.md", exact: true }).click();
  await expect(page.locator(".tree-sidebar")).toBeVisible();
  await expect(page.locator(".breadcrumb")).toHaveText("project/README.md");
  await expect(page.locator(".file-panel")).toBeVisible();
  await page.getByRole("button", { name: "Browse files", exact: true }).click();
  await expect(page.locator(".tree-sidebar")).toHaveCount(0);
});

test("mobile Code menu stays within the viewport and theme selection persists", async ({
  page,
}) => {
  await page.setViewportSize({ width: 360, height: 800 });
  await page.goto("/team/project");
  await expect(page.getByRole("table")).toBeVisible();
  for (const [width, theme] of [
    [360, "dark"],
    [390, "light"],
  ] as const) {
    await page.setViewportSize({ width, height: 800 });
    await page.getByLabel("Appearance", { exact: true }).selectOption(theme);
    await page.locator(".clone-menu summary").click();
    await expect(
      page.getByLabel("Repository URL", { exact: true }),
    ).toHaveValue("http://127.0.0.1:5175/git/team/project.git");
    await expect(
      page.getByRole("button", { name: "Copy URL", exact: true }),
    ).toBeVisible();
    const geometry = await page
      .locator(".clone-menu .git-popover")
      .evaluate((menu) => {
        const bounds = menu.getBoundingClientRect();
        return {
          viewport: innerWidth,
          body: document.documentElement.scrollWidth,
          left: bounds.left,
          right: bounds.right,
        };
      });
    expect(geometry.body).toBeLessThanOrEqual(geometry.viewport);
    expect(geometry.left).toBeGreaterThanOrEqual(0);
    expect(geometry.right).toBeLessThanOrEqual(geometry.viewport);
    await page.locator(".clone-menu summary").click();
  }
  await page.reload();
  await expect(page.getByLabel("Appearance", { exact: true })).toHaveValue(
    "light",
  );
});

test("tag-only repositories browse files without inventing a default branch", async ({
  page,
}) => {
  await page.route(
    (url) => url.pathname === "/api/repos/team/project/refs",
    (route) =>
      route.fulfill({
        json: {
          head: null,
          unborn_head: "refs/heads/main",
          refs: [{ name: "refs/tags/v1", oid }],
          generation: 1,
        },
      }),
  );
  await page.goto("/team/project");
  await expect(
    page.getByRole("button", {
      name: "Switch branches or tags, current v1",
      exact: true,
    }),
  ).toBeVisible();
  await expect(
    page.getByText("This repository is empty", { exact: true }),
  ).toHaveCount(0);
  await expect(page.getByRole("table")).toBeVisible();
  await page
    .getByRole("table")
    .getByRole("link", { name: "README.md", exact: true })
    .click();
  await expect(page.locator(".file-panel")).toBeVisible();
});

test("revision picker filters branches and tags and restores keyboard focus", async ({
  page,
}) => {
  const branch = "release/a-very-long-branch-name-that-still-fits-on-a-phone";
  await page.route(
    (url) => url.pathname === "/api/repos/team/project/refs",
    (route) =>
      route.fulfill({
        json: {
          head: { name: "refs/heads/main", oid },
          unborn_head: null,
          refs: [
            { name: "refs/heads/main", oid },
            { name: `refs/heads/${branch}`, oid: "b".repeat(40) },
            {
              name: "refs/tags/v1.0",
              oid: "c".repeat(40),
              peeled: "d".repeat(40),
            },
          ],
          generation: 1,
        },
      }),
  );
  await page.goto("/team/project");
  const anchor = page.getByRole("button", { name: /^Switch branches or tags/ });
  await anchor.click();
  const dialog = page.getByRole("dialog", {
    name: "Switch branches/tags",
    exact: true,
  });
  const search = dialog.getByRole("textbox", {
    name: "Filter branches",
    exact: true,
  });
  await expect(search).toBeFocused();
  await expect(
    dialog.getByRole("menuitemradio", { name: "main default", exact: true }),
  ).toHaveAttribute("aria-checked", "true");
  await search.fill("missing");
  await expect(dialog.getByRole("status")).toHaveText(
    "No branches match “missing”.",
  );
  await search.fill("RELEASE/");
  await search.press("ArrowDown");
  await expect(
    dialog.getByRole("menuitemradio", { name: branch, exact: true }),
  ).toBeFocused();
  await page.keyboard.press("Enter");
  await expect(dialog).toHaveCount(0);
  await expect(anchor).toBeFocused();
  await expect(page).toHaveURL(new RegExp("rev=refs%2Fheads%2Frelease"));
  await page.setViewportSize({ width: 360, height: 800 });
  expect(
    await page.evaluate(() => document.documentElement.scrollWidth),
  ).toBeLessThanOrEqual(360);
  await anchor.click();
  await dialog.getByRole("tab", { name: "Branches", exact: true }).focus();
  await page.keyboard.press("ArrowRight");
  await expect(
    dialog.getByRole("tab", { name: "Tags", exact: true }),
  ).toBeFocused();
  await dialog
    .getByRole("textbox", { name: "Filter tags", exact: true })
    .fill("v1");
  await dialog
    .getByRole("menuitemradio", { name: "v1.0", exact: true })
    .click();
  await expect(anchor).toHaveText("v1.0");
  await anchor.click();
  const geometry = await dialog.evaluate((element) => ({
    left: element.getBoundingClientRect().left,
    right: element.getBoundingClientRect().right,
    width: innerWidth,
    page: document.documentElement.scrollWidth,
  }));
  expect(geometry.left).toBeGreaterThanOrEqual(0);
  expect(geometry.right).toBeLessThanOrEqual(geometry.width);
  expect(geometry.page).toBeLessThanOrEqual(geometry.width);
  await page.keyboard.press("Escape");
  await expect(anchor).toBeFocused();
  for (const scheme of ["light", "dark"]) {
    await page.getByLabel("Appearance", { exact: true }).selectOption(scheme);
    await expect(page.locator("html")).toHaveCSS("color-scheme", scheme);
  }
});
