import { chooseSelectOption } from "./select";
import { expect, test, type Page } from "@playwright/test";
import type { FilePreview } from "@/lib/contracts";
import { tableFromArrays, tableToIPC } from "apache-arrow";
import { strToU8, zipSync } from "fflate";
import { parquetWriteBuffer } from "hyparquet-writer";
import initSqlJs from "sql.js";
import { emptyIssueTemplates } from "./fixtures";
import {
  expectNoAccessibilityViolations,
  selectDarkTheme,
  selectTheme,
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
const rustSource = `pub struct Crab;

impl Crab {
    pub fn open(&self) {}
}

pub fn serve(crab: &Crab) {
    crab.open();
}
`;
const typescriptSource = `interface Repository {
  open(): void;
}

export function browse(repository: Repository) {
  repository.open();
}
`;
const additionalSymbolSources = {
  "src/navigation.rs": [
    `// ${"A wide source line with horizontal scrolling. ".repeat(12)}`,
    ...Array.from({ length: 80 }, (_, index) => `// Before ${index}`),
    "pub fn navigate() {}",
    ...Array.from({ length: 80 }, (_, index) => `// After ${index}`),
  ].join("\n"),
  "src/main.py": `class Repository:
    def open(self):
        return None

def browse(repository):
    repository.open()
`,
  "cmd/crab/main.go": `package main

type Repository struct{}

func (Repository) Open() {}

func Browse(repository Repository) { repository.Open() }
`,
  "src/Repository.java": `class Repository {
  void open() {}
}

class Browser {
  void browse(Repository repository) { repository.open(); }
}
`,
  "src/repository.c": `struct Repository { int open; };

void browse(struct Repository *repository) {}
`,
  "src/repository.cpp": `class Repository {
 public:
  void open() {}
};

void browse(Repository& repository) { repository.open(); }
`,
} as const;
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
    }
  >,
) {
  await page.route(
    "**/api/repos/team/project/file-preview?*",
    async (route) => {
      const path = new URL(route.request().url()).searchParams.get("path_hex");
      const file = Object.entries(files).find(
        ([name]) => pathHex(name) === path,
      )?.[1];
      if (!file) return route.fallback();
      const size = file.size ?? file.bytes.byteLength;
      let preview: FilePreview["preview"];
      if (size > 1024 * 1024) preview = { state: "omitted_size" };
      else if (file.bytes.includes(0)) preview = { state: "binary" };
      else {
        try {
          preview = {
            state: "inline",
            text: new TextDecoder("utf-8", { fatal: true }).decode(file.bytes),
          };
        } catch {
          preview = { state: "unsupported_encoding" };
        }
      }
      if (preview.state === "inline" && file.text !== null)
        expect(preview.text).toBe(file.text);
      return route.fulfill({
        json: {
          oid,
          size,
          mode: "100644",
          classification: "OrdinaryGit",
          preview,
          commit: oid,
          generation: 1,
        } satisfies FilePreview,
      });
    },
  );
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
    if (url.pathname === "/api/repos/team/project/issues/templates")
      return route.fulfill({ json: emptyIssueTemplates });
    if (
      url.pathname.startsWith("/api/repos/team/project/") &&
      !["GET", "HEAD"].includes(route.request().method())
    )
      expect(route.request().headers()["x-crab-repository-id"]).toBe(
        "00000000-0000-4000-8000-000000000099",
      );
    if (url.pathname === "/api/session")
      return route.fulfill({
        json: { authenticated: true, mode: "local", user: null, csrf: null },
      });
    if (url.pathname === "/api/repos")
      return route.fulfill({
        json: {
          repositories: [
            {
              id: "00000000-0000-4000-8000-000000000099",
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
    if (
      url.pathname.endsWith("/tree") ||
      url.pathname.endsWith("/tree-attribution")
    )
      return route.fulfill({
        json: {
          ...(url.pathname.endsWith("/tree-attribution")
            ? { state: "ready" }
            : {}),
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
            ...(url.pathname.endsWith("/tree-attribution")
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
          generation: 1,
          directory_oid: oid,
        },
      });
    if (url.pathname.endsWith("/file-codeowners"))
      return route.fulfill({
        json: {
          commit: url.searchParams.get("rev"),
          path_hex: url.searchParams.get("path_hex"),
          source: null,
        },
      });
    if (url.pathname.endsWith("/file-preview")) {
      const requestedPath = url.searchParams.get("path_hex");
      const additionalSymbolSource = Object.entries(
        additionalSymbolSources,
      ).find(([path]) => requestedPath === pathHex(path))?.[1];
      const text =
        requestedPath === pathHex("README.md")
          ? currentReadme
          : requestedPath === pathHex("src/lib.rs")
            ? rustSource
            : requestedPath === pathHex("src/symbols.ts")
              ? typescriptSource
              : (additionalSymbolSource ?? "Hello, team!");
      return route.fulfill({
        json: {
          oid:
            url.searchParams.get("path_hex") === pathHex("README.md")
              ? currentReadmeOid
              : oid,
          size: text.length,
          mode: "100644",
          classification: "OrdinaryGit",
          preview: { state: "inline", text },
          commit: oid,
          generation: 1,
        } satisfies FilePreview,
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
          base: url.searchParams.get("merge_base") ? currentHead : null,
          base_tip: url.searchParams.get("merge_base")
            ? currentHead
            : undefined,
          commit: url.searchParams.get("merge_base") ? createdBranchHead : oid,
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
              kind: "issue",
              number: url.searchParams.get("q") === "is:closed" ? 43 : 42,
              comment_count: 0,
              reaction_count: 0,
              author: { name: "Alice", profile: null },
              body: null,
              title: "Keep object storage reads bounded",
              closed_at:
                url.searchParams.get("q") === "is:closed"
                  ? 1700000001000
                  : null,
              state:
                url.searchParams.get("q") === "is:closed" ? "closed" : "open",
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
              assignees: [
                {
                  id: "00000000-0000-4000-8000-000000000001",
                  name: "Alice",
                  profile: null,
                },
              ],
              version: 1,
              created_at: 1_700_000_000_000,
              updated_at: 1_700_000_000_000,
              can_edit: true,
              can_label: true,
              can_assign: true,
            },
          ],
          next: null,
          counts: {
            open: 1,
            closed: 1,
            merged: 0,
          },
          authored_by_me: false,
          assigned_to_me: false,
          review_requested_from_me: false,
          matching: 1,
          navigation: {
            kind: "state",
            selected:
              url.searchParams.get("q") === "is:closed" ? "closed" : "open",
            open: "is:open",
            closed: "is:closed",
          },
        },
      });
    return route.fulfill({
      status: 404,
      json: { error: { message: "Fixture route unavailable" } },
    });
  });
});

test("GitHub paths retain file context, pin permalinks, and restore browser history", async ({
  page,
}) => {
  await page.route(
    (url) => url.pathname === "/api/repos/team/project/refs",
    (route) =>
      route.fulfill({
        json: {
          head: { name: "refs/heads/main", oid },
          unborn_head: null,
          generation: 1,
          refs: [
            { name: "refs/heads/main", oid },
            { name: "refs/heads/feature/x", oid },
          ],
        },
      }),
  );
  const catalogReads: string[] = [];
  page.on("request", (request) => {
    const url = new URL(request.url());
    if (url.pathname === "/api/repos") catalogReads.push(url.search);
  });
  await page.goto("/team/project");
  const navigation = page.getByRole("navigation", {
    name: "Repository",
    exact: true,
  });
  await expect(navigation.getByRole("link")).toHaveText([
    "Code",
    "Issues",
    "Pull requests",
    "Projects",
    "Activity",
    "Settings",
  ]);
  await expect(
    navigation.getByRole("link", { name: "Pull requests" }),
  ).toHaveAttribute("href", "/team/project/pulls");
  await expect(
    navigation.getByRole("link", { name: "Results", exact: true }),
  ).toHaveCount(0);
  await expect(
    navigation.getByRole("link", { name: "Projects", exact: true }),
  ).toHaveAttribute("href", "/team/project/projects");
  await expect(
    navigation.getByRole("link", { name: "Activity", exact: true }),
  ).toHaveAttribute("href", "/team/project/activity");
  await page.getByRole("button", { name: "Add file", exact: true }).click();
  await page.getByRole("menuitem", { name: "Create new file" }).click();
  await expect(page).toHaveURL("/team/project/new/main");
  await page.goBack();
  await expect(
    page.getByRole("region", { name: "Folders and files" }),
  ).toBeVisible();
  await page
    .getByRole("region", { name: "Folders and files" })
    .getByRole("link", { name: "README.md", exact: true })
    .click();
  await expect(page).toHaveURL("/team/project/blob/main/README.md");
  await page.getByRole("button", { name: "Blame", exact: true }).click();
  await expect(page).toHaveURL("/team/project/blame/main/README.md");
  await page.reload();
  await expect(page.getByLabel("Blame commits")).toBeVisible();
  await page.keyboard.press("y");
  await expect(page).toHaveURL(`/team/project/blame/${oid}/README.md`);
  await expect(
    page.getByRole("button", {
      name: "Switch branches or tags, current aaaaaaa",
      exact: true,
    }),
  ).toBeVisible();
  await page.goBack();
  await expect(page).toHaveURL("/team/project/blame/main/README.md");
  await expect(page.getByLabel("Blame commits")).toBeVisible();
  await page
    .getByRole("button", {
      name: "Switch branches or tags, current main",
      exact: true,
    })
    .click();
  await page
    .getByRole("menuitemradio", { name: "feature/x", exact: true })
    .click();
  await expect(page).toHaveURL("/team/project/blame/feature%2Fx/README.md");
  await page.reload();
  await expect(page.getByLabel("Blame commits")).toBeVisible();
  expect(catalogReads.length).toBeGreaterThan(0);
  expect(
    catalogReads.every(
      (query) =>
        new URLSearchParams(query).get("owner") === "team" &&
        new URLSearchParams(query).get("name") === "project",
    ),
  ).toBe(true);
});

for (const path of [
  "/team/project/results",
  "/team/project/results?report=frozen-report",
  `/team/project?rev=main&path=${pathHex("README.md")}&kind=Blob#L3`,
  "/team/project?view=releases&release=2",
  "/team/project/releases/2",
  "/team/project/issues?state=closed",
  "/team/project/pulls?state=open",
]) {
  test(`unsupported browser address shows not found without repository reads: ${path}`, async ({
    page,
  }) => {
    const repositoryReads: string[] = [];
    page.on("request", (request) => {
      if (new URL(request.url()).pathname.startsWith("/api/repos"))
        repositoryReads.push(request.url());
    });
    await page.goto(path);
    await expect(
      page.getByText("Page not found", { exact: true }),
    ).toBeVisible();
    await expect(page).toHaveURL(path);
    expect(repositoryReads).toEqual([]);
  });
}

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
        location: `/team/project/blob/main/README.md`,
        ready: page.locator(".file-panel"),
      },
      {
        location: "/team/project/delete/main/README.md",
        ready: page.getByRole("heading", { name: "Delete README.md" }),
      },
      {
        location: `/team/project/commit/${oid}`,
        ready: page.getByRole("heading", {
          name: "3 changed files",
          exact: true,
        }),
      },
      {
        location: "/team/project/issues",
        ready: page.getByRole("heading", { name: "All issues" }),
      },
      {
        location: "/team/project/issues/new",
        ready: page.getByRole("heading", { name: "New issue", exact: true }),
      },
      {
        location: "/team/project/branches",
        ready: page.getByRole("heading", {
          name: "Branches",
          exact: true,
          level: 2,
        }),
      },
      {
        location: "/team/project/settings",
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
  await page.goto("/team/project/issues/new");
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
    toolbar.getByRole("button", {
      name: "Mention a person or team",
      exact: true,
    }),
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
    .getByRole("button", { name: "Mention a person or team", exact: true })
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
  await page.goto(`/team/project/commit/${oid}`);

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
  await expect(modified.locator('[data-icon-token="typescript"]')).toHaveCount(
    1,
  );
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
    `/team/project/commit/${oid}`,
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
  ).toHaveAttribute("href", `/team/project/blob/main/src/index.ts`);
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
  await expect(page).toHaveURL(/\/(tree|blob)\/main(?:\/|$)/);
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
  await expect(page.locator(".tree-sidebar")).toHaveCSS("width", "320px");
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
  await expect(page).toHaveURL(/\/commits\/main\/README\.md\?kind=Blob$/);
  await expect(page.getByRole("heading", { name: "Commits" })).toBeVisible();
  await expect(
    page.getByRole("link", { name: "README.md", exact: true }),
  ).toHaveAttribute("href", `/team/project/blob/main/README.md`);
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
  await page.goto(`/team/project/blob/main/README.md`);
  await page.getByRole("button", { name: "Preview", exact: true }).click();
  const preview = page.locator(".file-markdown-preview");
  await expect(
    preview.getByRole("heading", { name: "Team project" }),
  ).toBeVisible();
  await expect(
    preview.getByRole("link", { name: "source entry" }),
  ).toHaveAttribute("href", `/team/project/blob/main/src/index.ts`);
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

