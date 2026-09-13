import { expect, test, type Page } from "@playwright/test";
import { tableFromArrays, tableToIPC } from "apache-arrow";
import { strToU8, zipSync } from "fflate";
import { parquetWriteBuffer } from "hyparquet-writer";
import initSqlJs from "sql.js";
import {
  expectNoAccessibilityViolations,
  selectDarkTheme,
} from "./accessibility";

const oid = "a".repeat(40);
const pathOid = "b".repeat(40);
const pathParent = "c".repeat(40);
const addedPathOid = "d".repeat(40);
const readme =
  "# Team project\n\nBrowse the [source entry](src/index.ts) without cloning.\n\n" +
  "![Architecture](docs/architecture.png) ![Vector](docs/vector.svg) " +
  "![Build status](https://status.example/build.svg)\n\n" +
  '```typescript\nconst project: string = "Crab";\n```\n';
const pathHex = (path: string) =>
  Array.from(new TextEncoder().encode(path), (byte) =>
    byte.toString(16).padStart(2, "0"),
  ).join("");

function pdfBytes(text: string) {
  const stream = `BT /F1 20 Tf 72 720 Td (${text}) Tj ET`;
  const objects = [
    "<< /Type /Catalog /Pages 2 0 R >>",
    "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
    "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>",
    `<< /Length ${Buffer.byteLength(stream)} >>\nstream\n${stream}\nendstream`,
    "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>",
  ];
  let value = "%PDF-1.4\n";
  const offsets = [0];
  for (const [index, object] of objects.entries()) {
    offsets.push(Buffer.byteLength(value));
    value += `${index + 1} 0 obj\n${object}\nendobj\n`;
  }
  const xref = Buffer.byteLength(value);
  value += `xref\n0 ${objects.length + 1}\n0000000000 65535 f \n`;
  value += offsets
    .slice(1)
    .map((offset) => `${String(offset).padStart(10, "0")} 00000 n \n`)
    .join("");
  value += `trailer\n<< /Size ${objects.length + 1} /Root 1 0 R >>\nstartxref\n${xref}\n%%EOF\n`;
  return strToU8(value);
}

async function routePreviewFiles(
  page: Page,
  files: Record<
    string,
    {
      text: string | null;
      bytes: Uint8Array;
      size?: number;
      textTruncated?: boolean;
    }
  >,
) {
  await page.route("**/api/repos/team/project/file?*", async (route) => {
    const path = new URL(route.request().url()).searchParams.get("path_hex");
    const file = Object.entries(files).find(
      ([name]) => pathHex(name) === path,
    )?.[1];
    if (!file) return route.fallback();
    return route.fulfill({
      json: {
        oid,
        size: file.size ?? file.bytes.byteLength,
        mode: "100644",
        classification: "OrdinaryGit",
        text: file.text,
        text_truncated: file.textTruncated ?? false,
      },
    });
  });
  await page.route("**/api/repos/team/project/blob?*", async (route) => {
    const path = new URL(route.request().url()).searchParams.get("path_hex");
    const file = Object.entries(files).find(
      ([name]) => pathHex(name) === path,
    )?.[1];
    if (!file) return route.fallback();
    const range = route.request().headers()["range"];
    if (range) {
      const match = /^bytes=(\d+)-(\d*)$/.exec(range);
      if (!match) return route.fulfill({ status: 416 });
      const start = Number(match[1]);
      const end = Math.min(
        match[2] ? Number(match[2]) + 1 : file.bytes.byteLength,
        file.bytes.byteLength,
      );
      return route.fulfill({
        status: 206,
        body: Buffer.from(file.bytes.slice(start, end)),
        contentType: "application/octet-stream",
        headers: {
          "accept-ranges": "bytes",
          "content-range": `bytes ${start}-${end - 1}/${file.bytes.byteLength}`,
        },
      });
    }
    return route.fulfill({
      body:
        route.request().method() === "HEAD"
          ? undefined
          : Buffer.from(file.bytes),
      contentType: "application/octet-stream",
      headers: {
        "accept-ranges": "bytes",
        "content-length": String(file.bytes.byteLength),
      },
    });
  });
}

