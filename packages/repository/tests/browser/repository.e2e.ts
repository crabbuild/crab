import { expect, test } from "@playwright/test";

const oid = "a".repeat(40);
const pathOid = "b".repeat(40);
const pathParent = "c".repeat(40);
const addedPathOid = "d".repeat(40);
const readme =
  "# Team project\n\nBrowse the [source entry](src/index.ts) without cloning.\n\n" +
  "![Architecture](docs/architecture.png) ![Vector](docs/vector.svg) " +
  "![Build status](https://status.example/build.svg)\n";
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
              protected_branches: [],
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
          oid: url.searchParams.has("path_hex") ? pathOid : oid,
          tree: "b".repeat(40),
          parents: [],
          author: "Alice",
          author_seconds: 1_700_000_000,
          message: url.searchParams.has("path_hex")
            ? "Update this path"
            : "Make the repository easier to browse",
        },
      });
    if (url.pathname.endsWith("/commits"))
      return route.fulfill({
        json: (() => {
          const pathHistory = url.searchParams.has("path_hex");
          const added = url.searchParams.get("cursor") === "older-path";
          return {
            items: [
              {
                oid: pathHistory ? (added ? addedPathOid : pathOid) : oid,
                tree: "b".repeat(40),
                parents: pathHistory && !added ? [pathParent] : [],
                author: "Alice",
                author_seconds: 1_700_000_000,
                message: pathHistory
                  ? added
                    ? "Add this path"
                    : "Update this path"
                  : "Make the repository easier to browse",
                change_kind: pathHistory
                  ? added
                    ? "Added"
                    : "Modified"
                  : undefined,
              },
            ],
            next: pathHistory && !added ? "older-path" : null,
            commit: oid,
          };
        })(),
      });
    if (url.pathname.endsWith("/tree"))
      return route.fulfill({
        json: {
          items: (url.searchParams.get("path_hex") === pathHex("src")
            ? [["src/index.ts", "Blob"]]
            : url.searchParams.get("path_hex")
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
          size: readme.length,
          mode: "100644",
          classification: "OrdinaryGit",
          text:
            url.searchParams.get("path_hex") === pathHex("README.md")
              ? readme
              : "Hello, team!",
        },
      });
    if (url.pathname.endsWith("/issues"))
      return route.fulfill({
        json: {
          items: [
            {
              number: 42,
              author: "Alice",
              body: null,
              title: "Keep object storage reads bounded",
              state:
                url.searchParams.get("state") === "closed" ? "closed" : "open",
              labels: [
                {
                  id: 1,
                  name: "kind/bug",
                  color: "d1242f",
                  description: "Something is not working",
                  version: 1,
                  created_at: 1_700_000_000_000,
                  updated_at: 1_700_000_000_000,
                },
              ],
              assignees: [{ subject: "alice", name: "Alice" }],
              version: 1,
              created_at: 1_700_000_000_000,
              updated_at: 1_700_000_000_000,
              can_edit: true,
              can_label: true,
              can_assign: true,
            },
          ],
          next: null,
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
  const readmePanel = page.getByRole("region", { name: "README.md" });
  await expect(readmePanel).toBeVisible();
  await expect(
    readmePanel.getByRole("heading", { name: "Team project" }),
  ).toBeVisible();
  await expect(
    readmePanel.getByRole("link", { name: "source entry" }),
  ).toHaveAttribute(
    "href",
    `/team/project?rev=${oid}&path=${pathHex("src/index.ts")}&kind=Blob`,
  );
  await expect(
    readmePanel.getByRole("img", { name: "Architecture" }),
  ).toHaveAttribute(
    "src",
    `/api/repos/team/project/asset?rev=${oid}&path_hex=${pathHex("docs/architecture.png")}`,
  );
  await expect(
    readmePanel.getByRole("link", { name: "Vector" }),
  ).toHaveAttribute(
    "href",
    `/api/repos/team/project/blob?rev=${oid}&path_hex=${pathHex("docs/vector.svg")}`,
  );
  await expect(
    readmePanel.getByRole("link", { name: "Build status" }),
  ).toHaveAttribute("href", "https://status.example/build.svg");
  await expect(
    readmePanel.getByRole("img", { name: "Build status" }),
  ).toHaveCount(0);
  await expect(readmePanel.locator(".repository-readme-body")).toHaveCSS(
    "padding",
    "32px",
  );
  await panel.getByRole("link", { name: "README.md", exact: true }).click();
  await expect(page.locator(".tree-sidebar")).toBeVisible();
  await expect(page.locator(".breadcrumb")).toHaveText("project/README.md");
  await expect(page.locator(".file-panel")).toBeVisible();
  await expect(page.locator(".latest-commit")).toContainText(
    "Update this path",
  );
  await expect(page.locator(".tree-sidebar")).toHaveCSS("width", "356px");
  await expect(page.getByPlaceholder("Go to file")).toBeVisible();
  await page
    .getByRole("button", { name: "Close file tree", exact: true })
    .click();
  await expect(page.locator(".tree-sidebar")).toHaveCount(0);
  await expect(
    page.getByRole("button", { name: "Open file tree", exact: true }),
  ).toBeVisible();
  await expect(page.locator(".file-navigation .breadcrumb")).toHaveText(
    "project/README.md",
  );
  await page.keyboard.press("t");
  await expect(page.locator(".tree-sidebar")).toBeVisible();
  await expect(page.getByPlaceholder("Go to file")).toBeFocused();
  await page.getByPlaceholder("Go to file").fill("README");
  await expect
    .poll(() =>
      page
        .locator('[aria-label="Repository files"]')
        .evaluate((tree) =>
          [
            ...(tree.shadowRoot?.querySelectorAll('[role="treeitem"]') ?? []),
          ].map((row) => row.getAttribute("aria-label")),
        ),
    )
    .toEqual(["README.md"]);
  await page.getByRole("link", { name: "History", exact: true }).click();
  await expect(page).toHaveURL(
    new RegExp(`view=commits.*path=${pathHex("README.md")}.*kind=Blob`),
  );
  await expect(page.getByRole("heading", { name: "Commits" })).toBeVisible();
  await expect(
    page.getByRole("link", { name: "README.md", exact: true }),
  ).toHaveAttribute(
    "href",
    `/team/project?rev=${oid}&path=${pathHex("README.md")}&kind=Blob`,
  );
  await expect(page.locator(".commit-list")).toContainText("Update this path");
  await expect(page.getByRole("button", { name: "Newer" })).toBeDisabled();
  await page.getByRole("button", { name: "Older" }).click();
  await expect(page.locator(".commit-list")).toContainText("Add this path");
  await expect(page.getByRole("button", { name: "Older" })).toBeDisabled();
  await page.getByRole("button", { name: "Newer" }).click();
  await expect(page.locator(".commit-list")).toContainText("Update this path");
});

test("deep links expand the active path and select its file", async ({
  page,
}) => {
  await page.goto(`/team/project?path=${pathHex("src/index.ts")}&kind=Blob`);
  const state = await page
    .locator('[aria-label="Repository files"]')
    .evaluate((tree) => {
      const rows = [
        ...(tree.shadowRoot?.querySelectorAll('[role="treeitem"]') ?? []),
      ];
      const active = rows.find(
        (row) => row.getAttribute("aria-selected") === "true",
      );
      const folder = rows.find(
        (row) => row.getAttribute("data-item-type") === "folder",
      );
      const folderContent = folder?.querySelector(
        '[data-item-section="content"]',
      );
      const selectedStyle = active ? getComputedStyle(active) : null;
      const selectedRail = active ? getComputedStyle(active, "::after") : null;
      const tokenProbe = document.createElement("span");
      tokenProbe.style.backgroundColor =
        "var(--control-transparent-bgColor-hover)";
      tokenProbe.style.color = "var(--fgColor-accent)";
      tree.closest(".app-shell")?.append(tokenProbe);
      const neutralBackground = getComputedStyle(tokenProbe).backgroundColor;
      const accentColor = getComputedStyle(tokenProbe).color;
      tokenProbe.remove();
      return {
        active: active?.getAttribute("aria-label"),
        activeHeight: active?.getBoundingClientRect().height,
        activeBackground: selectedStyle?.backgroundColor,
        activeRailBackground: selectedRail?.backgroundColor,
        activeRailLeft: selectedRail?.left,
        activeRailWidth: selectedRail?.width,
        neutralBackground,
        accentColor,
        expanded: rows
          .filter((row) => row.getAttribute("aria-expanded") === "true")
          .map((row) => row.getAttribute("aria-label")),
        folderIconWidth: folderContent
          ? getComputedStyle(folderContent, "::before").width
          : null,
      };
    });
  expect(state.active).toBe("index.ts");
  expect(state.activeHeight).toBe(32);
  expect(state.activeBackground).toBe(state.neutralBackground);
  expect(state.activeRailBackground).toBe(state.accentColor);
  expect(state.activeRailLeft).toBe("-16px");
  expect(state.activeRailWidth).toBe("3px");
  expect(state.expanded).toContain("src");
  expect(state.folderIconWidth).toBe("16px");
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

test("issues follow the GitHub list hierarchy in both themes and on mobile", async ({
  page,
}) => {
  await page.goto("/team/project?view=issues");
  const navigation = page.getByRole("complementary", {
    name: "Issue navigation",
  });
  await expect(
    navigation.getByRole("link", { name: "Issues", exact: true }),
  ).toHaveAttribute("aria-current", "page");
  await expect(page.getByRole("heading", { name: "All issues" })).toBeVisible();
  await expect(page.getByPlaceholder("Search all issues")).toBeVisible();
  await expect(page.locator(".issue-list-panel")).toContainText(
    "Keep object storage reads bounded",
  );
  await expect(page.getByText("kind/bug", { exact: true })).toBeVisible();

  await page.getByRole("link", { name: "Closed", exact: true }).click();
  await expect(page).toHaveURL(/state=closed/);
  await expect(
    page.getByRole("link", { name: "Closed", exact: true }),
  ).toHaveAttribute("aria-current", "page");

  for (const [width, theme] of [
    [1440, "dark"],
    [390, "light"],
  ] as const) {
    await page.setViewportSize({ width, height: 900 });
    await page.getByLabel("Appearance", { exact: true }).selectOption(theme);
    const geometry = await page.evaluate(() => ({
      viewport: innerWidth,
      page: document.documentElement.scrollWidth,
    }));
    expect(geometry.page).toBeLessThanOrEqual(geometry.viewport);
    await expect(navigation).toBeVisible();
    await expect(page.locator(".issue-list-panel")).toBeVisible();
  }
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