test("oversized source requires explicit byte inspection", async ({
  page,
}, info) => {
  const bytes = new Uint8Array(2 * 1024 * 1024).fill(65);
  await routePreviewFiles(page, { "huge.rs": { text: null, bytes } });
  let rawRequests = 0;
  let completedRawRequests = 0;
  const abortedRawRequests: string[] = [];
  page.on("request", (request) => {
    if (new URL(request.url()).pathname.endsWith("/blob")) rawRequests += 1;
  });
  page.on("requestfinished", (request) => {
    if (new URL(request.url()).pathname.endsWith("/blob"))
      completedRawRequests += 1;
  });
  page.on("requestfailed", (request) => {
    if (new URL(request.url()).pathname.endsWith("/blob"))
      abortedRawRequests.push(
        request.failure()?.errorText ?? "unknown failure",
      );
  });
  await page.goto("/team/project/blob/main/huge.rs");
  await expect(page.locator(".file-panel .notice")).toContainText(
    "Preview omitted because of file size",
  );
  await expect(
    page.getByRole("button", { name: "Info", exact: true }),
  ).toHaveAttribute("aria-pressed", "true");
  for (const name of ["Copy file contents", "Edit this file", "Blame"]) {
    await expect(page.getByRole("button", { name, exact: true })).toHaveCount(
      0,
    );
  }
  await expect(page.locator(".file-source-code")).toHaveCount(0);
  for (const theme of ["light", "dark"] as const) {
    await selectTheme(page, theme);
    for (const width of [1280, 390]) {
      await page.setViewportSize({ width, height: 900 });
      expect(
        await page.evaluate(() => document.documentElement.scrollWidth),
      ).toBeLessThanOrEqual(width);
      await expectNoAccessibilityViolations(page);
      await page.screenshot({
        path: info.outputPath(`omitted-preview-${theme}-${width}.png`),
      });
    }
  }
  expect(rawRequests).toBe(0);
  const preview = page.getByRole("button", { name: "Preview", exact: true });
  await preview.focus();
  await page.keyboard.press("Enter");
  await expect(page.locator(".generic-file-preview")).toContainText(
    "Text sample",
  );
  // Vite's root StrictMode exercises setup/cleanup twice. Count the completed
  // read separately and prove that any discarded setup cancels its request.
  await expect.poll(() => completedRawRequests).toBe(1);
  for (const failure of abortedRawRequests)
    expect(failure).toContain("ERR_ABORTED");
  expect(rawRequests).toBe(1 + abortedRawRequests.length);
});

test("empty, binary and unsupported text previews remain distinct", async ({
  page,
}) => {
  await routePreviewFiles(page, {
    "empty.rs": { text: "", bytes: new Uint8Array() },
    "binary.rs": { text: null, bytes: new Uint8Array([0, 65]) },
    "non-utf8.rs": { text: null, bytes: new Uint8Array([255, 65]) },
  });
  await page.goto("/team/project/blob/main/empty.rs");
  await expect(page.locator(".file-metadata")).toContainText("0 lines");
  await expect(
    page.getByRole("button", { name: "Copy file contents", exact: true }),
  ).toBeVisible();
  for (const [name, title] of [
    ["binary.rs", "Binary file"],
    ["non-utf8.rs", "Unsupported text encoding"],
  ]) {
    await page.goto(`/team/project/blob/main/${name}`);
    await expect(page.locator(".file-metadata")).toContainText(title);
    await page.getByRole("button", { name: "Info", exact: true }).click();
    await expect(page.locator(".file-panel .notice")).toContainText(title);
    await expect(
      page.getByRole("button", { name: "Copy file contents", exact: true }),
    ).toHaveCount(0);
  }
});

test("code palette persists and follows light and dark appearance", async ({
  page,
}) => {
  await page.goto(`/team/project/blob/main/README.md`);
  await page.getByRole("button", { name: "Preview", exact: true }).click();
  const highlighted = page.locator(".markdown-code-block");
  await expect(highlighted).toContainText('const project: string = "Crab";');

  await page
    .getByRole("button", { name: "Open user menu", exact: true })
    .click();
  await page.getByRole("menuitem", { name: "Appearance", exact: true }).click();
  await page.getByRole("radio", { name: /^VS Code/ }).click();
  await expect(page.locator("html")).toHaveAttribute(
    "data-code-theme",
    "vscode",
  );
  await expect
    .poll(() => page.evaluate(() => localStorage.getItem("crab-code-theme")))
    .toBe("vscode");

  await page.goBack();
  await page.getByRole("button", { name: "Preview", exact: true }).click();
  await expect(highlighted).toContainText('const project: string = "Crab";');

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
  await expect(page.locator("html")).toHaveAttribute(
    "data-code-theme",
    "vscode",
  );
});