async function selectTheme(page: Page, theme: "light" | "dark") {
  await page
    .locator(`[aria-label="${theme === "light" ? "Light" : "Dark"}"]`)
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
  let protectionVersion = 0;
  let archiveVersion = 0;
  let archived = false;
  let protectionRules: Array<{
    branch: string;
    required_approvals: number;
    required_checks: string[];
  }> = [];
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
              can_admin: true,
              archive_version: archiveVersion,
              archived,
              protection_version: protectionVersion,
              protected_branches: page.url().includes("scenario=protected")
                ? [
                    {
                      branch: "main",
                      required_approvals: 1,
                      required_checks: [],
                    },
                  ]
                : protectionRules,
            },
          ],
        },
      });
    if (url.pathname.endsWith("/settings/archive")) {
      expect(route.request().method()).toBe("PUT");
      const body = route.request().postDataJSON() as {
        expected_version: number;
        archived: boolean;
        repository: string;
      };
      if (body.expected_version !== archiveVersion)
        return route.fulfill({
          status: 409,
          json: {
            error: {
              code: "settings_changed",
              message: "Repository settings changed; reload before saving",
            },
          },
        });
      expect(body.repository).toBe("team/project");
      archiveVersion += 1;
      archived = body.archived;
      return route.fulfill({ json: { version: archiveVersion, archived } });
    }
    if (url.pathname.endsWith("/settings/branch-protections")) {
      expect(route.request().method()).toBe("PUT");
      const body = route.request().postDataJSON() as {
        expected_version: number;
        rules: typeof protectionRules;
      };
      if (body.expected_version !== protectionVersion)
        return route.fulfill({
          status: 409,
          json: {
            error: {
              code: "settings_changed",
              message:
                "Branch protection settings changed; reload before saving",
            },
          },
        });
      protectionVersion += 1;
      protectionRules = body.rules;
      return route.fulfill({
        json: { version: protectionVersion, rules: protectionRules },
      });
    }
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
            ...(url.searchParams.get("last_commit") === "true"
              ? {
                  last_commit: {
                    oid,
                    author: "Alice",
                    author_seconds: 1_700_000_000,
                    message: `Update ${path}`,
                  },
                }
              : {}),
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
          text_truncated: false,
        },
      });
    }
    if (url.pathname.endsWith("/blame"))
      return route.fulfill({
        json: {
          ranges: [
            {
              start: 1,
              lines: 2,
              commit: {
                oid: "1".repeat(40),
                tree: "2".repeat(40),
                parents: [],
                author: "Alice",
                author_seconds: 1_690_000_000,
                message: "Start the project documentation",
              },
            },
            {
              start: 3,
              lines: 3,
              commit: {
                oid: "3".repeat(40),
                tree: "4".repeat(40),
                parents: ["1".repeat(40)],
                author: "Bob",
                author_seconds: 1_700_000_000,
                message: "Explain repository navigation",
              },
            },
          ],
        },
      });
    if (url.pathname.endsWith("/changes"))
      return route.fulfill({
        json: {
          base: null,
          commit: oid,
          changes: [
            {
              path: "src/index.ts",
              path_hex: pathHex("src/index.ts"),
              kind: "Modified",
              old: {
                path: "src/index.ts",
                path_hex: pathHex("src/index.ts"),
                kind: "Blob",
                oid: "1".repeat(40),
                mode: "100644",
              },
              new: {
                path: "src/index.ts",
                path_hex: pathHex("src/index.ts"),
                kind: "Blob",
                oid: "2".repeat(40),
                mode: "100644",
              },
            },
            {
              path: "src/lib/new.ts",
              path_hex: pathHex("src/lib/new.ts"),
              kind: "Added",
              old: null,
              new: {
                path: "src/lib/new.ts",
                path_hex: pathHex("src/lib/new.ts"),
                kind: "Blob",
                oid: "3".repeat(40),
                mode: "100644",
              },
            },
            {
              path: "docs/old.md",
              path_hex: pathHex("docs/old.md"),
              kind: "Deleted",
              old: {
                path: "docs/old.md",
                path_hex: pathHex("docs/old.md"),
                kind: "Blob",
                oid: "4".repeat(40),
                mode: "100644",
              },
              new: null,
            },
          ],
        },
      });
    if (url.pathname.endsWith("/diff")) {
      const path = ["src/index.ts", "src/lib/new.ts", "docs/old.md"].find(
        (candidate) => pathHex(candidate) === url.searchParams.get("path_hex"),
      );
      if (!path) throw new Error("Unexpected changed-file fixture path");
      const added = path === "src/lib/new.ts";
      const deleted = path === "docs/old.md";
      return route.fulfill({
        json: {
          base: null,
          commit: oid,
          path,
          old: added
            ? null
            : {
                oid: "5".repeat(40),
                size: 12,
                mode: "100644",
                classification: "OrdinaryGit",
                text: "Old content\n",
              },
          new: deleted
            ? null
            : {
                oid: "6".repeat(40),
                size: 12,
                mode: "100644",
                classification: "OrdinaryGit",
                text: "New content\n",
              },
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

for (const theme of ["light", "dark"] as const) {
  test(`repository views pass automated WCAG A and AA checks in ${theme} theme`, async ({
    page,
  }) => {
    await page.goto("/team/project");
    await selectTheme(page, theme);
    for (const view of [
      {
        location: "/team/project",
        ready: page.getByRole("region", { name: "Folders and files" }),
      },
      {
        location: `/team/project?rev=refs%2Fheads%2Fmain&path=${pathHex("README.md")}&kind=Blob`,
        ready: page.locator(".file-panel"),
      },
      {
        location: `/team/project?view=commit&rev=${oid}`,
        ready: page.getByRole("heading", {
          name: "3 changed files",
          exact: true,
        }),
      },
      {
        location: "/team/project?view=issues",
        ready: page.getByRole("heading", { name: "All issues" }),
      },
      {
        location: "/team/project?view=issues&issue=new",
        ready: page.getByRole("heading", { name: "New issue", exact: true }),
      },
      {
        location: "/team/project?view=branches",
        ready: page.getByRole("heading", {
          name: "Branches",
          exact: true,
          level: 2,
        }),
      },
      {
        location: "/team/project?view=settings",
        ready: page.getByRole("heading", { name: "General", exact: true }),
      },
    ]) {
      await test.step(view.location, async () => {
        await page.goto(view.location);
        await expect(view.ready).toBeVisible();
        await expectNoAccessibilityViolations(page);
      });
    }
  });
}

test("new issue Markdown toolbar formats selections and remains usable on mobile", async ({
  page,
}) => {
  await page.goto("/team/project?view=issues&issue=new");
  const editor = page.locator(".discussion-editor");
  const toolbar = editor.getByRole("toolbar", {
    name: "Description formatting",
  });
  await expect(
    toolbar.getByRole("button", { name: "Add heading", exact: true }),
  ).toBeVisible();
  await expect(
    toolbar.getByRole("button", { name: "Add a task list", exact: true }),
  ).toBeVisible();
  await expect(
    toolbar.getByRole("button", { name: "Mention a user", exact: true }),
  ).toBeVisible();

  const description = editor.getByRole("textbox", {
    name: "Description",
    exact: true,
  });
  await description.fill("Ship safely");
  await description.evaluate((input: HTMLTextAreaElement) =>
    input.setSelectionRange(5, 11),
  );
  await toolbar
    .getByRole("button", { name: "Add bold text", exact: true })
    .click();
  await expect(description).toHaveValue("Ship **safely**");
  await expect(description).toBeFocused();
  await page.keyboard.press("ControlOrMeta+i");
  await expect(description).toHaveValue("Ship **_safely_**");

  await editor.getByRole("tab", { name: "Preview", exact: true }).click();
  await expect(toolbar).toBeHidden();
  await expect(
    editor
      .getByRole("tabpanel", { name: "Preview", exact: true })
      .locator("strong em"),
  ).toHaveText("safely");

  await editor.getByRole("tab", { name: "Write", exact: true }).click();
  await description.fill("test\nship");
  await description.selectText();
  await toolbar
    .getByRole("button", { name: "Add a task list", exact: true })
    .click();
  await expect(description).toHaveValue("- [ ] test\n- [ ] ship");

  await page.setViewportSize({ width: 320, height: 900 });
  await toolbar
    .getByRole("button", { name: "Mention a user", exact: true })
    .focus();
  const bounds = await toolbar.boundingBox();
  expect(bounds).not.toBeNull();
  expect(bounds!.x).toBeGreaterThanOrEqual(0);
  expect(bounds!.x + bounds!.width).toBeLessThanOrEqual(320);
  await expectNoAccessibilityViolations(page);
});

test("commit page keeps a sticky file tree beside independently scrolling diffs", async ({
  page,
}) => {
  await page.goto(`/team/project?view=commit&rev=${oid}`);

  await expect(
    page.getByRole("heading", { name: "3 changed files", exact: true }),
  ).toBeVisible();
  const tree = page.locator('file-tree-container[aria-label="Changed files"]');
  await expect(
    tree.getByRole("treeitem", { name: "docs", exact: true }),
  ).toHaveAttribute("aria-expanded", "true");
  await expect(
    tree.getByRole("treeitem", { name: "src", exact: true }),
  ).toHaveAttribute("aria-expanded", "true");
  await expect(
    tree.getByRole("treeitem", { name: "lib", exact: true }),
  ).toHaveAttribute("aria-expanded", "true");

  const modified = tree.getByRole("treeitem", {
    name: "index.ts",
    exact: true,
  });
  await expect(modified).toHaveAttribute("aria-selected", "true");
  const workspace = page.locator(".change-workspace");
  const diff = page.locator(".change-diff-pane");
  const panels = diff.locator(".diff-panel");
  await expect(panels).toHaveCount(3);
  await expect(
    diff.getByRole("heading", { name: "src/index.ts", exact: true }),
  ).toBeVisible();
  await expect(
    diff.getByRole("heading", { name: "src/lib/new.ts", exact: true }),
  ).toBeVisible();
  await expect(
    diff.getByRole("heading", { name: "docs/old.md", exact: true }),
  ).toBeVisible();
  await expect(panels.first()).toContainText("Modified");
  await expect(panels.first()).toContainText("New content");

  await expect(workspace).toHaveCSS("position", "sticky");
  await expect(diff).toHaveCSS("overflow-y", "auto");
  const treeScroll = tree.locator('[data-file-tree-virtualized-scroll="true"]');
  await expect(treeScroll).toHaveCSS("overflow-y", "auto");

  await workspace.evaluate((node) => {
    node.style.height = "300px";
    node.style.minHeight = "0";
  });
  await expect
    .poll(() => diff.evaluate((node) => node.scrollHeight > node.clientHeight))
    .toBe(true);

  const deleted = tree.getByRole("treeitem", { name: "old.md", exact: true });
  const deletedPanel = diff.locator(`#changed-file-${pathHex("docs/old.md")}`);
  await deleted.click();
  await expect(deleted).toHaveAttribute("aria-selected", "true");
  await expect
    .poll(() => diff.evaluate((node) => node.scrollTop))
    .toBeGreaterThan(0);
  await expect
    .poll(async () => {
      const paneTop = await diff.evaluate(
        (node) => node.getBoundingClientRect().top,
      );
      const paneBottom = await diff.evaluate(
        (node) => node.getBoundingClientRect().bottom,
      );
      const panelTop = await deletedPanel.evaluate(
        (node) => node.getBoundingClientRect().top,
      );
      const panelBottom = await deletedPanel.evaluate(
        (node) => node.getBoundingClientRect().bottom,
      );
      return panelTop >= paneTop && panelBottom <= paneBottom;
    })
    .toBe(true);
  await expect(panels.first()).toBeAttached();

  await expect
    .poll(async () => {
      const treeRight = await tree.evaluate(
        (node) => node.getBoundingClientRect().right,
      );
      const diffLeft = await diff.evaluate(
        (node) => node.getBoundingClientRect().left,
      );
      return diffLeft - treeRight;
    })
    .toBeGreaterThanOrEqual(0);

  await workspace.evaluate((node) => {
    node.style.removeProperty("height");
    node.style.removeProperty("min-height");
  });
  await page.setViewportSize({ width: 600, height: 900 });
  await expect(workspace).toHaveCSS("position", "static");
  await expect(diff).toHaveCSS("overflow-y", "visible");
  await expect
    .poll(async () => {
      const treeBottom = await tree.evaluate(
        (node) => node.getBoundingClientRect().bottom,
      );
      const diffTop = await diff.evaluate(
        (node) => node.getBoundingClientRect().top,
      );
      return diffTop - treeBottom;
    })
    .toBeGreaterThanOrEqual(0);
  await expectNoAccessibilityViolations(page);
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
  await expect(
    panel.getByRole("columnheader", { name: "Last commit" }),
  ).toBeVisible();
  const readmeRow = panel.getByRole("row").filter({ hasText: "README.md" });
  await expect(readmeRow.getByText("Update README.md")).toHaveAttribute(
    "href",
    `/team/project?view=commit&rev=${oid}`,
  );
  await expect(readmeRow.getByRole("time")).toHaveAttribute(
    "datetime",
    "2023-11-14T22:13:20.000Z",
  );
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
  await expect(preview.locator(".markdown-code-block")).toContainText(
    'const project: string = "Crab";',
  );
  await page.getByRole("button", { name: "Code", exact: true }).click();
  await expect(preview).toHaveCount(0);
});

test("code palette persists and follows light and dark appearance", async ({
  page,
}) => {
  await page.goto(
    `/team/project?rev=refs%2Fheads%2Fmain&path=${pathHex("README.md")}&kind=Blob`,
  );
  await page.getByRole("button", { name: "Preview", exact: true }).click();
  const highlighted = page.locator(".markdown-code-block");
  await expect(highlighted).toContainText('const project: string = "Crab";');

  await page
    .getByRole("combobox", { name: "Code theme" })
    .selectOption("vscode");
  await expect(page.locator("html")).toHaveAttribute(
    "data-code-theme",
    "vscode",
  );
  await expect
    .poll(() => page.evaluate(() => localStorage.getItem("crab-code-theme")))
    .toBe("vscode");

  const codeColors = () =>
    highlighted.evaluate((container) => {
      const tokens = container.shadowRoot?.querySelectorAll("[data-line] span");
      return [...(tokens ?? [])].map((token) => getComputedStyle(token).color);
    });
  await expect
    .poll(async () => new Set(await codeColors()).size)
    .toBeGreaterThan(1);
  const lightColors = await codeColors();
  await selectTheme(page, "dark");
  await expect(highlighted).toBeVisible();
  await expect.poll(codeColors).not.toEqual(lightColors);

  await page.reload();
  await expect(page.getByRole("combobox", { name: "Code theme" })).toHaveValue(
    "vscode",
  );
  await expect(page.locator("html")).toHaveAttribute(
    "data-code-theme",
    "vscode",
  );
});

test("format-aware previews explore data, office files, media, and databases locally", async ({
  page,
}) => {
  const duckdbWorkerRequests: string[] = [];
  page.on("request", (request) => {
    if (request.url().includes("duckdb-browser-mvp.worker"))
      duckdbWorkerRequests.push(request.url());
  });
  const csv = "run,model,score\n1,small,0.91\n2,large,0.98\n";
  const svg =
    '<svg xmlns="http://www.w3.org/2000/svg" width="80" height="40"><rect width="80" height="40" fill="#0969da"/></svg>';
  const workbook = zipSync({
    "xl/workbook.xml": strToU8(
      '<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheets><sheet name="Experiments"/></sheets></workbook>',
    ),
    "xl/worksheets/sheet1.xml": strToU8(
      '<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row><c r="A1" t="inlineStr"><is><t>epoch</t></is></c><c r="B1" t="inlineStr"><is><t>loss</t></is></c></row><row><c r="A2"><v>1</v></c><c r="B2"><v>0.42</v></c></row></sheetData></worksheet>',
    ),
  });
  const SQL = await initSqlJs();
  const database = new SQL.Database();
  database.run(
    "CREATE TABLE runs (id INTEGER, model TEXT, score REAL); " +
      "INSERT INTO runs VALUES (1, 'large', 0.98); " +
      "CREATE VIEW top_runs AS SELECT * FROM runs WHERE score > 0.95;",
  );
  const sqlite = database.export();
  database.close();
  const parquet = new Uint8Array(
    parquetWriteBuffer({
      columnData: [
        { name: "run", data: [1, 2], type: "INT32" },
        { name: "score", data: [0.91, 0.98], type: "DOUBLE" },
      ],
    }),
  );
  const arrow = tableToIPC(
    tableFromArrays({ run: [1, 2], model: ["small", "large"] }),
    "file",
  );
  await routePreviewFiles(page, {
    "metrics.csv": {
      text: null,
      bytes: strToU8(csv),
      textTruncated: true,
    },
    "diagram.svg": { text: svg, bytes: strToU8(svg) },
    "report.xlsx": { text: null, bytes: workbook },
    "runs.sqlite": { text: null, bytes: sqlite },
    "features.parquet": {
      text: null,
      bytes: parquet,
      size: 5 * 1024 * 1024 * 1024,
    },
    "batch.arrow": { text: null, bytes: arrow },
    "handbook.pdf": {
      text: null,
      bytes: pdfBytes("Private in-browser PDF preview"),
    },
    "model.onnx": {
      text: null,
      bytes: new Uint8Array([0x08, 0x03, 0x12, 0x00, 0xff, 0x00]),
    },
  });

  await page.goto(
    `/team/project?rev=refs%2Fheads%2Fmain&path=${pathHex("metrics.csv")}&kind=Blob`,
  );
  const csvWorkbench = page.getByRole("region", {
    name: "metrics.csv query workbench",
  });
  await expect(csvWorkbench.getByRole("cell", { name: "large" })).toBeVisible();
  const csvEditor = csvWorkbench.getByRole("textbox", { name: "SQL query" });
  await expect(csvWorkbench.locator(".cm-editor")).toBeVisible();
  await expect(
    csvWorkbench.locator(".cm-line span").filter({ hasText: "SELECT" }).first(),
  ).toHaveCSS("font-weight", "600");
  await csvEditor.fill("SELECT sc");
  await csvEditor.press("Control+Space");
  await expect(page.getByRole("option", { name: /score/ })).toBeVisible();
  await csvEditor.press("Enter");
  await expect(csvEditor).toHaveText("SELECT score");
  await csvEditor.fill("SELECT model, score FROM data WHERE score > 0.95");
  await csvWorkbench.getByRole("button", { name: "Run query" }).click();
  await expect(csvWorkbench.getByRole("cell", { name: "0.98" })).toBeVisible();
  await csvWorkbench.getByRole("button", { name: "Chart" }).click();
  await expect(
    csvWorkbench.getByRole("region", { name: "Query result chart" }),
  ).toBeVisible();
  await csvWorkbench.getByText("Recent runs", { exact: false }).click();
  await expect(csvWorkbench.getByText(/Query · success · 1 row/)).toBeVisible();

  await csvWorkbench
    .getByRole("textbox", { name: "SQL query" })
    .fill("SELECT model, score FROM data");
  await csvWorkbench.getByRole("button", { name: "Explain" }).click();
  await expect(
    csvWorkbench.getByRole("cell", { name: "physical_plan" }),
  ).toBeVisible();
  const resultDownload = page.waitForEvent("download");
  await csvWorkbench
    .getByRole("button", { name: "Download query result as CSV" })
    .click();
  expect((await resultDownload).suggestedFilename()).toBe("metrics-query.csv");

  await csvWorkbench
    .getByRole("textbox", { name: "SQL query" })
    .fill("SELECT sum(i) AS total FROM range(10000000000) values(i)");
  await csvWorkbench.getByRole("button", { name: "Run query" }).click();
  await csvWorkbench.getByRole("button", { name: "Stop query" }).click();
  await expect(csvWorkbench.getByText(/Query stopped/)).toBeVisible();
  await csvWorkbench.getByRole("button", { name: "Count rows" }).click();
  await csvWorkbench.getByRole("button", { name: "Run query" }).click();
  await expect(csvWorkbench.getByRole("cell", { name: "2" })).toBeVisible();
  await page.setViewportSize({ width: 600, height: 900 });
  await expect(
    csvWorkbench.getByRole("textbox", { name: "SQL query" }),
  ).toBeVisible();
  await expect(
    csvWorkbench.getByRole("table", { name: "Query results" }),
  ).toBeVisible();
  await expectNoAccessibilityViolations(page);
  await page.setViewportSize({ width: 1280, height: 720 });

  await page.goto(
    `/team/project?rev=refs%2Fheads%2Fmain&path=${pathHex("diagram.svg")}&kind=Blob`,
  );
  const image = page.getByRole("img", { name: "Preview of diagram.svg" });
  await expect(image).toBeVisible();
  await expect
    .poll(() =>
      image.evaluate((node) => (node as HTMLImageElement).naturalWidth),
    )
    .toBe(80);

  await page.goto(
    `/team/project?rev=refs%2Fheads%2Fmain&path=${pathHex("report.xlsx")}&kind=Blob`,
  );
  await expect(
    page.getByRole("navigation", { name: "Workbook sheets" }),
  ).toContainText("Experiments");
  await expect(page.getByRole("cell", { name: "0.42" })).toBeVisible();

  await page.goto(
    `/team/project?rev=refs%2Fheads%2Fmain&path=${pathHex("runs.sqlite")}&kind=Blob`,
  );
  const sqliteWorkbench = page.getByRole("region", {
    name: "runs.sqlite query workbench",
  });
  await expect(
    sqliteWorkbench.getByRole("complementary", { name: "Dataset schema" }),
  ).toContainText("top_runs");
  await expect(
    sqliteWorkbench.getByRole("cell", { name: "large" }),
  ).toBeVisible();
  const sqliteEditor = sqliteWorkbench.getByRole("textbox", {
    name: "SQL query",
  });
  await sqliteEditor.fill("SELECT sc");
  await sqliteEditor.press("Control+Space");
  const sqliteCompletion = page.getByRole("option", { name: /score/ });
  await expect(sqliteCompletion).toBeVisible();
  await sqliteCompletion.click();
  await expect(sqliteEditor).toHaveText("SELECT score");
  await sqliteWorkbench.getByRole("button", { name: "top_runs" }).click();
  await sqliteWorkbench.getByRole("button", { name: "Sample rows" }).click();
  await expect(sqliteEditor).toContainText('FROM "top_runs"');
  await sqliteWorkbench.getByRole("button", { name: "Run query" }).click();
  await expect(
    sqliteWorkbench.getByRole("cell", { name: "large" }),
  ).toBeVisible();
  await sqliteEditor.fill("SELECT avg(score) AS average FROM runs");
  await sqliteWorkbench.getByRole("button", { name: "Run query" }).click();
  await expect(
    sqliteWorkbench.getByRole("columnheader", { name: "average" }),
  ).toBeVisible();
  await expect(
    sqliteWorkbench.getByRole("cell", { name: "0.98" }),
  ).toBeVisible();
  await sqliteEditor.fill(
    "WITH RECURSIVE count_up(value) AS (VALUES(0) UNION ALL SELECT value + 1 FROM count_up WHERE value < 100000000) SELECT sum(value) FROM count_up",
  );
  await sqliteWorkbench.getByRole("button", { name: "Run query" }).click();
  await sqliteWorkbench.getByRole("button", { name: "Stop query" }).click();
  await expect(sqliteWorkbench.getByText(/Query stopped/)).toBeVisible();
  await expect(
    sqliteWorkbench.getByRole("button", { name: "Run query" }),
  ).toBeEnabled();

  await page.goto(
    `/team/project?rev=refs%2Fheads%2Fmain&path=${pathHex("features.parquet")}&kind=Blob`,
  );
  const parquetWorkbench = page.getByRole("region", {
    name: "features.parquet query workbench",
  });
  await expect
    .poll(() =>
      duckdbWorkerRequests.some(
        (url) =>
          new URL(url).searchParams.get("worker-policy") ===
          "duckdb-extensions-v1",
      ),
    )
    .toBe(true);
  await expect(parquetWorkbench).toContainText("5.00 GB");
  await expect(parquetWorkbench).toContainText("2 source rows");
  const parquetQuery = parquetWorkbench.getByRole("textbox", {
    name: "SQL query",
  });
  await parquetQuery.fill("SELECT  FROM data");
  await parquetQuery.press("Home");
  for (let index = 0; index < 7; index += 1)
    await parquetQuery.press("ArrowRight");
  await parquetWorkbench.getByTitle("Insert score into the query").click();
  await expect(parquetQuery).toHaveText('SELECT "score" FROM data');
  await parquetWorkbench
    .getByRole("button", { name: "Profile columns" })
    .click();
  await expect(parquetQuery).toContainText("approx_count_distinct");
  await parquetWorkbench.getByRole("button", { name: "Run query" }).click();
  const profileColumn = parquetWorkbench.getByRole("columnheader", {
    name: "column_name",
  });
  await profileColumn.getByRole("button").click();
  await expect(profileColumn).toHaveAttribute("aria-sort", "ascending");
  await parquetWorkbench.getByRole("button", { name: "Count rows" }).click();
  await parquetWorkbench.getByRole("button", { name: "Run query" }).click();
  const countResult = parquetWorkbench.getByRole("table", {
    name: "Query results",
  });
  await expect(countResult.getByRole("row")).toHaveCount(2);
  await expect(countResult.getByRole("cell", { name: "2" })).toBeVisible();

  await page.goto(
    `/team/project?rev=refs%2Fheads%2Fmain&path=${pathHex("batch.arrow")}&kind=Blob`,
  );
  const arrowWorkbench = page.getByRole("region", {
    name: "batch.arrow query workbench",
  });
  await expect(arrowWorkbench).toContainText("2 source rows");
  await arrowWorkbench
    .getByRole("textbox", { name: "SQL query" })
    .fill("SELECT model FROM data WHERE run = 2");
  await arrowWorkbench.getByRole("button", { name: "Run query" }).click();
  await expect(
    arrowWorkbench.getByRole("cell", { name: "large" }),
  ).toBeVisible();
  await selectDarkTheme(page);
  await expectNoAccessibilityViolations(page);

  await page.goto(
    `/team/project?rev=refs%2Fheads%2Fmain&path=${pathHex("handbook.pdf")}&kind=Blob`,
  );
  await expect(
    page.getByRole("img", { name: "Page 1 of handbook.pdf" }),
  ).toBeVisible();
  await expect(page.getByText("Page 1 of 1")).toBeVisible();
  await expect(page.getByText("Private in-browser PDF preview")).toBeAttached();

  await page.goto(
    `/team/project?rev=refs%2Fheads%2Fmain&path=${pathHex("model.onnx")}&kind=Blob`,
  );
  await expect(page.getByText("ONNX model")).toBeVisible();
  await expect(
    page.getByRole("heading", { name: "Hex and ASCII" }),
  ).toBeVisible();
  await expect(page.getByText(/00000000\s+08 03 12 00 ff 00/)).toBeVisible();
});

test("blame and source panes resize with pointer and keyboard controls", async ({
  page,
}) => {
  await page.goto(
    `/team/project?rev=refs%2Fheads%2Fmain&path=${pathHex("README.md")}&kind=Blob`,
  );
  await page.getByRole("button", { name: "Blame", exact: true }).click();

  const separator = page.getByRole("separator", {
    name: "Resize blame pane",
  });
  const blamePane = page.getByLabel("Blame commits");
  const sourcePane = page.getByLabel("File source");
  await expect(separator).toHaveAttribute("aria-valuenow", "48");
  await expect(blamePane).toBeVisible();
  await expect(sourcePane).toBeVisible();
  await expectNoAccessibilityViolations(page);

  const initialWidth = (await blamePane.boundingBox())?.width ?? 0;
  const handle = await separator.boundingBox();
  if (!handle) throw new Error("Blame resize handle is not visible");
  await page.mouse.move(handle.x + handle.width / 2, handle.y + 20);
  await page.mouse.down();
  await page.mouse.move(handle.x + handle.width / 2 + 120, handle.y + 20);
  await page.mouse.up();
  await expect
    .poll(async () => (await blamePane.boundingBox())?.width ?? 0)
    .toBeGreaterThan(initialWidth + 80);

  await separator.press("Home");
  await expect(separator).toHaveAttribute("aria-valuenow", "25");
  await separator.press("ArrowRight");
  await expect(separator).toHaveAttribute("aria-valuenow", "30");
  await separator.dblclick();
  await expect(separator).toHaveAttribute("aria-valuenow", "48");
});

test("file tree and content panes resize with pointer and keyboard controls", async ({
  page,
}) => {
  await page.goto(
    `/team/project?rev=refs%2Fheads%2Fmain&path=${pathHex("README.md")}&kind=Blob`,
  );

  const separator = page.getByRole("separator", {
    name: "Resize file tree pane",
  });
  const treePane = page.locator(".tree-sidebar");
  const contentPane = page.locator(".code-main");
  await expect(separator).toHaveAttribute("aria-valuenow", "356");
  await expect(treePane).toBeVisible();
  await expect(contentPane).toBeVisible();
  await expectNoAccessibilityViolations(page);

  const initialWidth = (await treePane.boundingBox())?.width ?? 0;
  const handle = await separator.boundingBox();
  if (!handle) throw new Error("File tree resize handle is not visible");
  await page.mouse.move(handle.x + handle.width / 2, handle.y + 20);
  await page.mouse.down();
  await page.mouse.move(handle.x + handle.width / 2 + 120, handle.y + 20);
  await page.mouse.up();
  await expect
    .poll(async () => (await treePane.boundingBox())?.width ?? 0)
    .toBeGreaterThan(initialWidth + 80);

  await separator.press("Home");
  await expect(separator).toHaveAttribute("aria-valuenow", "240");
  await separator.press("ArrowRight");
  await expect(separator).toHaveAttribute("aria-valuenow", "264");
  await separator.dblclick();
  await expect(separator).toHaveAttribute("aria-valuenow", "356");

  await page.setViewportSize({ width: 600, height: 900 });
  await expect(separator).toBeHidden();
  await expect(treePane).toBeVisible();
  await expect(contentPane).toBeVisible();
  await expectNoAccessibilityViolations(page);
});

test("blame commit messages open their commit details", async ({ page }) => {
  await page.goto(
    `/team/project?rev=refs%2Fheads%2Fmain&path=${pathHex("README.md")}&kind=Blob`,
  );
  await page.getByRole("button", { name: "Blame", exact: true }).click();

  const message = page.getByRole("link", {
    name: "Start the project documentation",
    exact: true,
  });
  await expect(message).toHaveAttribute(
    "href",
    `/team/project?view=commit&rev=${"1".repeat(40)}`,
  );
  await message.click();
  await expect(
    page.getByRole("heading", {
      name: "Make the repository easier to browse",
      exact: true,
    }),
  ).toBeVisible();
});

test("footer groups the Crab mark and tagline into one compact signature", async ({
  page,
}) => {
  await page.goto("/");
  const footer = page.locator(".site-footer");
  const brand = footer.getByRole("link", { name: "Crab repositories" });
  const tagline = footer.getByText("Git for any file at any scale");
  await expect(brand.locator(".footer-brand-mark")).toBeVisible();
  await expect(tagline).toBeVisible();
  const spacing = await footer.evaluate((node) => {
    const brand = node.querySelector(".footer-brand")?.getBoundingClientRect();
    const tagline = node
      .querySelector(".footer-tagline")
      ?.getBoundingClientRect();
    return brand && tagline
      ? tagline.left - brand.right
      : Number.POSITIVE_INFINITY;
  });
  expect(spacing).toBeLessThanOrEqual(12);
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
  await expect(page.locator('[aria-label="Light"]')).toHaveAttribute(
    "aria-pressed",
    "true",
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
              archive_version: 0,
              archived: false,
              protection_version: 0,
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

test("repository settings changes the default branch with explicit confirmation", async ({
  page,
}) => {
  let defaultName = "refs/heads/main";
  const featureOid = "c".repeat(40);
  let update: unknown;
  await page.route(
    (url) => url.pathname === "/api/repos/team/project/refs",
    (route) =>
      route.fulfill({
        json: {
          head: {
            name: defaultName,
            oid: defaultName.endsWith("main") ? oid : featureOid,
          },
          unborn_head: null,
          refs: [
            { name: "refs/heads/main", oid },
            { name: "refs/heads/feature/docs", oid: featureOid },
          ],
          generation: defaultName.endsWith("main") ? 1 : 2,
        },
      }),
  );
  await page.route(
    (url) => url.pathname === "/api/repos/team/project/settings/default-branch",
    (route) => {
      expect(route.request().method()).toBe("PATCH");
      update = route.request().postDataJSON();
      defaultName = "refs/heads/feature/docs";
      return route.fulfill({
        json: { branch: defaultName, commit: featureOid },
      });
    },
  );

  await page.goto("/team/project?view=settings");
  await expect(
    page.getByRole("link", { name: "Settings", exact: true }),
  ).toHaveAttribute("aria-current", "page");
  await expect(
    page.getByRole("heading", { name: "Default branch" }),
  ).toBeVisible();
  await expect(page.locator(".default-branch-current")).toContainText("main");
  await page.getByRole("button", { name: "Change" }).click();
  await page
    .getByRole("combobox", { name: "Choose a branch" })
    .selectOption("refs/heads/feature/docs");
  await page.getByRole("button", { name: "Update", exact: true }).click();
  await expect(
    page.getByText("Change the default branch to feature/docs?"),
  ).toBeVisible();
  await page
    .getByRole("button", {
      name: "I understand, update the default branch",
    })
    .click();
  expect(update).toEqual({
    name: "feature/docs",
    expected_head: "refs/heads/main",
    expected_oid: featureOid,
  });
  await expect(page.locator(".default-branch-current")).toContainText(
    "feature/docs",
  );
  await selectTheme(page, "dark");
  await page.setViewportSize({ width: 390, height: 800 });
  expect(
    await page.evaluate(() => document.documentElement.scrollWidth),
  ).toBeLessThanOrEqual(390);
  await expectNoAccessibilityViolations(page);
});

test("repository settings archive and unarchive the repository with exact confirmation", async ({
  page,
}) => {
  const updates: unknown[] = [];
  page.on("request", (request) => {
    if (
      request.method() === "PUT" &&
      new URL(request.url()).pathname.endsWith("/settings/archive")
    )
      updates.push(request.postDataJSON());
  });

  await page.goto("/team/project?view=settings");
  await expect(
    page.getByRole("heading", { name: "Danger Zone" }),
  ).toBeVisible();
  await page
    .getByRole("button", { name: "Archive this repository", exact: true })
    .click();
  const archive = page.getByRole("region", { name: "archive repository" });
  const confirmArchive = archive.getByRole("button", {
    name: "I understand the consequences, archive this repository",
  });
  await expect(confirmArchive).toBeDisabled();
  await archive.getByLabel("Repository name").fill("team/project");
  await confirmArchive.click();

  await expect(
    page.getByText("This repository was archived and is read-only."),
  ).toBeVisible();
  await expect(
    page.getByText("Archived", { exact: true }).first(),
  ).toBeVisible();
  await expect(
    page.getByRole("button", {
      name: "Unarchive this repository",
      exact: true,
    }),
  ).toBeVisible();
  await expect(page.getByRole("button", { name: "Change" })).toHaveCount(0);
  await page.getByRole("link", { name: "Branches", exact: true }).click();
  await expect(
    page.getByRole("button", { name: "Add branch protection rule" }),
  ).toHaveCount(0);

  await page.getByRole("link", { name: "Issues", exact: true }).first().click();
  await expect(page.getByRole("button", { name: "New issue" })).toHaveCount(0);
  await page.goto("/team/project?view=issues&issue=new");
  await expect(
    page.getByRole("heading", { name: "This repository is read-only" }),
  ).toBeVisible();

  await page.goto("/team/project?view=settings");
  await page
    .getByRole("button", { name: "Unarchive this repository", exact: true })
    .click();
  const unarchive = page.getByRole("region", { name: "unarchive repository" });
  await unarchive.getByLabel("Repository name").fill("team/project");
  await unarchive
    .getByRole("button", {
      name: "I understand the consequences, unarchive this repository",
    })
    .click();
  await expect(
    page.getByText("This repository was archived and is read-only."),
  ).toHaveCount(0);
  await page.getByRole("link", { name: "Issues", exact: true }).first().click();
  await expect(page.getByRole("button", { name: "New issue" })).toBeVisible();
  expect(updates).toEqual([
    {
      expected_version: 0,
      archived: true,
      repository: "team/project",
    },
    {
      expected_version: 1,
      archived: false,
      repository: "team/project",
    },
  ]);

  await selectTheme(page, "dark");
  await page.setViewportSize({ width: 390, height: 800 });
  expect(
    await page.evaluate(() => document.documentElement.scrollWidth),
  ).toBeLessThanOrEqual(390);
  await expectNoAccessibilityViolations(page);
});

test("repository settings create, edit, and delete branch protection rules", async ({
  page,
}) => {
  const updates: unknown[] = [];
  page.on("request", (request) => {
    if (
      request.method() === "PUT" &&
      new URL(request.url()).pathname.endsWith("/settings/branch-protections")
    )
      updates.push(request.postDataJSON());
  });

  await page.goto("/team/project?view=settings&section=branches");
  await expect(
    page.getByRole("link", { name: "Branches", exact: true }),
  ).toHaveAttribute("aria-current", "page");
  await expect(
    page.getByRole("heading", { name: "Branch protection rules" }),
  ).toBeVisible();
  await expect(page.getByText("No branch protection rules")).toBeVisible();

  await page
    .getByRole("button", { name: "Add branch protection rule" })
    .click();
  await page.getByLabel("Branch name").fill("main");
  await page.getByLabel("Required approving reviews").selectOption("2");
  await page.getByLabel("Required status checks").fill("ci/test\nsecurity");
  await page.getByRole("button", { name: "Save changes" }).click();
  const rule = page.locator(".protection-list > li");
  await expect(rule).toContainText("main");
  await expect(rule).toContainText("2 approving reviews required");
  await expect(rule.locator(".protection-checks")).toContainText("ci/test");
  await expect(rule.locator(".protection-checks")).toContainText("security");
  expect(updates[0]).toEqual({
    expected_version: 0,
    rules: [
      {
        branch: "main",
        required_approvals: 2,
        required_checks: ["ci/test", "security"],
      },
    ],
  });

  await page.getByRole("button", { name: "Edit main" }).click();
  await page.getByLabel("Required approving reviews").selectOption("1");
  await page.getByLabel("Required status checks").fill("build");
  await page.getByRole("button", { name: "Save changes" }).click();
  await expect(rule).toContainText("1 approving review required");
  await expect(rule.locator(".protection-checks")).toHaveText("build");
  expect(updates[1]).toEqual({
    expected_version: 1,
    rules: [
      {
        branch: "main",
        required_approvals: 1,
        required_checks: ["build"],
      },
    ],
  });

  await page.getByRole("button", { name: "Delete main" }).click();
  const confirmation = page.getByRole("region", {
    name: "Delete main protection",
  });
  await expect(confirmation).toContainText("Remove protection from main?");
  await confirmation.getByRole("button", { name: "Remove rule" }).click();
  await expect(page.getByText("No branch protection rules")).toBeVisible();
  expect(updates[2]).toEqual({ expected_version: 2, rules: [] });

  await selectTheme(page, "dark");
  await page.setViewportSize({ width: 390, height: 800 });
  expect(
    await page.evaluate(() => document.documentElement.scrollWidth),
  ).toBeLessThanOrEqual(390);
  await expectNoAccessibilityViolations(page);
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
