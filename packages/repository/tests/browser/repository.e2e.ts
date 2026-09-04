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
    ).toHaveValue("http://127.0.0.1:5175/git/team/project");
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
