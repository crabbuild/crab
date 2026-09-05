import { expect, test, type Page } from "@playwright/test";
import { expectNoAccessibilityViolations } from "./accessibility";

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

async function selectTheme(page: Page, theme: "light" | "dark") {
  await page.getByRole("button", { name: "Appearance", exact: true }).click();
  await page
    .getByRole("menuitemradio", {
      name: theme === "light" ? "Light" : "Dark",
      exact: true,
    })
    .click();
}

test.beforeEach(async ({ page }) => {
  let created = false;
  let createdBranch: string | null = null;
  let createdBranchHead = oid;
  let deleted = false;
  let currentHead = oid;
  let currentReadme = readme;
  let currentReadmeOid = oid;
  let uploadedFiles: string[] = [];
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
              protected_branches: page.url().includes("scenario=protected")
                ? [
                    {
                      branch: "main",
                      required_approvals: 1,
                      required_checks: [],
                    },
                  ]
                : [],
            },
          ],
        },
      });
    if (url.pathname.endsWith("/uploads")) {
      const body = route.request().postDataJSON() as {
        branch: string;
        expected_head: string;
        files: { path_hex: string; content_base64: string }[];
        message: string;
      };
      expect(route.request().method()).toBe("POST");
      expect(body).toEqual({
        branch: "refs/heads/main",
        expected_head: oid,
        files: [
          {
            path_hex: pathHex("notes.txt"),
            content_base64: "YWxwaGEK",
          },
          {
            path_hex: pathHex("raw.bin"),
            content_base64: "AP8KgA==",
          },
        ],
        message: "Upload repository files",
      });
      uploadedFiles = ["notes.txt", "raw.bin"];
      currentHead = "2".repeat(40);
      return route.fulfill({
        status: 201,
        json: {
          branch: "refs/heads/main",
          commit: currentHead,
          paths_hex: body.files.map((file) => file.path_hex),
        },
      });
    }
    if (url.pathname.endsWith("/contents")) {
      const body = route.request().postDataJSON() as {
        branch: string;
        expected_head: string;
        new_branch?: string;
        expected_blob?: string;
        path_hex: string;
        content?: string;
        message: string;
      };
      const method = route.request().method();
      if (body.new_branch) {
        expect(method).toBe("PATCH");
        expect(body).toEqual({
          branch: "refs/heads/main",
          expected_head: oid,
          new_branch: "docs/readme-review",
          expected_blob: oid,
          path_hex: pathHex("README.md"),
          content: "# Proposed in Crab\n",
          message: "Propose README update",
        });
        createdBranch = `refs/heads/${body.new_branch}`;
        createdBranchHead = "3".repeat(40);
        return route.fulfill({
          status: 200,
          json: {
            branch: createdBranch,
            commit: createdBranchHead,
            path_hex: body.path_hex,
          },
        });
      }
      if (method === "POST") {
        expect(body).toEqual({
          branch: "refs/heads/main",
          expected_head: oid,
          path_hex: pathHex("NEW.md"),
          content: "Created from Crab\n",
          message: "Create NEW.md",
        });
        created = true;
      } else if (method === "PATCH") {
        expect(body).toEqual({
          branch: "refs/heads/main",
          expected_head: oid,
          expected_blob: oid,
          path_hex: pathHex("README.md"),
          content: "# Edited in Crab\n",
          message: "Update README",
        });
        currentHead = "e".repeat(40);
        currentReadmeOid = "f".repeat(40);
        currentReadme = body.content ?? "";
      } else {
        expect(method).toBe("DELETE");
        expect(body).toEqual({
          branch: "refs/heads/main",
          expected_head: "e".repeat(40),
          expected_blob: "f".repeat(40),
          path_hex: pathHex("README.md"),
          message: "Delete README",
        });
        currentHead = "1".repeat(40);
        deleted = true;
      }
      return route.fulfill({
        status: method === "POST" ? 201 : 200,
        json: {
          branch: "refs/heads/main",
          commit: currentHead,
          path_hex: body.path_hex,
        },
      });
    }
    if (url.pathname.endsWith("/branches")) {
      const body = route.request().postDataJSON() as {
        name: string;
        source_oid: string;
      };
      expect(route.request().method()).toBe("POST");
      if (body.name === "existing")
        return route.fulfill({
          status: 409,
          json: {
            error: {
              code: "branch_exists",
              message: "A branch with this name already exists",
            },
          },
        });
      createdBranch = `refs/heads/${body.name}`;
      createdBranchHead = body.source_oid;
      return route.fulfill({
        status: 201,
        json: { branch: createdBranch, commit: body.source_oid },
      });
    }
    if (url.pathname.endsWith("/refs"))
      return route.fulfill({
        json: {
          head: { name: "refs/heads/main", oid: currentHead },
          refs: [
            { name: "refs/heads/main", oid: currentHead },
            ...(createdBranch
              ? [{ name: createdBranch, oid: createdBranchHead }]
              : []),
          ],
          generation: 1,
        },
      });
    if (url.pathname.endsWith("/commit")) {
      if (
        route.request().headers()["x-test-latest-commit"] === "fail-once" &&
        url.searchParams.get("path_hex") === pathHex("src/index.ts")
      )
        return route.fulfill({
          status: 422,
          json: {
            error: {
              message: "This request exceeds the repository read budget",
            },
          },
        });
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
    }
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
    if (url.pathname.endsWith("/search")) {
      const query = url.searchParams.get("q")?.toLowerCase() ?? "";
      return route.fulfill({
        json: {
          items: [
            ...(!deleted ? ["README.md"] : []),
            "src/index.ts",
            ...(created ? ["NEW.md"] : []),
          ]
            .filter((path) => path.toLowerCase().includes(query))
            .map((path) => ({
              path,
              path_hex: pathHex(path),
              kind: "Blob",
              oid,
              mode: "100644",
            })),
          commit: currentHead,
          truncated: false,
        },
      });
    }
    if (url.pathname.endsWith("/tree"))
      return route.fulfill({
        json: {
          items: (url.searchParams.get("path_hex") === pathHex("src")
            ? [["src/index.ts", "Blob"]]
            : url.searchParams.get("path_hex")
              ? []
              : [
                  ["zeta.txt", "Blob"],
                  ...(!deleted ? [["README.md", "Blob"]] : []),
                  ["Beta", "Tree"],
                  ["file10.txt", "Blob"],
                  ["src", "Tree"],
                  ["alpha", "Tree"],
                  ["file2.txt", "Blob"],
                  ...(created ? [["NEW.md", "Blob"]] : []),
                  ...uploadedFiles.map((path) => [path, "Blob"]),
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
    if (url.pathname.endsWith("/file")) {
      const text =
        url.searchParams.get("path_hex") === pathHex("README.md")
          ? currentReadme
          : "Hello, team!";
      return route.fulfill({
        json: {
          oid:
            url.searchParams.get("path_hex") === pathHex("README.md")
              ? currentReadmeOid
              : oid,
          size: text.length,
          mode: "100644",
          classification: "OrdinaryGit",
          text,
        },
      });
    }
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

test("repository views pass automated WCAG A and AA checks", async ({
  page,
}) => {
  for (const theme of ["light", "dark"] as const) {
    await page.goto("/team/project");
    await selectTheme(page, theme);
    for (const location of [
      "/team/project",
      `/team/project?rev=refs%2Fheads%2Fmain&path=${pathHex("README.md")}&kind=Blob`,
      "/team/project?view=issues",
      "/team/project?view=branches",
    ]) {
      await page.goto(location);
      await page.waitForLoadState("networkidle");
      await expectNoAccessibilityViolations(page);
    }
  }
});

test("overview groups files with their commit and opens the tree when navigating", async ({
  context,
  page,
}) => {
  await context.grantPermissions(["clipboard-read", "clipboard-write"]);
  await page.goto("/team/project");
  await expect(
    page.getByRole("complementary", { name: "About this repository" }),
  ).toBeVisible();
  await expect(page.locator(".tree-sidebar")).toHaveCount(0);
  const panel = page.getByRole("region", { name: "Folders and files" });
  const toolbar = page.locator(".repo-overview .toolbar");
  await expect(toolbar).toHaveCSS("margin-bottom", "16px");
  await expect
    .poll(async () => {
      const toolbarBottom = await toolbar.evaluate(
        (node) => node.getBoundingClientRect().bottom,
      );
      const panelTop = await panel.evaluate(
        (node) => node.getBoundingClientRect().top,
      );
      return panelTop - toolbarBottom;
    })
    .toBeGreaterThanOrEqual(16);
  await expect(
    panel.getByText("Make the repository easier to browse"),
  ).toBeVisible();
  await expect
    .poll(() =>
      panel
        .locator("tbody tr td:first-child a")
        .allTextContents()
        .then((names) => names.map((name) => name.trim())),
    )
    .toEqual([
      "alpha",
      "Beta",
      "src",
      "file2.txt",
      "file10.txt",
      "README.md",
      "zeta.txt",
    ]);
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
  await expect(page).toHaveURL(/rev=refs%2Fheads%2Fmain/);
  await expect(page.locator(".tree-sidebar")).toBeVisible();
  await expect(page.locator(".breadcrumb")).toContainText("project/README.md");
  await page.getByRole("button", { name: "Copy path", exact: true }).click();
  await expect(
    page.getByRole("button", { name: "Path copied", exact: true }),
  ).toBeVisible();
  await expect
    .poll(() => page.evaluate(() => navigator.clipboard.readText()))
    .toBe("README.md");
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
  await expect(page.locator(".file-navigation .breadcrumb")).toContainText(
    "project/README.md",
  );
  await page.keyboard.press("t");
  await expect(page.locator(".tree-sidebar")).toBeVisible();
  await expect(page.getByPlaceholder("Go to file")).toBeFocused();
  await page.getByPlaceholder("Go to file").fill("README");
  await expect(
    page
      .getByLabel("Repository file search results")
      .getByRole("option", { name: "README.md" }),
  ).toBeVisible();
  await page.getByPlaceholder("Go to file").press("Escape");
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

test("Markdown files switch between source and a repository-aware preview", async ({
  page,
}) => {
  await page.goto(
    `/team/project?rev=refs%2Fheads%2Fmain&path=${pathHex("README.md")}&kind=Blob`,
  );
  await page.getByRole("button", { name: "Preview", exact: true }).click();
  const preview = page.locator(".file-markdown-preview");
  await expect(
    preview.getByRole("heading", { name: "Team project" }),
  ).toBeVisible();
  await expect(
    preview.getByRole("link", { name: "source entry" }),
  ).toHaveAttribute(
    "href",
    `/team/project?rev=${oid}&path=${pathHex("src/index.ts")}&kind=Blob`,
  );
  await expect(
    preview.getByRole("img", { name: "Architecture" }),
  ).toHaveAttribute(
    "src",
    `/api/repos/team/project/asset?rev=${oid}&path_hex=${pathHex("docs/architecture.png")}`,
  );
  await page.getByRole("button", { name: "Code", exact: true }).click();
  await expect(preview).toHaveCount(0);
});

test("Go to file finds a deep repository path before its directory is expanded", async ({
  page,
}) => {
  await page.setExtraHTTPHeaders({ "x-test-latest-commit": "fail-once" });
  await page.goto("/team/project");
  await expect(
    page.getByRole("button", { name: "Browse files", exact: true }),
  ).toBeVisible();
  await page.keyboard.press("t");
  await expect(page.getByPlaceholder("Go to file")).toBeFocused();
  await page.getByPlaceholder("Go to file").fill("index");
  const result = page
    .getByLabel("Repository file search results")
    .getByRole("option", { name: "src/index.ts" });
  await expect(result).toBeVisible();
  await page.getByPlaceholder("Go to file").press("Enter");
  await expect(page).toHaveURL(
    new RegExp(`path=${pathHex("src/index.ts")}.*kind=Blob`),
  );
  await expect(page.getByPlaceholder("Go to file")).toHaveValue("");
  await expect(page.getByLabel("Repository files")).toBeVisible();
  await expect(page.locator(".breadcrumb")).toContainText(
    "project/src/index.ts",
  );
  const latestCommit = page.locator(".latest-commit-error");
  await expect(latestCommit).toContainText("Latest commit unavailable");
  await expect(latestCommit).toContainText("repository read budget");
  await expect(page.locator(".file-panel")).toBeVisible();
  expect((await latestCommit.boundingBox())?.height).toBeLessThanOrEqual(60);
  await page.setExtraHTTPHeaders({});
  await page.getByRole("button", { name: "Retry latest commit" }).click();
  await expect(latestCommit).toHaveCount(0);
  await expect(page.locator(".latest-commit")).toContainText(
    "Update this path",
  );
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

test("one directory click selects, expands, and loads its children", async ({
  page,
}) => {
  const treeRequests: string[] = [];
  page.on("request", (request) => {
    if (new URL(request.url()).pathname.endsWith("/tree"))
      treeRequests.push(request.url());
  });
  await page.goto(
    `/team/project?rev=${oid}&path=${pathHex("README.md")}&kind=Blob`,
  );
  const tree = page.locator('[aria-label="Repository files"]');
  const directory = tree.getByRole("treeitem", {
    name: "src",
    exact: true,
  });
  await expect(directory).toHaveAttribute("aria-expanded", "false");
  await page.waitForLoadState("networkidle");
  treeRequests.length = 0;

  await directory.click();

  await expect(page).toHaveURL(new RegExp(`path=${pathHex("src")}.*kind=Tree`));
  await expect(directory).toHaveAttribute("aria-selected", "true");
  await expect(directory).toHaveAttribute("aria-expanded", "true");
  await expect(
    tree.getByRole("treeitem", { name: "index.ts", exact: true }),
  ).toBeVisible();
  expect(
    treeRequests.filter((url) => !new URL(url).searchParams.get("path_hex")),
  ).toHaveLength(0);

  await directory.click();
  await expect(directory).toHaveAttribute("aria-expanded", "false");
  await expect(
    tree.getByRole("treeitem", { name: "index.ts", exact: true }),
  ).toHaveCount(0);
});

test("file-tree add menu commits a file and keeps GitHub control spacing", async ({
  context,
  page,
}) => {
  await context.grantPermissions(["clipboard-read", "clipboard-write"]);
  await page.goto(`/team/project?path=${pathHex("README.md")}&kind=Blob`);
  const add = page.getByRole("button", {
    name: "Add file",
    exact: true,
  });
  await expect(add).toBeVisible();
  await expect(add).toHaveCSS("width", "32px");
  await expect(add).toHaveCSS("height", "32px");
  await add.click();
  await page.getByRole("menuitem", { name: "Create new file" }).click();
  await expect(
    page.getByRole("heading", { name: "Create new file" }),
  ).toBeVisible();
  await expect(page.getByLabel("File name")).toBeFocused();
  await page.setViewportSize({ width: 360, height: 800 });
  expect(
    await page.evaluate(() => document.documentElement.scrollWidth),
  ).toBeLessThanOrEqual(360);
  await page.getByLabel("File name").fill("NEW.md");
  await page.getByLabel("File content").fill("Created from Crab\n");
  await page.getByLabel("Commit message").fill("Create NEW.md");
  await page.getByRole("button", { name: "Commit changes" }).click();
  await expect(page).toHaveURL(
    new RegExp(`rev=refs%2Fheads%2Fmain.*path=${pathHex("NEW.md")}`),
  );
  await expect(page.locator(".breadcrumb")).toContainText("project/NEW.md");
  await page.getByRole("button", { name: "Copy file contents" }).click();
  await expect
    .poll(() => page.evaluate(() => navigator.clipboard.readText()))
    .toBe("Hello, team!");
});

test("file-tree add menu uploads text and binary files in one commit", async ({
  page,
}) => {
  await page.goto(`/team/project?path=${pathHex("README.md")}&kind=Blob`);
  await page.getByRole("button", { name: "Add file" }).click();
  await page.getByRole("menuitem", { name: "Upload files" }).click();
  await expect(
    page.getByRole("heading", { name: "Upload files" }),
  ).toBeVisible();
  await page.getByLabel("Choose files to upload").evaluate((element) => {
    const transfer = new DataTransfer();
    transfer.items.add(
      new File(["alpha\n"], "notes.txt", { type: "text/plain" }),
    );
    transfer.items.add(
      new File([new Uint8Array([0, 255, 10, 128])], "raw.bin", {
        type: "application/octet-stream",
      }),
    );
    const input = element as HTMLInputElement;
    input.files = transfer.files;
    input.dispatchEvent(new Event("change", { bubbles: true }));
  });
  const files = page.getByRole("list", { name: "Files to upload" });
  await expect(files).toContainText("notes.txt");
  await expect(files).toContainText("raw.bin");
  await page.setViewportSize({ width: 360, height: 800 });
  expect(
    await page.evaluate(() => document.documentElement.scrollWidth),
  ).toBeLessThanOrEqual(360);
  await page.getByLabel("Commit message").fill("Upload repository files");
  await page.getByRole("button", { name: "Commit changes" }).click();
  await expect(page).toHaveURL(/rev=refs%2Fheads%2Fmain/);
  await expect(
    page.getByRole("region", { name: "Folders and files" }),
  ).toContainText("raw.bin");
});

test("protected file edits create a review branch and open the exact comparison", async ({
  page,
}) => {
  await page.goto(
    `/team/project?rev=refs%2Fheads%2Fmain&path=${pathHex("README.md")}&kind=Blob&view=edit&scenario=protected`,
  );
  await expect(
    page.getByRole("heading", { name: "Editing README.md" }),
  ).toBeVisible();
  const direct = page.getByRole("radio", {
    name: /Commit directly to main/,
  });
  await expect(direct).toBeDisabled();
  await expect(
    page.getByRole("radio", {
      name: /Create a new branch for this commit/,
    }),
  ).toBeChecked();
  await page.getByLabel("File content").fill("# Proposed in Crab\n");
  await page.getByLabel("Commit message").fill("Propose README update");
  await page.getByLabel("New branch name").fill("docs/readme-review");
  await page.getByRole("button", { name: "Propose changes" }).click();
  await expect(page).toHaveURL(/view=pulls&pull=new/);
  await expect(page.locator(".compare-picker select").nth(0)).toHaveValue(
    "refs/heads/main",
  );
  await expect(page.locator(".compare-picker select").nth(1)).toHaveValue(
    "refs/heads/docs/readme-review",
  );
  await expect(page.getByLabel("Title", { exact: true })).toHaveValue(
    "Propose README update",
  );
});

test("branch file actions edit and delete through reviewable commit pages", async ({
  context,
  page,
}) => {
  await context.grantPermissions(["clipboard-read", "clipboard-write"]);
  await page.goto(
    `/team/project?rev=refs%2Fheads%2Fmain&path=${pathHex("README.md")}&kind=Blob`,
  );
  const edit = page.getByRole("button", {
    name: "Edit this file",
    exact: true,
  });
  const remove = page.getByRole("button", {
    name: "Delete this file",
    exact: true,
  });
  await expect(edit).toBeVisible();
  await expect(remove).toBeVisible();
  await expect(edit).toHaveCSS("width", "34px");
  await expect(remove).toHaveCSS("height", "32px");

  await edit.click();
  await expect(
    page.getByRole("heading", { name: "Editing README.md" }),
  ).toBeVisible();
  await expect(page.getByLabel("File content")).toBeFocused();
  await page.getByLabel("File content").fill("# Edited in Crab\n");
  await page.getByLabel("Commit message").fill("Update README");
  await page.getByRole("button", { name: "Commit changes" }).click();
  await page.getByRole("button", { name: "Copy file contents" }).click();
  await expect
    .poll(() => page.evaluate(() => navigator.clipboard.readText()))
    .toBe("# Edited in Crab\n");

  await page
    .getByRole("button", { name: "Delete this file", exact: true })
    .click();
  await expect(
    page.getByRole("heading", { name: "Delete README.md" }),
  ).toBeVisible();
  await expect(page.locator(".delete-file-summary")).toContainText(
    "remain available in the repository history",
  );
  await page.getByLabel("Commit message").fill("Delete README");
  await page.getByRole("button", { name: "Commit changes" }).click();
  await expect(page).toHaveURL("/team/project?rev=refs%2Fheads%2Fmain");
  await expect(
    page.getByRole("region", { name: "Folders and files" }),
  ).not.toContainText("README.md");
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
    await selectTheme(page, theme);
    await page.locator(".clone-menu summary").click();
    await expect(
      page.getByLabel("Repository URL", { exact: true }),
    ).toHaveValue("http://127.0.0.1:5175/git/team/project.git");
    await expect(
      page.getByRole("button", { name: "Copy URL", exact: true }),
    ).toBeVisible();
    await expect(
      page.getByRole("link", { name: "Download ZIP" }),
    ).toHaveAttribute("href", `/api/repos/team/project/archive?rev=${oid}`);
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
  await expect(
    page.getByRole("button", { name: "Appearance", exact: true }),
  ).toContainText("Light");
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
    await selectTheme(page, theme);
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
  for (const scheme of ["light", "dark"] as const) {
    await selectTheme(page, scheme);
    await expect(page.locator("html")).toHaveCSS("color-scheme", scheme);
  }
});

test("branch and tag pages follow the GitHub refs hierarchy", async ({
  page,
}) => {
  let alphaDeleted = false;
  let deleteAttempts = 0;
  await page.context().grantPermissions(["clipboard-read", "clipboard-write"]);
  await page.route(
    (url) => url.pathname === "/api/repos",
    (route) =>
      route.fulfill({
        json: {
          repositories: [
            {
              owner: "team",
              name: "project",
              description: "A repository for our team.",
              access: "write",
              protected_branches: [
                {
                  branch: "main",
                  required_approvals: 1,
                  required_checks: [],
                },
                {
                  branch: "feature/docs",
                  required_approvals: 1,
                  required_checks: [],
                },
              ],
            },
          ],
        },
      }),
  );
  await page.route(
    (url) => url.pathname === "/api/repos/team/project/refs",
    (route) =>
      route.fulfill({
        json: {
          head: { name: "refs/heads/main", oid },
          unborn_head: null,
          refs: [
            { name: "refs/heads/main", oid },
            { name: "refs/heads/feature/docs", oid: "c".repeat(40) },
            ...(!alphaDeleted
              ? [{ name: "refs/heads/alpha", oid: "b".repeat(40) }]
              : []),
            {
              name: "refs/tags/v1.0",
              oid: "d".repeat(40),
              peeled: "e".repeat(40),
            },
          ],
          generation: 1,
        },
      }),
  );
  await page.route(
    (url) => url.pathname === "/api/repos/team/project/branches",
    (route) => {
      expect(route.request().method()).toBe("DELETE");
      expect(route.request().postDataJSON()).toEqual({
        name: "alpha",
        expected_oid: "b".repeat(40),
      });
      deleteAttempts += 1;
      if (deleteAttempts === 1)
        return route.fulfill({
          status: 409,
          json: {
            error: {
              code: "branch_changed",
              message:
                "The branch changed or was already deleted; reload before retrying",
            },
          },
        });
      alphaDeleted = true;
      return route.fulfill({
        json: {
          branch: "refs/heads/alpha",
          deleted_oid: "b".repeat(40),
        },
      });
    },
  );
  await page.goto("/team/project?scenario=protected");
  await page.getByRole("link", { name: "3 branches", exact: true }).click();
  await expect(page).toHaveURL(/view=branches/);
  await expect(
    page.getByRole("link", { name: "Code", exact: true }),
  ).toHaveAttribute("aria-current", "page");
  await expect(
    page.getByRole("heading", { name: "Branches", level: 2 }),
  ).toBeVisible();
  const defaultGroup = page.getByRole("region", { name: "Default" });
  await expect(defaultGroup).toContainText("main");
  await expect(defaultGroup).toContainText("default");
  await expect(defaultGroup).toContainText("protected");
  const branches = page.getByRole("region", { name: "Branches" });
  await expect(branches.locator(".ref-name-cell > a")).toHaveText([
    "alpha",
    "feature/docs",
  ]);
  await branches.getByRole("button", { name: "Copy alpha" }).click();
  await expect(
    branches.getByRole("button", { name: "Copied alpha" }),
  ).toBeVisible();
  await expect(
    branches.getByRole("link", { name: "Compare", exact: true }).first(),
  ).toHaveAttribute(
    "href",
    "/team/project?view=pulls&pull=new&base=refs%2Fheads%2Fmain&head=refs%2Fheads%2Falpha",
  );
  await expect(
    defaultGroup.getByRole("button", { name: /^Delete / }),
  ).toHaveCount(0);
  await expect(
    branches.getByRole("button", { name: "Delete feature/docs" }),
  ).toHaveCount(0);
  await branches.getByRole("button", { name: "Delete alpha" }).click();
  await expect(branches.getByText("Delete alpha?")).toBeVisible();
  await branches.getByRole("button", { name: "Cancel" }).click();
  await expect(branches.getByText("Delete alpha?")).toHaveCount(0);
  await branches.getByRole("button", { name: "Delete alpha" }).click();
  await branches.getByRole("button", { name: "Delete branch" }).click();
  await expect(branches.getByRole("alert")).toHaveText(
    "The branch changed or was already deleted; reload before retrying",
  );
  await branches.getByRole("button", { name: "Delete branch" }).click();
  await expect(branches.getByRole("link", { name: "alpha" })).toHaveCount(0);
  expect(deleteAttempts).toBe(2);

  await page.getByRole("textbox", { name: "Search branches" }).fill("DOCS");
  await expect(page.locator(".ref-name-cell > a")).toHaveText(["feature/docs"]);
  await page
    .getByRole("navigation", { name: "Repository refs" })
    .getByRole("link", { name: "Tags", exact: true })
    .click();
  await expect(page).toHaveURL(/view=tags/);
  await expect(page.getByRole("region", { name: "Tags" })).toContainText(
    "v1.0",
  );
  await expect(page.locator(".ref-commit")).toHaveText("eeeeeee");

  await selectTheme(page, "dark");
  await page.setViewportSize({ width: 390, height: 800 });
  expect(
    await page.evaluate(() => document.documentElement.scrollWidth),
  ).toBeLessThanOrEqual(390);
});

test("revision picker creates a branch from the exact viewed commit", async ({
  page,
}) => {
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
  await search.fill("existing");
  await dialog
    .getByRole("button", {
      name: "Create branch: existing from 'main'",
      exact: true,
    })
    .click();
  await expect(dialog.getByRole("alert")).toHaveText(
    "A branch with this name already exists",
  );
  await expect(search).toHaveValue("existing");

  await search.fill("feature/browser");
  const request = page.waitForRequest(
    (candidate) =>
      candidate.url().endsWith("/api/repos/team/project/branches?") &&
      candidate.method() === "POST",
  );
  await dialog
    .getByRole("button", {
      name: "Create branch: feature/browser from 'main'",
      exact: true,
    })
    .click();
  expect((await request).postDataJSON()).toEqual({
    name: "feature/browser",
    source_oid: oid,
  });
  await expect(page).toHaveURL(/rev=refs%2Fheads%2Ffeature%2Fbrowser/);
  await expect(anchor).toHaveText("feature/browser");
  await anchor.click();
  await expect(
    dialog.getByRole("menuitemradio", {
      name: "feature/browser",
      exact: true,
    }),
  ).toHaveAttribute("aria-checked", "true");
});