test("supported source files expose browser-parsed current-file symbols", async ({
  page,
}) => {
  await page.setViewportSize({ width: 1600, height: 1200 });
  await page.goto(`/team/project/blob/main/src/lib.rs`);

  const treeSidebar = page.locator(".tree-sidebar");
  const tree = page.locator(".repository-tree");
  const breadcrumb = page.locator(".file-navigation");
  const fileHeader = page.locator(".file-panel > .panel-header");
  const symbols = page.getByRole("complementary", { name: "Code symbols" });
  const symbolsHeader = symbols.locator(".code-symbols-header");
  await expect(symbols).toBeVisible();
  await expect(symbols).toHaveCSS("position", "sticky");
  await expect(breadcrumb).toHaveCSS("position", "sticky");
  await expect(fileHeader).toHaveCSS("position", "sticky");
  await expect
    .poll(() =>
      treeSidebar.evaluate((node) =>
        Math.max(0, node.getBoundingClientRect().bottom - window.innerHeight),
      ),
    )
    .toBe(0);
  await expect
    .poll(async () => {
      const [sidebarBottom, treeBottom] = await Promise.all([
        treeSidebar.evaluate((node) => node.getBoundingClientRect().bottom),
        tree.evaluate((node) => node.getBoundingClientRect().bottom),
      ]);
      return Math.abs(sidebarBottom - treeBottom);
    })
    .toBeLessThanOrEqual(1);
  await expect
    .poll(() =>
      symbols.evaluate((node) =>
        Math.max(0, node.getBoundingClientRect().bottom - window.innerHeight),
      ),
    )
    .toBeLessThanOrEqual(1);
  await expect(symbols.getByText("impl Crab", { exact: true })).toBeVisible();
  await expect(
    symbols.getByRole("button", { name: /serve function 7/ }),
  ).toBeVisible();
  const resize = page.getByRole("separator", { name: "Resize symbols pane" });
  const originalWidth = (await symbols.boundingBox())!.width;
  await resize.focus();
  await resize.press("ArrowLeft");
  await expect
    .poll(async () => (await symbols.boundingBox())!.width)
    .toBeGreaterThan(originalWidth + 10);
  const grip = resize.locator(":scope > div");
  const bounds = (await grip.boundingBox())!;
  const keyboardWidth = (await symbols.boundingBox())!.width;
  await page.mouse.move(
    bounds.x + bounds.width / 2,
    bounds.y + bounds.height / 2,
  );
  await page.mouse.down();
  await page.mouse.move(bounds.x - 50, bounds.y + bounds.height / 2, {
    steps: 5,
  });
  await page.mouse.up();
  await expect
    .poll(async () => (await symbols.boundingBox())!.width)
    .toBeGreaterThan(keyboardWidth + 35);
  await expectNoAccessibilityViolations(page);

  await page.locator(".file-source-code").evaluate((node) => {
    node.style.minHeight = `${window.innerHeight * 3}px`;
  });
  await page.evaluate(() => window.scrollTo(0, 900));
  await expect
    .poll(async () => {
      const [breadcrumbBottom, headerTop] = await Promise.all([
        breadcrumb.evaluate((node) => node.getBoundingClientRect().bottom),
        fileHeader.evaluate((node) => node.getBoundingClientRect().top),
      ]);
      return Math.abs(breadcrumbBottom - headerTop);
    })
    .toBeLessThanOrEqual(1);
  await expect
    .poll(() =>
      treeSidebar.evaluate((node) => {
        const bounds = node.getBoundingClientRect();
        return [bounds.top, window.innerHeight - bounds.bottom];
      }),
    )
    .toEqual([0, 0]);
  await expect
    .poll(() =>
      page.evaluate(() => ({
        document: document.scrollingElement?.scrollTop ?? 0,
        source: document.querySelector(".file-source-code")?.scrollTop ?? 0,
      })),
    )
    .toEqual({ document: 900, source: 0 });
  await expect
    .poll(async () => {
      const [symbolsBounds, symbolsHeaderTop, toolbarTop] = await Promise.all([
        symbols.evaluate((node) => {
          const bounds = node.getBoundingClientRect();
          return {
            top: bounds.top,
            bottomOverflow: Math.max(0, bounds.bottom - window.innerHeight),
          };
        }),
        symbolsHeader.evaluate((node) => node.getBoundingClientRect().top),
        fileHeader.evaluate((node) => node.getBoundingClientRect().top),
      ]);
      return Math.max(
        Math.abs(symbolsBounds.top - toolbarTop),
        Math.abs(symbolsBounds.top - symbolsHeaderTop),
        symbolsBounds.bottomOverflow,
      );
    })
    .toBeLessThanOrEqual(1);
  await expect(symbols).toBeVisible();
  await page.evaluate(() => window.scrollTo(0, 0));

  await symbols.getByPlaceholder("Filter symbols").fill("serve");
  await expect(
    symbols.getByRole("button", { name: /serve function 7/ }),
  ).toBeVisible();
  await expect(
    symbols.getByRole("button", { name: /open method 4/ }),
  ).toHaveCount(0);

  await symbols.getByRole("button", { name: /serve function 7/ }).click();
  await expect(page).toHaveURL(/#L7$/);
  await expect(page.locator('[data-line="7"]')).toHaveAttribute(
    "data-code-symbol-target",
    "true",
  );
  const allSymbols = symbols.getByRole("button", {
    name: "All symbols",
    exact: true,
  });
  await allSymbols.focus();
  await allSymbols.press("Enter");
  await expect(symbols.getByPlaceholder("Filter symbols")).toHaveValue("serve");
  await symbols.getByPlaceholder("Filter symbols").fill("");

  await page.locator('[data-line="8"] span', { hasText: "open" }).click();
  await expect(symbols.getByText("Definitions 1")).toBeVisible();
  await expect(symbols.getByText("References 1")).toBeVisible();
  const reference = symbols.getByRole("button", { name: "open call 8" });
  await reference.click();
  await expect(reference).toHaveAttribute("aria-current", "location");
  await expect(page).toHaveURL(/#L8$/);
  await expectNoAccessibilityViolations(page);

  await symbols.getByRole("button", { name: "Close symbols" }).click();
  await expect(symbols).toHaveCount(0);
  await page.getByRole("button", { name: "Open code symbols" }).click();
  await expect(
    page.getByRole("complementary", { name: "Code symbols" }),
  ).toBeVisible();

  await page.goto(`/team/project/blob/main/src/symbols.ts`);
  const typeScriptSymbols = page.getByRole("complementary", {
    name: "Code symbols",
  });
  await expect(
    typeScriptSymbols.getByRole("button", { name: /Repository interface 1/ }),
  ).toBeVisible();
  await expect(
    typeScriptSymbols.getByRole("button", { name: /browse function 5/ }),
  ).toBeVisible();

  for (const [path, symbol] of [
    ["src/main.py", /browse function 5/],
    ["cmd/crab/main.go", /Browse function 7/],
    ["src/Repository.java", /browse method 6/],
    ["src/repository.c", /browse function 3/],
    ["src/repository.cpp", /browse function 6/],
  ] as const) {
    await page.goto(`/team/project/blob/main/${path}`);
    await expect(
      page
        .getByRole("complementary", { name: "Code symbols" })
        .getByRole("button", { name: symbol }),
    ).toBeVisible();
  }
});

test("symbol details show every current-file use with highlighted source previews", async ({
  page,
}) => {
  const text = `fn build_metrics(value: i32) -> i32 { value + 1 }
fn report() {
    let callback = build_metrics;
    let first = { let text = "🦀é"; build_metrics(1) };
    let second = callback(build_metrics(2)) + build_metrics(3);
    let description = "build_metrics"; // build_metrics
}
`;
  await routePreviewFiles(page, {
    "src/references.rs": { text, bytes: strToU8(text) },
  });
  await page.setViewportSize({ width: 1440, height: 1000 });
  await page.goto("/team/project/blob/main/src/references.rs");
  const symbols = page.getByRole("complementary", { name: "Code symbols" });
  await symbols
    .getByRole("button", { name: "build_metrics function 1" })
    .click();
  await expect(
    symbols.getByRole("heading", { name: "References 4" }),
  ).toBeVisible();
  const definition = symbols.getByRole("button", {
    name: "build_metrics function 1",
  });
  await expect(definition).toContainText(
    "fn build_metrics(value: i32) -> i32 { value + 1 }",
  );
  await expect(
    symbols.getByRole("button", { name: "build_metrics identifier 3" }),
  ).toContainText("let callback = build_metrics;");
  await expect(
    symbols
      .getByRole("button", { name: "build_metrics call 4" })
      .locator("mark"),
  ).toHaveText("build_metrics");
  const repeated = symbols.getByRole("button", {
    name: "build_metrics call 5",
  });
  await expect(repeated).toContainText(
    "let second = callback(build_metrics(2)) + build_metrics(3);",
  );
  await expect(repeated.locator("mark")).toHaveCount(2);
  await repeated.click();
  await expect(page).toHaveURL(/#L5$/);
  await expect(repeated).toHaveAttribute("aria-current", "location");
  await expect(symbols.locator(".code-symbol-detail-title")).toHaveText(
    "functionbuild_metrics",
  );
  await expect
    .poll(() =>
      definition
        .locator("code span")
        .evaluateAll(
          (nodes) =>
            new Set(nodes.map((node) => getComputedStyle(node).color)).size,
        ),
    )
    .toBeGreaterThan(1);
  await expectNoAccessibilityViolations(page);
  await selectDarkTheme(page);
  await expect
    .poll(() =>
      definition
        .locator("code span")
        .evaluateAll(
          (nodes) =>
            new Set(nodes.map((node) => getComputedStyle(node).color)).size,
        ),
    )
    .toBeGreaterThan(1);
  await expectNoAccessibilityViolations(page);
  await symbols.getByRole("button", { name: "Close symbols" }).click();
  await page
    .locator('.file-source-code [data-line="1"] span', {
      hasText: /^build_metrics$/,
    })
    .click();
  await expect(
    symbols.getByRole("heading", { name: "References 4" }),
  ).toBeVisible();
  await page.setViewportSize({ width: 390, height: 844 });
  await expect(repeated).toBeVisible();
  expect(
    await page.evaluate(() => document.documentElement.scrollWidth),
  ).toBeLessThanOrEqual(390);
});

test("file code owners follow the inspected commit and link to the exact policy line", async ({
  page,
}) => {
  const historical = pathParent;
  const file = "README.md";
  const policy = ".github/CODEOWNERS";
  const owner = "@release-owner";
  const text = `${"# Ownership rules\n".repeat(6)}*.md ${owner}\n`;
  const seen: string[] = [];
  await page.route(
    (url) => url.pathname.endsWith("/file-preview"),
    (route) => {
      const query = new URL(route.request().url()).searchParams;
      const source = query.get("path_hex") === pathHex(policy);
      return route.fulfill({
        json: {
          oid,
          size: source ? text.length : readme.length,
          mode: "100644",
          classification: "OrdinaryGit",
          preview: { state: "inline", text: source ? text : readme },
          commit: query.get("rev") === historical ? historical : oid,
          generation: 1,
        } satisfies FilePreview,
      });
    },
  );
  await page.route(
    (url) => url.pathname.endsWith("/file-codeowners"),
    (route) => {
      const query = new URL(route.request().url()).searchParams;
      const rev = query.get("rev");
      const path = query.get("path_hex");
      if (rev) seen.push(rev);
      const source =
        path === pathHex(file) && rev === historical
          ? {
              path: policy,
              path_hex: pathHex(policy),
              line: 7,
              owners: [
                { reference: owner, eligible: true },
                { reference: "@unavailable", eligible: false },
              ],
              problem: null,
            }
          : null;
      return route.fulfill({ json: { commit: rev, path_hex: path, source } });
    },
  );
  const button = page.getByRole("button", { name: "Code owners", exact: true });
  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto(`/team/project/blob/${historical}/${file}`);
  await button.focus();
  await button.press("Enter");
  const popover = page.getByRole("dialog", { name: "File code owners" });
  await expect(popover.getByRole("listitem")).toHaveText(owner);
  await expect(popover).toContainText(
    "Some listed owners could not be resolved or lack Write access.",
  );
  const rule = popover.getByRole("link", { name: `${policy}, line 7` });
  const target = `/team/project/blob/${historical}/${policy}#L7`;
  await expect(rule).toHaveAttribute("href", target);
  await expectNoAccessibilityViolations(page);
  await rule.click();
  await expect(page).toHaveURL(target);
  await expect(page.locator('.file-source-code [data-line="7"]')).toContainText(
    owner,
  );
  await page.goto(`/team/project/blob/main/${file}`);
  await expect.poll(() => seen).toContain(oid);
  await expect(button).toHaveCount(0);
});

test("source stays beside the file tree across tablet widths and remains reachable on mobile", async ({
  page,
}) => {
  await page.setViewportSize({ width: 1133, height: 1140 });
  await page.goto("/team/project/blob/main/src/navigation.rs#L82");
  const source = page.locator('.file-source-code [data-line="82"]');
  await expect(source).toBeInViewport();

  for (const width of [900, 785, 641, 600, 390, 1133]) {
    await page.setViewportSize({ width, height: 1140 });
    if (width > 640) {
      await expect
        .poll(() =>
          page.evaluate(() => {
            const tree = document
              .querySelector(".tree-sidebar")!
              .getBoundingClientRect();
            const main = document
              .querySelector(".code-main")!
              .getBoundingClientRect();
            return main.left >= tree.right && main.right <= innerWidth;
          }),
        )
        .toBe(true);
    }
    await expect(page.locator(".file-source-code")).toBeVisible();
    expect(
      await page.evaluate(() => document.documentElement.scrollWidth),
    ).toBeLessThanOrEqual(width);
  }

  await page.setViewportSize({ width: 785, height: 1140 });
  await page
    .getByRole("button", { name: "Close file tree", exact: true })
    .click();
  await expect(page.locator(".tree-sidebar")).toHaveCount(0);
  await expect
    .poll(() =>
      page
        .locator(".code-main")
        .evaluate((node) => node.getBoundingClientRect().width),
    )
    .toBe(785);
  await expectNoAccessibilityViolations(page);
});

test("Symbols grip stays at the viewport midpoint across file lengths and scrolling", async ({
  page,
}) => {
  await page.setViewportSize({ width: 1440, height: 1000 });
  const divider = page.getByRole("separator", { name: "Resize symbols pane" });
  const expectAlignedDivider = async () => {
    await expect
      .poll(() =>
        divider.evaluate((node) => {
          const line = node.getBoundingClientRect();
          const grip = node.firstElementChild!.getBoundingClientRect();
          const parent = node.parentElement!;
          const source = parent
            .querySelector(".file-body")!
            .getBoundingClientRect();
          const toolbar = parent
            .querySelector(".file-toolbar")!
            .getBoundingClientRect();
          const symbols = parent
            .querySelector(".code-symbols-panel")!
            .getBoundingClientRect();
          const tree = parent
            .closest(".code-layout")!
            .querySelector(".tree-resizer")!;
          const treeTarget = tree
            .querySelector(".pane-resizer-target")!
            .getBoundingClientRect();
          const treeGripY =
            tree.getBoundingClientRect().top +
            parseFloat(getComputedStyle(tree, "::after").top);
          return Math.max(
            Math.abs(grip.y + grip.height / 2 - innerHeight / 2),
            Math.abs(grip.x + grip.width / 2 - source.right + 0.5),
            Math.abs(line.x + line.width / 2 - grip.x - grip.width / 2),
            Math.abs(line.y + line.height / 2 - grip.y - grip.height / 2),
            Math.abs(symbols.top - toolbar.top),
            Math.abs(treeTarget.y + treeTarget.height / 2 - innerHeight / 2),
            Math.abs(treeGripY - innerHeight / 2),
          );
        }),
      )
      .toBeLessThanOrEqual(0.02);
  };
  for (const [count, continuation] of [
    [1, 0],
    [30, 0],
    [1, 200],
    [120, 200],
  ]) {
    const text = [
      ...Array.from(
        { length: count },
        (_, index) => `pub fn symbol_${index}() {}`,
      ),
      ...Array.from(
        { length: continuation },
        () => "// Source continues below the outline.",
      ),
    ].join("\n");
    await routePreviewFiles(page, {
      "src/outline.rs": { text, bytes: strToU8(text) },
    });
    await page.goto("/team/project/blob/main/src/outline.rs");
    const symbols = page.getByRole("complementary", { name: "Code symbols" });
    await expect(
      symbols.getByRole("button", { name: "symbol_0 function 1", exact: true }),
    ).toBeVisible();
    await expectAlignedDivider();
    const grip = (await divider.locator(":scope > div").boundingBox())!;
    const middle = { x: grip.x + grip.width / 2, y: grip.y + grip.height / 2 };
    const width = (await symbols.boundingBox())!.width;
    // Start beside the grip, then drag vertically beyond its target.
    await page.mouse.move(middle.x + 10, middle.y);
    await expect(divider).toHaveAttribute("data-separator", "hover");
    await page.mouse.down();
    await page.mouse.move(middle.x - 50, middle.y + 120, { steps: 8 });
    await expect(divider).toHaveAttribute("data-separator", "active");
    await page.mouse.up();
    await expect
      .poll(async () => (await symbols.boundingBox())!.width)
      .toBeGreaterThan(width + 40);
    await expectAlignedDivider();
    const draggedWidth = (await symbols.boundingBox())!.width;
    await divider.press("ArrowRight");
    await expect
      .poll(async () => (await symbols.boundingBox())!.width)
      .toBeLessThan(draggedWidth - 10);
    await divider.dblclick();
    await expect
      .poll(async () => (await symbols.boundingBox())!.width)
      .toBe(320);
    await page.evaluate(() => window.scrollTo(0, 900));
    await expectAlignedDivider();
    await page.evaluate(() =>
      window.scrollTo({
        top: document.documentElement.scrollHeight,
        behavior: "instant",
      }),
    );
    await expectAlignedDivider();
    await expect(page.locator(".site-footer")).toBeInViewport();
    await expect(symbols.locator(".code-symbols-header")).toBeInViewport({
      ratio: 1,
    });
    await expect(
      symbols.getByRole("textbox", { name: "Filter symbols" }),
    ).toBeInViewport({ ratio: 1 });
    await expect(page.locator(".tree-sidebar-controls")).toBeInViewport({
      ratio: 1,
    });
    if (count === 1) {
      expect((await symbols.boundingBox())!.height).toBeLessThan(400);
    } else {
      const list = symbols.locator(".code-symbols-list");
      await expect
        .poll(() =>
          list.evaluate((node) => node.scrollHeight > node.clientHeight),
        )
        .toBe(true);
      const documentPosition = await page.evaluate(() => window.scrollY);
      await list.hover();
      await page.mouse.wheel(0, 10000);
      await expect
        .poll(() => list.evaluate((node) => node.scrollTop))
        .toBeGreaterThan(0);
      await page.mouse.wheel(0, -10000);
      await expect.poll(() => list.evaluate((node) => node.scrollTop)).toBe(0);
      expect(await page.evaluate(() => window.scrollY)).toBe(documentPosition);
    }
  }
  for (const height of [720, 1200]) {
    await page.setViewportSize({ width: 1440, height });
    for (const top of [0, 900, Number.MAX_SAFE_INTEGER]) {
      await page.evaluate((top) => window.scrollTo(0, top), top);
      await expectAlignedDivider();
    }
  }
});

test("file navigation stays joined to Code and Blame when the explorer toggles while scrolled", async ({
  page,
}) => {
  for (const width of [1440, 785]) {
    await page.setViewportSize({ width, height: 900 });
    await page.goto("/team/project/blob/main/src/navigation.rs#L82");
    await expect(
      page.locator('.file-source-code [data-line="82"]'),
    ).toBeInViewport();
    await page
      .getByRole("button", { name: "Close file tree", exact: true })
      .click();
    await page.evaluate(() =>
      window.scrollTo({ top: 1200, behavior: "instant" }),
    );

    const navigation = page.locator(".file-navigation");
    const toolbar = page.locator(".file-panel > .panel-header");
    const joinedHeader = async () => {
      await expect
        .poll(() =>
          navigation.evaluate((node) => node.getBoundingClientRect().top),
        )
        .toBe(0);
      await expect
        .poll(async () => {
          const [row, controls] = await Promise.all([
            navigation.boundingBox(),
            toolbar.boundingBox(),
          ]);
          return Math.abs(controls!.y - row!.y - row!.height);
        })
        .toBeLessThanOrEqual(1);
    };
    await joinedHeader();
    const openTree = page.getByRole("button", {
      name: "Open file tree",
      exact: true,
    });
    await expect(openTree).toBeInViewport();
    await page.getByRole("button", { name: "Blame", exact: true }).click();
    await expect(page.getByLabel("Blame view", { exact: true })).toBeVisible();
    await joinedHeader();
    await page.getByRole("button", { name: "Code", exact: true }).click();
    await expect(
      page.locator('.file-source-code [data-line="82"]'),
    ).toBeInViewport();
    await joinedHeader();
    await openTree.click();
    await joinedHeader();
    await expect(
      page.locator('.file-source-code [data-line="82"]'),
    ).toBeInViewport();
    if (width > 900) {
      const symbols = page.getByRole("complementary", { name: "Code symbols" });
      await expect
        .poll(async () => {
          const [pane, controls] = await Promise.all([
            symbols.boundingBox(),
            toolbar.boundingBox(),
          ]);
          return Math.abs(pane!.y - controls!.y);
        })
        .toBe(0);
      const [controls, pane] = await Promise.all([
        toolbar.boundingBox(),
        symbols.boundingBox(),
      ]);
      expect(controls!.x + controls!.width).toBeLessThanOrEqual(pane!.x);
    }
    expect(
      await page.evaluate(() => document.documentElement.scrollWidth),
    ).toBeLessThanOrEqual(width);
    await page.getByRole("button", { name: "Top", exact: true }).click();
    await expect.poll(() => page.evaluate(() => window.scrollY)).toBe(0);
    await expect(
      page.getByRole("button", { name: "Top", exact: true }),
    ).toBeHidden();
  }
});

test("symbol and line-hash navigation preserve the source horizontal scroll position", async ({
  page,
}) => {
  await page.setViewportSize({ width: 1133, height: 900 });
  await page.goto("/team/project/blob/main/src/navigation.rs#L82");
  const source = page.locator(".file-source-code [data-code]");
  const target = page.locator('.file-source-code [data-line="82"]');
  await expect(target).toBeInViewport();
  await expect(source).toHaveJSProperty("scrollLeft", 0);
  const symbols = page.getByRole("complementary", { name: "Code symbols" });

  for (const offset of [0, 120]) {
    await source.evaluate((node, left) => {
      node.scrollLeft = left;
    }, offset);
    await page.evaluate(() => window.scrollTo({ top: 0, behavior: "instant" }));
    await symbols
      .getByRole("button", { name: "navigate function 82", exact: true })
      .click();
    await expect(target).toHaveAttribute("data-code-symbol-target", "true");
    await expect(target).toBeInViewport();
    await expect
      .poll(() =>
        target.evaluate((node) =>
          Math.abs(
            node.getBoundingClientRect().top +
              node.getBoundingClientRect().height / 2 -
              innerHeight / 2,
          ),
        ),
      )
      .toBeLessThanOrEqual(1);
    await expect(source).toHaveJSProperty("scrollLeft", offset);
    await symbols
      .getByRole("button", { name: "All symbols", exact: true })
      .click();
  }
  await expect(page).toHaveURL(/#L82$/);
});

test("long file breadcrumbs keep complete entry names on one line and leave copying reachable", async ({
  page,
  context,
}) => {
  await context.grantPermissions(["clipboard-read", "clipboard-write"]);
  const path =
    "source/commands/history_recovery/fixtures/layered_catalog_reconstruction_tests.rs";
  await page.setViewportSize({ width: 917, height: 1314 });
  await page.goto(`/team/project/blob/main/${path}`);
  const trail = page.getByRole("navigation", {
    name: "File path",
    exact: true,
  });
  await expect(trail).toBeVisible();
  const entries = await trail.locator("a, strong").evaluateAll((elements) =>
    elements.map((element) => {
      const range = document.createRange();
      range.selectNodeContents(element);
      return {
        lines: range.getClientRects().length,
        width: element.clientWidth,
        full: element.scrollWidth,
      };
    }),
  );
  expect(entries.length).toBeGreaterThan(3);
  expect(
    entries.every((entry) => entry.lines === 1 && entry.width >= entry.full),
  ).toBe(true);
  await expect(trail.locator("strong")).toBeInViewport({ ratio: 1 });
  await page.getByRole("button", { name: "Copy path", exact: true }).click();
  await expect
    .poll(() => page.evaluate(() => navigator.clipboard.readText()))
    .toBe(path);
  await page.setViewportSize({ width: 390, height: 844 });
  await selectDarkTheme(page);
  await expect(
    page.getByRole("separator", { name: "Resize symbols pane" }),
  ).toHaveCount(0);
  expect(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= innerWidth,
    ),
  ).toBe(true);
  await expectNoAccessibilityViolations(page);
});

test("SQL workbench shares form controls and keeps chart axes keyboard accessible", async ({
  page,
}) => {
  await routePreviewFiles(page, {
    "metrics.csv": {
      text: null,
      bytes: strToU8(
        "name,category,score,count\nalpha,primary,0.91,2\nbeta,secondary,0.98,5\n",
      ),
    },
  });
  await page.goto("/team/project/blob/main/metrics.csv");
  const workbench = page.getByRole("region", {
    name: "metrics.csv query workbench",
  });
  await expect(workbench.getByRole("cell", { name: "alpha" })).toBeVisible();
  const editor = workbench.getByRole("textbox", { name: "SQL query" });
  const surface = workbench.locator(".sql-editor");
  const search = workbench.locator(".data-search");
  await workbench.getByRole("searchbox", { name: "Search rows" }).fill("beta");
  await expect(workbench.getByRole("cell", { name: "alpha" })).toHaveCount(0);
  await workbench.getByRole("searchbox", { name: "Search rows" }).clear();
  await expect(surface).toHaveCSS(
    "border-radius",
    await search.evaluate((node) => getComputedStyle(node).borderRadius),
  );
  await expect(surface).toHaveCSS("border-width", "1px");
  await editor.focus();
  await expect(surface).toHaveCSS("box-shadow", /0px 0px 0px 2px/);
  await expect(workbench.locator(".cm-editor")).toHaveCSS(
    "outline-style",
    "none",
  );
  await workbench.getByRole("button", { name: "Chart", exact: true }).click();
  const label = workbench.getByRole("combobox", { name: "Label", exact: true });
  const value = workbench.getByRole("combobox", { name: "Value", exact: true });

  for (const [width, theme] of [
    [1280, "light"],
    [390, "dark"],
  ] as const) {
    await page.setViewportSize({ width, height: 900 });
    await selectTheme(page, theme);
    await label.press("ArrowDown");
    const category = page.getByRole("option", {
      name: "category",
      exact: true,
    });
    await category.focus();
    await category.press("Enter");
    await expect(label).toBeFocused();
    await expect(label).toHaveText("category");
    await expect(workbench.locator(".data-chart-label").first()).toHaveText(
      "primary",
    );
    await value.press("ArrowDown");
    await expect(
      page.getByRole("option", { name: "category", exact: true }),
    ).toHaveCount(0);
    const count = page.getByRole("option", { name: "count", exact: true });
    await count.focus();
    await count.press("Enter");
    await expect(value).toBeFocused();
    await expect(
      workbench.getByRole("img", { name: "primary: 2", exact: true }),
    ).toBeVisible();
    await expect(
      workbench.locator(
        "select:visible, input:not([data-slot]):visible, button:not([data-slot]):visible",
      ),
    ).toHaveCount(0);
    expect(
      await page.evaluate(
        () => document.documentElement.scrollWidth <= innerWidth,
      ),
    ).toBe(true);
    expect(
      await editor
        .locator(".cm-line")
        .first()
        .evaluate((line) => {
          return (
            getComputedStyle(line, "::selection").color ===
            getComputedStyle(line).color
          );
        }),
    ).toBe(true);
    await expectNoAccessibilityViolations(page);
  }
});

test("Explain preserves query plans and the compact SQL header across engines", async ({
  page,
}) => {
  const SQL = await initSqlJs();
  const database = new SQL.Database();
  database.run(
    "CREATE TABLE data (name TEXT, score REAL); INSERT INTO data VALUES ('alpha', 1), ('beta', 2)",
  );
  const sqlite = database.export();
  database.close();
  await routePreviewFiles(page, {
    "plan.csv": { text: null, bytes: strToU8("name,score\nalpha,1\nbeta,2\n") },
    "plan.sqlite": { text: null, bytes: sqlite },
  });
  for (const extension of ["csv", "sqlite"]) {
    await page.goto(`/team/project/blob/main/plan.${extension}`);
    const workbench = page.getByRole("region", {
      name: `plan.${extension} query workbench`,
    });
    await expect(
      workbench.getByRole("cell", { name: "alpha", exact: true }),
    ).toBeVisible();
    const editor = workbench.getByRole("textbox", { name: "SQL query" });
    await editor.fill(
      "SELECT a.name, b.score FROM data a JOIN data b ON a.score = b.score",
    );
    await workbench
      .getByRole("button", { name: "Explain", exact: true })
      .click();
    const plan = workbench.getByRole("region", {
      name: "Query plan",
      exact: true,
    });
    await expect(plan).toBeVisible();
    await expect(
      workbench.getByRole("button", { name: "Chart", exact: true }),
    ).toHaveCount(0);
    await expect(
      workbench.getByText("Local data workbench", { exact: true }),
    ).toHaveCount(0);
    await expect(workbench.locator(".data-query-heading")).toContainText(
      extension === "sqlite" ? "SQLite" : "DuckDB",
    );
    await expect(editor).toHaveAccessibleDescription(/Read-only.*Ctrl\+Space/);
    if (extension === "csv") {
      const diagram = plan.locator("pre");
      await expect(diagram).toContainText("HASH_JOIN");
      await expect(diagram).toHaveCSS("white-space", "pre");
      expect((await diagram.innerText()).split("\n").length).toBeGreaterThan(
        10,
      );
      expect((await diagram.boundingBox())!.height).toBeGreaterThan(100);
    } else {
      await expect(
        plan.getByRole("columnheader", { name: "detail", exact: true }),
      ).toBeVisible();
      await expect(plan).toContainText("SCAN");
    }
    for (const [width, theme] of [
      [1280, "light"],
      [390, "dark"],
    ] as const) {
      await page.setViewportSize({ width, height: 900 });
      await selectTheme(page, theme);
      expect(
        await page.evaluate(() => document.documentElement.scrollWidth),
      ).toBeLessThanOrEqual(width);
      await expectNoAccessibilityViolations(page);
    }
    await editor.fill("SELECT missing_column FROM data");
    await workbench
      .getByRole("button", { name: "Run query", exact: true })
      .click();
    await expect(workbench.getByRole("alert")).toContainText(
      "Query did not run",
    );
    await expect(plan).toBeVisible();
    await workbench
      .getByRole("button", { name: "Count rows", exact: true })
      .click();
    await workbench
      .getByRole("button", { name: "Run query", exact: true })
      .click();
    await expect(
      workbench.getByRole("cell", { name: "2", exact: true }),
    ).toBeVisible();
    await expect(plan).toHaveCount(0);
    await expect(
      workbench.getByRole("button", { name: "Chart", exact: true }),
    ).toBeVisible();
  }
});

test("delimited workbenches preserve encoded text and align their headers", async ({
  page,
}) => {
  test.setTimeout(60_000);
  const columns = [
    "name",
    "amount (£)",
    ...Array.from({ length: 10 }, (_, index) => `field_${index}`),
  ];
  const csv = `${columns.join(",")}\ncafé,10,${Array(10).fill("1").join(",")}\n`;
  const latin1 = Uint8Array.from(csv, (character) => character.charCodeAt(0));
  const tsv = "name\tamount (£)\ncafé\t10\n";
  const utf16 = new Uint8Array(2 + tsv.length * 2);
  const units = new DataView(utf16.buffer);
  units.setUint16(0, 0xfeff, true);
  for (let index = 0; index < tsv.length; index++)
    units.setUint16(2 + index * 2, tsv.charCodeAt(index), true);
  const rangeRequests: string[] = [];
  page.on("request", (request) => {
    if (
      request.url().includes("/blob?") &&
      new URL(request.url()).searchParams.get("path_hex") ===
        pathHex("large.csv")
    )
      rangeRequests.push(request.headers()["range"] ?? "");
  });
  await routePreviewFiles(page, {
    "encoded.csv": { text: null, bytes: latin1 },
    "encoded.tsv": { text: null, bytes: utf16 },
    "large.csv": {
      text: null,
      bytes: strToU8("name,value\nlarge,1\n"),
      size: 5 * 1024 * 1024 * 1024,
    },
  });
  for (const name of ["encoded.csv", "encoded.tsv"]) {
    await page.goto(`/team/project/blob/main/${name}`);
    const workbench = page.getByRole("region", {
      name: `${name} query workbench`,
    });
    await expect(
      workbench.getByRole("columnheader", { name: "amount (£)", exact: true }),
    ).toBeVisible();
    await expect(
      workbench.getByRole("cell", { name: "café", exact: true }),
    ).toBeVisible();
    for (const width of [1440, 900]) {
      await page.setViewportSize({ width, height: 1000 });
      await expect
        .poll(() =>
          workbench.evaluate((node) => {
            const schema = node
              .querySelector(".data-schema header")!
              .getBoundingClientRect();
            const result = node
              .querySelector(".data-results-toolbar")!
              .getBoundingClientRect();
            return Math.abs(schema.height - result.height);
          }),
        )
        .toBeLessThanOrEqual(1);
    }
    await page.setViewportSize({ width: 640, height: 1000 });
    expect(
      await page.evaluate(() => document.documentElement.scrollWidth),
    ).toBeLessThanOrEqual(640);
    await expect(
      workbench.getByRole("button", { name: "Table", exact: true }),
    ).toBeVisible();
    await page.setViewportSize({ width: 1440, height: 1000 });
    await workbench.getByRole("button", { name: "Chart", exact: true }).click();
    await expect(
      workbench.getByRole("region", { name: "Query result chart" }),
    ).toBeVisible();
    await expectNoAccessibilityViolations(page);
  }
  await page.goto("/team/project/blob/main/large.csv");
  await expect(
    page.getByRole("cell", { name: "large", exact: true }),
  ).toBeVisible();
  expect(rangeRequests.some((value) => value.startsWith("bytes="))).toBe(true);
});

test("Git LFS previews load on request and respect the stored file size", async ({
  page,
}) => {
  const markdown =
    "# Stored Markdown\n\nThe payload came from Git LFS. [Sibling document](sibling.md)";
  const names = {
    "stored.md": markdown.length,
    "oversized.md": 2 * 1024 * 1024,
    "large.csv": 51 * 1024 * 1024,
    "unknown-size.csv": undefined,
  };
  const commit = "e".repeat(40);
  const artifactReads: string[] = [];
  await page.route(
    "**/api/repos/team/project/file-preview?*",
    async (route) => {
      const path = new URL(route.request().url()).searchParams.get("path_hex");
      const file = Object.entries(names).find(
        ([name]) => pathHex(name) === path,
      );
      if (!file) return route.fallback();
      const pointer = `version https://git-lfs.github.com/spec/v1\noid sha256:${"f".repeat(64)}\nsize ${file[1] ?? 100_000_000}\n`;
      return route.fulfill({
        json: {
          oid: pathOid,
          size: pointer.length,
          stored_size: file[1],
          mode: "100644",
          classification: "LfsPointer",
          preview: { state: "inline", text: pointer },
          commit,
          generation: 1,
        } satisfies FilePreview,
      });
    },
  );
  await page.route("**/api/repos/team/project/lfs-content?*", async (route) => {
    artifactReads.push(route.request().url());
    return route.fulfill({
      body: markdown,
      contentType: "application/octet-stream",
    });
  });

  await page.goto("/team/project/blob/main/stored.md");
  await expect(
    page.getByRole("button", { name: "Pointer", exact: true }),
  ).toHaveAttribute("aria-pressed", "true");
  expect(artifactReads).toEqual([]);
  await page.getByRole("button", { name: "Preview", exact: true }).click();
  await expect(
    page.getByRole("heading", { name: "Stored Markdown" }),
  ).toBeVisible();
  await expect(
    page.getByRole("link", { name: "Sibling document" }),
  ).toHaveAttribute("href", `/team/project/blob/${commit}/sibling.md`);
  await expect(page.getByRole("link", { name: "Raw pointer" })).toHaveAttribute(
    "href",
    `/api/repos/team/project/blob?rev=${commit}&path_hex=${pathHex("stored.md")}`,
  );
  expect(
    new Set(artifactReads.map((url) => new URL(url).searchParams.get("rev"))),
  ).toEqual(new Set([commit]));
  const previewReads = artifactReads.length;

  for (const [name, limit] of [
    ["oversized.md", "1.0 MB"],
    ["large.csv", "50.0 MB"],
  ]) {
    await page.goto(`/team/project/blob/main/${name}`);
    await page.getByRole("button", { name: "Preview", exact: true }).click();
    await expect(
      page.getByText(`Preview is limited to ${limit}`),
    ).toBeVisible();
    await expect(
      page.getByRole("link", { name: "Download Git LFS file" }),
    ).toBeVisible();
  }
  await page.goto("/team/project/blob/main/unknown-size.csv");
  await expect(
    page.getByRole("link", { name: "Download Git LFS file" }),
  ).toBeVisible();
  await expect(
    page.getByRole("button", { name: "Preview", exact: true }),
  ).toHaveCount(0);
  expect(artifactReads).toHaveLength(previewReads);
});

test("format-aware previews explore data, office files, media, and databases locally", async ({
  page,
}) => {
  test.setTimeout(60_000);
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

  await page.goto(`/team/project/blob/main/metrics.csv`);
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
    csvWorkbench.getByRole("region", { name: "Query plan", exact: true }),
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

  await page.goto(`/team/project/blob/main/diagram.svg`);
  const image = page.getByRole("img", { name: "Preview of diagram.svg" });
  await expect(image).toBeVisible();
  await expect
    .poll(() =>
      image.evaluate((node) => (node as HTMLImageElement).naturalWidth),
    )
    .toBe(80);
  const zoom = page.getByRole("slider", { name: "Image zoom" });
  await zoom.press("ArrowRight");
  await expect(zoom).toHaveAttribute("aria-valuenow", "125");

  await page.goto(`/team/project/blob/main/report.xlsx`);
  await expect(
    page.getByRole("navigation", { name: "Workbook sheets" }),
  ).toContainText("Experiments");
  await expect(page.getByRole("cell", { name: "0.42" })).toBeVisible();

  await page.goto(`/team/project/blob/main/runs.sqlite`);
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
  await sqliteWorkbench
    .getByRole("searchbox", { name: "Filter columns" })
    .fill("score");
  await expect(
    sqliteWorkbench.getByTitle("Insert model into the query"),
  ).toHaveCount(0);
  await sqliteWorkbench
    .getByRole("searchbox", { name: "Filter columns" })
    .clear();
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

  await page.goto(`/team/project/blob/main/features.parquet`);
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

  await page.goto(`/team/project/blob/main/batch.arrow`);
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

  await page.goto(`/team/project/blob/main/handbook.pdf`);
  await expect(
    page.getByRole("img", { name: "Page 1 of handbook.pdf" }),
  ).toBeVisible();
  await expect(page.getByText("Page 1 of 1")).toBeVisible();
  await expect(page.getByText("Private in-browser PDF preview")).toBeAttached();

  await page.goto(`/team/project/blob/main/model.onnx`);
  await expect(page.getByText("ONNX model")).toBeVisible();
  await expect(
    page.getByRole("heading", { name: "Hex and ASCII" }),
  ).toBeVisible();
  await expect(page.getByText(/00000000\s+08 03 12 00 ff 00/)).toBeVisible();
});

test("audio previews play and pause using the shared media controls", async ({
  page,
}) => {
  const rate = 8000;
  const audio = Buffer.alloc(44 + 4 * rate, 128);
  audio.write("RIFF", 0);
  audio.writeUInt32LE(audio.length - 8, 4);
  audio.write("WAVEfmt ", 8);
  audio.writeUInt32LE(16, 16);
  audio.writeUInt16LE(1, 20);
  audio.writeUInt16LE(1, 22);
  audio.writeUInt32LE(rate, 24);
  audio.writeUInt32LE(rate, 28);
  audio.writeUInt16LE(1, 32);
  audio.writeUInt16LE(8, 34);
  audio.write("data", 36);
  audio.writeUInt32LE(audio.length - 44, 40);
  await routePreviewFiles(page, { "sample.wav": { text: null, bytes: audio } });

  await page.goto("/team/project/blob/main/sample.wav");
  const player = page.getByRole("region", {
    name: "Audio preview of sample.wav",
  });
  const position = player.getByRole("slider", { name: "Playback position" });
  await expect(position).toHaveAttribute("aria-valuemax", "4");
  await player.getByRole("button", { name: "Play", exact: true }).click();
  await expect
    .poll(() =>
      player
        .locator("audio")
        .evaluate((node: HTMLAudioElement) => node.currentTime),
    )
    .toBeGreaterThan(0);
  await player.getByRole("button", { name: "Pause", exact: true }).click();
  await expect(
    player.getByRole("button", { name: "Play", exact: true }),
  ).toBeVisible();
  await chooseSelectOption(
    player.getByRole("combobox", { name: "Playback speed" }),
    "1.5",
  );
  await expect
    .poll(() =>
      player
        .locator("audio")
        .evaluate((node: HTMLAudioElement) => node.playbackRate),
    )
    .toBe(1.5);
});

test("blame and source panes resize with pointer and keyboard controls", async ({
  page,
}) => {
  await page.goto(`/team/project/blob/main/README.md`);
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
  const grabX = handle.x + 2;
  await page.mouse.move(grabX, handle.y + 20);
  await page.mouse.down();
  await page.mouse.move(grabX + 1, handle.y + 20);
  await expect(separator).toHaveAttribute("aria-valuenow", "48");
  await page.mouse.move(grabX + 120, handle.y + 20);
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

  const touch = await page.context().newCDPSession(page);
  const sendTouches = (
    type: "touchStart" | "touchMove" | "touchEnd" | "touchCancel",
    touchPoints: { id: number; x: number; y: number }[],
  ) => touch.send("Input.dispatchTouchEvent", { type, touchPoints });
  try {
    await touch.send("Emulation.setTouchEmulationEnabled", {
      enabled: true,
      maxTouchPoints: 2,
    });
    const box = (await separator.boundingBox())!;
    const first = {
      id: 1,
      x: box.x + box.width / 2,
      y: box.y + box.height / 2,
    };
    const moved = { ...first, x: first.x + 40 };
    await sendTouches("touchStart", [first]);
    await sendTouches("touchMove", [moved]);
    await expect
      .poll(async () => Number(await separator.getAttribute("aria-valuenow")))
      .toBeGreaterThan(48);
    const ownedValue = await separator.getAttribute("aria-valuenow");
    const current = (await separator.boundingBox())!;
    const second = { id: 2, x: current.x + current.width / 2, y: moved.y + 20 };
    // Capture the secondary native pointer before React rejects its pointerdown.
    // Its later capture loss must not end the owning pointer's drag.
    await separator.evaluate((node) => {
      node.addEventListener(
        "pointerdown",
        (event) => {
          const pointerId = (event as PointerEvent).pointerId;
          node.setAttribute("data-captured-pointer", String(pointerId));
          node.setPointerCapture(pointerId);
        },
        { once: true },
      );
    });
    await sendTouches("touchStart", [moved, second]);
    await sendTouches("touchMove", [moved, { ...second, x: second.x + 80 }]);
    await expect(separator).toHaveAttribute("aria-valuenow", ownedValue!);
    const captured = Number(
      await separator.getAttribute("data-captured-pointer"),
    );
    expect(captured).toBeGreaterThan(0);
    await separator.evaluate(
      (node, pointerId) => node.releasePointerCapture(pointerId),
      captured,
    );
    const secondaryMoved = { ...second, x: second.x + 81 };
    await sendTouches("touchMove", [moved, secondaryMoved]);
    await sendTouches("touchMove", [
      { ...moved, x: moved.x + 40 },
      secondaryMoved,
    ]);
    await expect
      .poll(async () => Number(await separator.getAttribute("aria-valuenow")))
      .toBeGreaterThan(Number(ownedValue));
    const beforeCancel = await separator.getAttribute("aria-valuenow");
    await sendTouches("touchCancel", []);

    const resumed = {
      ...first,
      x: (await separator.boundingBox())!.x + box.width / 2,
    };
    await sendTouches("touchStart", [resumed]);
    await sendTouches("touchMove", [{ ...resumed, x: resumed.x + 40 }]);
    await expect
      .poll(async () => Number(await separator.getAttribute("aria-valuenow")))
      .toBeGreaterThan(Number(beforeCancel));
    await sendTouches("touchEnd", []);
  } finally {
    await separator.evaluate((node) =>
      node.removeAttribute("data-captured-pointer"),
    );
    await touch.detach();
  }
});

test("file tree controls align the branch picker and place collapse after search", async ({
  page,
}) => {
  await page.goto(`/team/project/blob/main/README.md`);
  const sidebar = page.locator(".tree-sidebar");
  await expect(sidebar.getByText("Files", { exact: true })).toHaveCount(0);
  const branch = sidebar.getByRole("button", {
    name: /^Switch branches or tags/,
  });
  const search = sidebar.getByRole("button", { name: "Focus file search" });
  const collapse = sidebar.getByRole("button", { name: "Close file tree" });
  const searchBounds = await search.boundingBox();
  const collapseBounds = await collapse.boundingBox();
  expect(collapseBounds!.x).toBeGreaterThan(
    searchBounds!.x + searchBounds!.width,
  );
  expect(collapseBounds!.y).toBe(searchBounds!.y);
  const iconOffset = await branch.evaluate((node) => {
    const icon = node.querySelector('[data-icon="inline-start"]')!;
    return (
      icon.getBoundingClientRect().left - node.getBoundingClientRect().left
    );
  });
  expect(iconOffset).toBeLessThan(16);
  const arrowOffset = await branch.evaluate((node) => {
    const arrow = node.querySelector('[data-icon="inline-end"]')!;
    return (
      node.getBoundingClientRect().right - arrow.getBoundingClientRect().right
    );
  });
  expect(arrowOffset).toBeLessThan(16);

  await branch.click();
  const dialog = page.getByRole("dialog", { name: "Switch branches/tags" });
  await expect(dialog).toBeVisible();
  expect((await dialog.boundingBox())!.x).toBeCloseTo(
    (await branch.boundingBox())!.x,
    0,
  );
  await page.keyboard.press("Escape");
  await search.click();
  await expect(page.locator("#repository-tree-search")).toBeFocused();
});

test("a short file leaves the tree usable after reopening and resizing", async ({
  page,
}) => {
  await page.setViewportSize({ width: 1280, height: 1100 });
  const text = "# Short file\n";
  await routePreviewFiles(page, {
    "README.md": { text, bytes: strToU8(text) },
  });
  await page.goto("/team/project/blob/main/README.md");
  await expect(
    page.getByRole("region", { name: "File README.md" }),
  ).toBeVisible();
  await page.getByRole("button", { name: "Close file tree" }).click();
  await page.getByRole("button", { name: "Open file tree" }).click();

  const sidebar = page.locator(".tree-sidebar");
  const footer = page.locator(".site-footer");
  for (const height of [1100, 1280]) {
    await page.setViewportSize({ width: 1280, height });
    await expect
      .poll(async () => {
        const pane = (await sidebar.boundingBox())!;
        const bottom = (await footer.boundingBox())!;
        return Math.max(
          Math.abs(pane.y + pane.height - bottom.y),
          Math.abs(bottom.y + bottom.height - height),
        );
      })
      .toBeLessThanOrEqual(1);
  }
  await page.setViewportSize({ width: 390, height: 844 });
  await expect(sidebar.locator(".repository-tree")).toHaveCSS(
    "height",
    "300px",
  );
  await sidebar.getByRole("combobox", { name: "Go to file" }).fill("readme");
  await expect(
    sidebar.getByRole("option", { name: "README.md" }),
  ).toBeInViewport();
});

test("file tree scrolls independently while its resize grip stays viewport-centered", async ({
  page,
}) => {
  const source = "A line of repository content.\n".repeat(200);
  await routePreviewFiles(page, {
    "long.txt": { text: source, bytes: strToU8(source) },
  });
  await page.route("**/api/repos/team/project/tree?*", (route) =>
    route.fulfill({
      json: {
        items: Array.from({ length: 100 }, (_, index) => {
          const path = `file-${index}.txt`;
          return {
            path,
            path_hex: pathHex(path),
            kind: "Blob",
            oid,
            mode: "100644",
          };
        }),
        next: null,
        commit: oid,
        generation: 1,
        directory_oid: oid,
      },
    }),
  );
  await page.goto(`/team/project/blob/main/long.txt`);
  const sidebar = page.locator(".tree-sidebar");
  const separator = page.getByRole("separator", {
    name: "Resize file tree pane",
  });
  const treeScroll = sidebar.locator(
    '[data-file-tree-virtualized-scroll="true"]',
  );
  await expect(treeScroll).toBeVisible();
  await expect(treeScroll).toHaveCSS("overscroll-behavior-y", "contain");
  const pagePosition = await page.evaluate(() => window.scrollY);
  const scrollport = (await treeScroll.boundingBox())!;
  const scrollbarX = scrollport.x + scrollport.width - 3;
  await page.mouse.move(scrollbarX, scrollport.y + 40);
  await page.mouse.wheel(0, 500);
  await expect
    .poll(() => treeScroll.evaluate((node) => node.scrollTop))
    .toBeGreaterThan(0);
  await expect(separator).toHaveAttribute("aria-valuenow", "320");
  await treeScroll.hover();
  await page.mouse.wheel(0, 10000);
  await expect
    .poll(() =>
      treeScroll.evaluate(
        (node) => node.scrollHeight - node.clientHeight - node.scrollTop,
      ),
    )
    .toBe(0);
  const endPosition = await treeScroll.evaluate((node) => node.scrollTop);
  await expect
    .poll(() => page.evaluate(() => window.scrollY))
    .toBe(pagePosition);

  // Leave room for the scrollport to grow as the repository header scrolls out.
  // A list already at its bottom would naturally clamp its previous offset.
  await page.mouse.wheel(0, -500);
  await expect
    .poll(() => treeScroll.evaluate((node) => node.scrollTop))
    .toBeLessThan(endPosition);
  const treePosition = await treeScroll.evaluate((node) => node.scrollTop);

  for (const scrollTop of [900, Number.MAX_SAFE_INTEGER]) {
    await page.evaluate((top) => window.scrollTo(0, top), scrollTop);
    await expect
      .poll(async () => {
        const pane = (await sidebar.boundingBox())!;
        const divider = (await separator.boundingBox())!;
        const grip = (await separator
          .locator(".pane-resizer-target")
          .boundingBox())!;
        return Math.max(
          Math.abs(divider.y + divider.height / 2 - (pane.y + pane.height / 2)),
          Math.abs(grip.y + grip.height / 2 - page.viewportSize()!.height / 2),
        );
      })
      .toBeLessThanOrEqual(1);
    await expect
      .poll(() => treeScroll.evaluate((node) => node.scrollTop))
      .toBe(treePosition);
  }
  await expect(separator).toBeInViewport();
  await separator.press("ArrowRight");
  await expect(separator).toHaveAttribute("aria-valuenow", "344");
});

test("file tree and content panes resize with pointer and keyboard controls", async ({
  page,
}) => {
  await page.goto(`/team/project/blob/main/README.md`);

  const separator = page.getByRole("separator", {
    name: "Resize file tree pane",
  });
  const treePane = page.locator(".tree-sidebar");
  const contentPane = page.locator(".code-main");
  await expect(separator).toHaveAttribute("aria-valuenow", "320");
  await expect(treePane).toBeVisible();
  await expect(contentPane).toBeVisible();
  await expectNoAccessibilityViolations(page);

  const initialWidth = (await treePane.boundingBox())?.width ?? 0;
  const handle = await separator.locator(".pane-resizer-target").boundingBox();
  if (!handle) throw new Error("File tree resize handle is not visible");
  const grabX = handle.x + 2;
  const grabY = handle.y + handle.height / 2;
  await page.mouse.move(grabX, grabY);
  await page.mouse.down();
  await page.mouse.move(grabX + 1, grabY);
  await expect(separator).toHaveAttribute("aria-valuenow", "321");
  await page.mouse.move(grabX + 120, grabY);
  await page.mouse.up();
  await expect
    .poll(async () => (await treePane.boundingBox())?.width ?? 0)
    .toBeGreaterThan(initialWidth + 80);

  await separator.press("Home");
  await expect(separator).toHaveAttribute("aria-valuenow", "240");
  await separator.press("ArrowRight");
  await expect(separator).toHaveAttribute("aria-valuenow", "264");
  await separator.locator(".pane-resizer-target").dblclick();
  await expect(separator).toHaveAttribute("aria-valuenow", "320");

  await page.setViewportSize({ width: 600, height: 900 });
  await expect(separator).toBeHidden();
  await expect(treePane).toBeVisible();
  await expect(contentPane).toBeVisible();
  await expectNoAccessibilityViolations(page);
});

test("blame commit messages open their commit details", async ({ page }) => {
  await page.goto(`/team/project/blob/main/README.md`);
  await page.getByRole("button", { name: "Blame", exact: true }).click();

  const message = page.getByRole("link", {
    name: "Start the project documentation",
    exact: true,
  });
  await expect(message).toHaveAttribute(
    "href",
    `/team/project/commit/${"1".repeat(40)}`,
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
  const headerMark = page.locator(".global-header .brand-mark");
  const footer = page.locator(".site-footer");
  const brand = footer.getByRole("link", { name: "Crab repositories" });
  const footerMark = brand.locator(".footer-brand-mark");
  const tagline = footer.getByText("Git for any file at any scale");
  await expect(headerMark).toBeVisible();
  await expect(footerMark).toBeVisible();
  expect(
    await headerMark.evaluate((image: HTMLImageElement) => image.naturalWidth),
  ).toBeGreaterThan(0);
  expect(
    await footerMark.evaluate((image: HTMLImageElement) => image.naturalWidth),
  ).toBeGreaterThan(0);
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

test("code search preserves scope and links each match to the searched commit", async ({
  page,
}) => {
  const expression = "render NOT is:archived";
  let continuationAttempts = 0;
  await page.route(
    (url) => url.pathname === "/api/repos/team/project/code-search",
    (route) => {
      const request = new URL(route.request().url());
      expect(request.searchParams.get("q")).toBe(expression);
      expect(request.searchParams.get("kind")).toBe("symbol");
      expect(request.searchParams.get("path")).toBe("src");
      expect(request.searchParams.get("rev")).toBe("refs/heads/main");
      expect(request.searchParams.get("case_sensitive")).toBe("true");
      const cursor = request.searchParams.get("cursor");
      if (cursor) {
        expect(cursor).toBe("signed-search-cursor");
        if (++continuationAttempts === 1)
          return route.fulfill({
            status: 503,
            json: { error: { message: "Search is temporarily unavailable" } },
          });
      }
      const item = cursor ? "src/index.ts" : "src/render.ts";
      return route.fulfill({
        json: {
          commit: oid,
          scanned: cursor ? 1 : 3,
          skipped: cursor ? 0 : 1,
          next: cursor ? null : "signed-search-cursor",
          items: [
            {
              path: item,
              path_hex: pathHex(item),
              more_lines: false,
              lines: [
                {
                  line: cursor ? 7 : 2,
                  before: "export function ",
                  matched: "render",
                  after: "() {}",
                },
              ],
            },
          ],
        },
      });
    },
  );
  await page.goto("/team/project");
  await page
    .locator(".repo-header")
    .getByRole("link", { name: "Search code", exact: true })
    .click();
  const form = page.getByRole("search", { name: "Search repository code" });
  await form.getByLabel("Search text").fill(expression);
  await chooseSelectOption(form.getByLabel("Search in"), "symbol");
  await form.getByLabel("Directory (optional)").fill("src");
  await form.getByRole("checkbox", { name: "Match case" }).check();
  await form.getByRole("button", { name: "Search", exact: true }).click();
  await expect(page).toHaveURL(/code-search\?.*kind=symbol/);
  const results = page.getByRole("region", { name: "Code search results" });
  await expect(
    results.getByText("1 matching file", { exact: false }),
  ).toBeVisible();
  await expect(
    results.getByText("Skipped 1 file", { exact: false }),
  ).toBeVisible();
  await expect(results.locator("mark")).toHaveText("render");
  const next = results.getByRole("button", { name: "Scan next page" });
  await next.click();
  await expect(results.getByRole("alert")).toHaveText(
    "Search is temporarily unavailable",
  );
  await expect(
    results.getByRole("link", { name: "src/render.ts" }),
  ).toBeVisible();
  await next.click();
  await expect(
    results.getByText("2 matching files", { exact: false }),
  ).toBeVisible();
  await expectNoAccessibilityViolations(page);
  const line = results.locator("a[href$='#L7']");
  await expect(line).toHaveAttribute(
    "href",
    `/team/project/blob/${oid}/src/index.ts#L7`,
  );
  await line.click();
  await expect(page).toHaveURL(`/team/project/blob/${oid}/src/index.ts#L7`);
  await page.goBack();
  await expect(form.getByLabel("Search text")).toHaveValue(expression);
});

test("Go to file finds a deep repository path before its directory is expanded", async ({
  page,
}) => {
  await page.setExtraHTTPHeaders({ "x-test-latest-commit": "fail-once" });
  await page.goto("/team/project");
  await expect(
    page.getByRole("button", { name: "Go to file T", exact: true }),
  ).toBeVisible();
  await page.keyboard.press("t");
  await expect(page.getByPlaceholder("Go to file")).toBeFocused();
  await page.getByPlaceholder("Go to file").fill("index");
  const result = page
    .getByLabel("Repository file search results")
    .getByRole("option", { name: "src/index.ts" });
  await expect(result).toBeVisible();
  await page.getByPlaceholder("Go to file").press("Enter");
  await expect(page).toHaveURL(/\/blob\/main\/src\/index\.ts$/);
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
  await page.goto(`/team/project/blob/main/src/index.ts`);
  const tree = page.locator('[aria-label="Repository files"]');
  // Directory loading and the selection frame finish after the tree mounts.
  await expect(
    tree.getByRole("treeitem", { name: "index.ts", exact: true }),
  ).toHaveAttribute("aria-selected", "true");
  const state = await tree.evaluate((tree) => {
    const rows = [
      ...(tree.shadowRoot?.querySelectorAll('[role="treeitem"]') ?? []),
    ];
    const active = rows.find(
      (row) => row.getAttribute("aria-selected") === "true",
    );
    return {
      active: active?.getAttribute("aria-label"),
      expanded: rows
        .filter((row) => row.getAttribute("aria-expanded") === "true")
        .map((row) => row.getAttribute("aria-label")),
    };
  });
  expect(state.active).toBe("index.ts");
  expect(state.expanded).toContain("src");
});

test("one directory click selects, expands, and loads its children", async ({
  page,
}) => {
  const treeRequests: string[] = [];
  page.on("request", (request) => {
    if (new URL(request.url()).pathname.endsWith("/tree"))
      treeRequests.push(request.url());
  });
  await page.goto(`/team/project/blob/main/README.md`);
  const tree = page.locator('[aria-label="Repository files"]');
  const directory = tree.getByRole("treeitem", {
    name: "src",
    exact: true,
  });
  await expect(directory).toHaveAttribute("aria-expanded", "false");
  await page.waitForLoadState("networkidle");
  treeRequests.length = 0;

  await directory.click();

  await expect(page).toHaveURL("/team/project/tree/main/src");
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

test("inline file creation keeps editor settings and drafts through preview and commit review", async ({
  page,
}) => {
  await page.goto("/team/project/new/main/src");
  await expect(page.locator('[aria-label="Repository files"]')).toBeVisible();
  const filename = page.getByRole("textbox", {
    name: "File name",
    exact: true,
  });
  const editor = page.getByRole("textbox", {
    name: "File content",
    exact: true,
  });
  const commit = page.getByRole("button", {
    name: "Commit changes…",
    exact: true,
  });
  await expect(commit).toBeDisabled();
  await filename.fill("guide.md");
  await page.getByRole("combobox", { name: "Tab size", exact: true }).click();
  await page.getByRole("option", { name: "4", exact: true }).click();
  await editor.fill("alpha");
  await editor.press("Home");
  await editor.press("Tab");
  await expect(editor).toHaveText(/^ {4}alpha$/);
  await editor.press("ControlOrMeta+z");
  await expect(editor).toHaveText("alpha");
  await page
    .getByRole("combobox", { name: "Line wrapping", exact: true })
    .click();
  await page.getByRole("option", { name: "Soft wrap", exact: true }).click();
  await expect(editor).toHaveCSS("white-space", "break-spaces");
  const draft = "# Draft guide\n\n[Readme](../README.md)\n";
  await editor.fill(draft);
  await page.getByRole("tab", { name: "Preview", exact: true }).click();
  await expect(
    page.getByRole("heading", { name: "Draft guide" }),
  ).toBeVisible();
  await expect(
    page.getByRole("link", { name: "Readme", exact: true }),
  ).toHaveAttribute("href", "/team/project/blob/main/README.md");
  await page.getByRole("tab", { name: "Edit", exact: true }).click();
  await expect(editor.locator(".cm-line")).toHaveText(draft.split("\n"));
  await editor.focus();
  await editor.press("Escape");
  await editor.press("Tab");
  await expect(editor).not.toBeFocused();
  await commit.click();
  const dialog = page.getByRole("dialog", {
    name: "Commit changes",
    exact: true,
  });
  await expect(dialog.getByLabel("Commit message")).toHaveValue(
    "Create guide.md",
  );
  await dialog.getByLabel("Commit message").fill("Add the draft guide");
  await dialog
    .getByRole("button", { name: "Cancel changes", exact: true })
    .click();
  await expect(commit).toBeFocused();
  await expect(editor.locator(".cm-line")).toHaveText(draft.split("\n"));
  await commit.click();
  await expect(dialog.getByLabel("Commit message")).toHaveValue(
    "Add the draft guide",
  );
  await page.route(
    (url) => url.pathname.endsWith("/contents"),
    async (route) => {
      if (route.request().method() !== "POST") return route.fallback();
      expect(route.request().postDataJSON()).toEqual({
        branch: "refs/heads/main",
        expected_head: oid,
        path_hex: pathHex("src/guide.md"),
        content: draft,
        message: "Add the draft guide",
      });
      await route.fulfill({
        status: 409,
        json: {
          error: {
            message: "The branch changed. Your draft is still available.",
          },
        },
      });
    },
  );
  await dialog
    .getByRole("button", { name: "Commit changes", exact: true })
    .click();
  await expect(dialog.getByRole("alert")).toContainText("The branch changed");
  await dialog
    .getByRole("button", { name: "Cancel changes", exact: true })
    .click();
  await expect(editor.locator(".cm-line")).toHaveText(draft.split("\n"));
  for (const [width, theme] of [
    [1280, "light"],
    [390, "dark"],
  ] as const) {
    await page.setViewportSize({ width, height: 900 });
    await selectTheme(page, theme);
    expect(
      await page.evaluate(
        () => document.documentElement.scrollWidth <= innerWidth,
      ),
    ).toBe(true);
    await expectNoAccessibilityViolations(page);
    await commit.click();
    await dialog.evaluate((element) =>
      Promise.all(
        element.getAnimations().map((animation) => animation.finished),
      ),
    );
    await expectNoAccessibilityViolations(page);
    await dialog
      .getByRole("button", { name: "Cancel changes", exact: true })
      .click();
    expect(
      await editor
        .locator(".cm-line")
        .first()
        .evaluate((line) => {
          return (
            getComputedStyle(line, "::selection").color ===
            getComputedStyle(line).color
          );
        }),
    ).toBe(true);
  }
});

test("file-tree add menu commits a file and keeps GitHub control spacing", async ({
  context,
  page,
}) => {
  await context.grantPermissions(["clipboard-read", "clipboard-write"]);
  await page.goto(`/team/project/blob/main/README.md`);
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
  await page
    .getByRole("button", { name: "Commit changes…", exact: true })
    .click();
  await page.getByLabel("Commit message").fill("Create NEW.md");
  await page.getByRole("button", { name: "Commit changes" }).click();
  await expect(page).toHaveURL(/\/blob\/main\/NEW\.md$/);
  await expect(page.locator(".breadcrumb")).toContainText("project/NEW.md");
  await page.getByRole("button", { name: "Copy file contents" }).click();
  await expect
    .poll(() => page.evaluate(() => navigator.clipboard.readText()))
    .toBe("Hello, team!");
});

test("file-tree add menu uploads text and binary files in one commit", async ({
  page,
}) => {
  await page.goto(`/team/project/blob/main/README.md`);
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
  await expect(page).toHaveURL(/\/(tree|blob)\/main(?:\/|$)/);
  await expect(
    page.getByRole("region", { name: "Folders and files" }),
  ).toContainText("raw.bin");
});

test("protected file edits create a review branch and open the exact comparison", async ({
  page,
}) => {
  await page.goto(`/team/project/edit/main/README.md?scenario=protected`);
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
  await expect(page).toHaveURL(/\/compare\//);
  await expect(page.getByRole("combobox", { name: "base:" })).toHaveText(
    "main",
  );
  await expect(page.getByRole("combobox", { name: "compare:" })).toHaveText(
    "docs/readme-review",
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
  await page.goto(`/team/project/blob/main/README.md`);
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
  const direct = page.getByRole("radio", { name: "Commit directly to main" });
  const propose = page.getByRole("radio", {
    name: "Create a new branch for this commit and start a pull request",
  });
  await direct.focus();
  await direct.press("ArrowDown");
  await expect(propose).toBeChecked();
  await expect(page.getByLabel("New branch name")).toBeVisible();
  await expect(
    page.getByRole("button", { name: "Propose changes" }),
  ).toBeDisabled();
  await propose.press("ArrowUp");
  await expect(direct).toBeChecked();
  await expect(page.getByLabel("New branch name")).toHaveCount(0);
  await page.getByLabel("Commit message").fill("Delete README");
  await page.getByRole("button", { name: "Commit changes" }).click();
  await expect(page).toHaveURL("/team/project/tree/main");
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
    await page.locator(".clone-menu [data-slot=collapsible-trigger]").click();
    await expect(
      page.getByLabel("Repository URL", { exact: true }),
    ).toHaveValue(`${new URL(page.url()).origin}/git/team/project.git`);
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
    await page.locator(".clone-menu [data-slot=collapsible-trigger]").click();
  }
  await page.reload();
  await expect(page.locator("html")).toHaveCSS("color-scheme", "light");
});

test("issues follow the GitHub list hierarchy in both themes and on mobile", async ({
  page,
}) => {
  await page.goto("/team/project/issues");
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

  await page.getByRole("link", { name: /^Closed \d+$/, exact: true }).click();
  await expect(page).toHaveURL(/q=is%3Aclosed/);
  await expect(
    page.getByRole("link", { name: /^Closed \d+$/, exact: true }),
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
  await expect(page).toHaveURL(/\/tree\/release%2F/);
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
              id: "00000000-0000-4000-8000-000000000099",
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
                {
                  branch: "alpha",
                  required_approvals: 1,
                  required_checks: [],
                  allow_deletions: true,
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
    (url) => url.pathname === "/api/repos/team/project/branches/deletion",
    (route) => route.fulfill({ json: { retarget: null } }),
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
  await expect(page).toHaveURL(/\/branches$/);
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
  ).toHaveAttribute("href", "/team/project/compare/main...alpha");
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
  await expect(page).toHaveURL(/\/tags$/);
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

test("repository settings sections have space between them on desktop and mobile", async ({
  page,
}) => {
  await page.goto("/team/project/settings");
  const panels = page.locator(".settings-content > .settings-section");
  await expect(
    page.getByRole("region", { name: "Pull request merge methods" }),
  ).toBeVisible();
  for (const width of [1205, 390]) {
    await page.setViewportSize({ width, height: 850 });
    const gaps = await panels.evaluateAll((panels) =>
      panels
        .slice(1)
        .map(
          (panel, index) =>
            panel.getBoundingClientRect().top -
            panels[index].getBoundingClientRect().bottom,
        ),
    );
    expect(gaps.length).toBeGreaterThanOrEqual(4);
    for (const gap of gaps) expect(gap).toBeGreaterThanOrEqual(16);
  }
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

  await page.goto("/team/project/settings");
  await expect(
    page.getByRole("link", { name: "Settings", exact: true }),
  ).toHaveAttribute("aria-current", "page");
  await expect(
    page.getByRole("heading", { name: "Default branch" }),
  ).toBeVisible();
  await expect(page.locator(".default-branch-current")).toContainText("main");
  await page.getByRole("button", { name: "Change" }).click();
  await chooseSelectOption(
    page.getByRole("combobox", { name: "Choose a branch" }),
    "refs/heads/feature/docs",
  );
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

  await page.goto("/team/project/settings");
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
  await page.goto("/team/project/issues/new");
  await expect(
    page.getByRole("heading", { name: "This repository is read-only" }),
  ).toBeVisible();

  await page.goto("/team/project/settings");
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

  await page.goto("/team/project/settings/branches");
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
  await chooseSelectOption(page.getByLabel("Required approving reviews"), "2");
  const queue = page.getByRole("checkbox", { name: "Require merge queue" });
  const allowDeletions = page.getByRole("checkbox", {
    name: "Allow deletions",
  });
  await expect(queue).toBeDisabled();
  await page.getByLabel("Required status checks").fill("ci/test\nsecurity");
  await queue.click();
  await allowDeletions.click();
  await expect(queue).toBeChecked();
  await page.getByRole("button", { name: "Save changes" }).click();
  const rule = page.locator(".protection-list > li");
  await expect(rule).toContainText("main");
  await expect(rule).toContainText("2 approving reviews required");
  await expect(rule).toContainText("Merge queue required");
  await expect(rule).toContainText("Deletions allowed");
  await expect(rule.locator(".protection-checks")).toContainText("ci/test");
  await expect(rule.locator(".protection-checks")).toContainText("security");
  expect(updates[0]).toEqual({
    expected_version: 0,
    rules: [
      {
        branch: "main",
        required_approvals: 2,
        require_merge_queue: true,
        allow_deletions: true,
        required_checks: ["ci/test", "security"],
      },
    ],
  });

  await page.getByRole("button", { name: "Edit main" }).click();
  await expect(queue).toBeChecked();
  await expect(allowDeletions).toBeChecked();
  await allowDeletions.click();
  await chooseSelectOption(page.getByLabel("Required approving reviews"), "1");
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
        require_merge_queue: true,
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

test("repository settings manage members with a revisioned CSRF-protected replacement", async ({
  page,
}) => {
  let revision = 42;
  let conflictNext = false;
  let members = [
    { subject: "alice-id", name: "Alice", access: "admin" },
    { subject: "bob-id", name: "Bob", access: "read" },
  ];
  const updates: unknown[] = [];

  await page.route("**/api/session", (route) =>
    route.fulfill({
      json: {
        authenticated: true,
        mode: "oidc",
        user: {
          issuer: "https://issuer.example",
          subject: "alice-id",
          name: "Alice",
        },
        csrf: "membership-csrf",
      },
    }),
  );
  await page.route(
    (url) => url.pathname === "/api/repos/team/project/members",
    (route) => {
      if (route.request().method() === "GET")
        return route.fulfill({
          json: {
            revision,
            members,
            groups: [],
            can_admin: members.some(
              (m) => m.subject === "alice-id" && m.access === "admin",
            ),
            groups_supported: true,
          },
        });

      expect(route.request().method()).toBe("PUT");
      expect(route.request().headers()["x-csrf-token"]).toBe("membership-csrf");
      const update = route.request().postDataJSON() as {
        expected_revision: number;
        members: typeof members;
      };
      updates.push(update);
      if (conflictNext) {
        conflictNext = false;
        revision += 1;
        members = [
          { subject: "alice-id", name: "Alice", access: "admin" },
          { subject: "charlie-id", name: "Remote change", access: "write" },
        ];
        return route.fulfill({
          status: 409,
          json: {
            error: {
              code: "membership_changed",
              message: "Membership changed. Reload members and try again.",
            },
          },
        });
      }
      if (!update.members.some((member) => member.access === "admin"))
        return route.fulfill({
          status: 422,
          json: {
            error: {
              code: "administrator_required",
              message: "At least one administrator is required.",
            },
          },
        });
      expect(update.expected_revision).toBe(revision);
      revision += 1;
      members = update.members;
      return route.fulfill({
        json: {
          revision,
          members,
          groups: [],
          can_admin: members.some(
            (m) => m.subject === "alice-id" && m.access === "admin",
          ),
          groups_supported: true,
        },
      });
    },
  );

  await page.goto("/team/project/settings/access");
  await expect(
    page.getByRole("link", { name: "Members", exact: true }),
  ).toHaveAttribute("aria-current", "page");
  await expect(page.getByRole("heading", { name: "Members" })).toBeVisible();
  await expect(page.getByText("alice-id", { exact: true })).toBeVisible();

  await page.getByRole("button", { name: "Add member", exact: true }).click();
  let form = page.locator(".member-form");
  await expect(form.locator("select")).toHaveCount(0);
  await form.getByLabel("Subject").fill("charlie-id");
  await form.getByLabel("Display name").fill("Charlie");
  await form.getByRole("button", { name: "Access" }).click();
  await page.getByRole("menuitemradio", { name: /^Read/ }).click();
  await form.getByRole("button", { name: "Add member", exact: true }).click();

  await page.getByRole("button", { name: "Edit Charlie" }).click();
  form = page.locator(".member-form");
  await form.getByLabel("Display name").fill("Charles");
  await form.getByRole("button", { name: "Access" }).click();
  await page.getByRole("menuitemradio", { name: /^Triage/ }).click();
  await form.getByRole("button", { name: "Save member" }).click();

  await page.getByRole("button", { name: "Remove Bob" }).click();
  const removal = page.getByRole("region", { name: "Remove member" });
  await expect(removal).toContainText("Remove this member?");
  await removal.getByRole("button", { name: "Remove member" }).click();
  await page.getByRole("button", { name: "Save access" }).click();
  expect(updates[0]).toEqual({
    expected_revision: 42,
    groups: [],
    members: [
      { subject: "alice-id", name: "Alice", access: "admin" },
      { subject: "charlie-id", name: "Charles", access: "triage" },
    ],
  });
  await expect(page.getByRole("status")).toContainText("Access updated.");
  await page.reload();
  await expect(
    page.locator('.member-role[data-access="triage"]'),
  ).toContainText("Triage");
  await page.getByRole("button", { name: "Edit Charles" }).click();
  form = page.locator(".member-form");
  await form.getByRole("button", { name: "Access" }).click();
  await page.getByRole("menuitemradio", { name: /^Write/ }).click();
  await form.getByRole("button", { name: "Save member" }).click();
  await page.getByRole("button", { name: "Save access" }).click();
  await expect(page.locator('.member-role[data-access="write"]')).toContainText(
    "Write",
  );

  await page.getByRole("button", { name: "Edit Charles" }).click();
  form = page.locator(".member-form");
  await form.getByLabel("Display name").fill("Charles draft");
  await form.getByRole("button", { name: "Save member" }).click();
  conflictNext = true;
  await page.getByRole("button", { name: "Save access" }).click();
  await expect(page.getByRole("alert")).toContainText(
    "Another administrator changed membership.",
  );
  await expect(page.getByText("Charles draft", { exact: true })).toBeVisible();

  await page.getByRole("button", { name: "Reload members" }).click();
  await expect(page.getByText("Remote change", { exact: true })).toBeVisible();
  await expect(page.getByText("Charles draft", { exact: true })).toHaveCount(0);

  await page.getByRole("button", { name: "Remove Alice" }).click();
  await page
    .getByRole("region", { name: "Remove member" })
    .getByRole("button", { name: "Remove member" })
    .click();
  await page.getByRole("button", { name: "Save access" }).click();
  await expect(page.getByRole("alert")).toContainText(
    "At least one administrator is required.",
  );

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
  await expect(page).toHaveURL(/\/tree\/feature%2Fbrowser$/);
  await expect(anchor).toHaveText("feature/browser");
  await anchor.click();
  await expect(
    dialog.getByRole("menuitemradio", {
      name: "feature/browser",
      exact: true,
    }),
  ).toHaveAttribute("aria-checked", "true");
});
