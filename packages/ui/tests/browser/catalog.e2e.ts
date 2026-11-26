import { expect, test, type Page } from "@playwright/test";

const owner = "user-7456000bc221ab0a67cd7d50464b5393";

async function session(page: Page) {
  await page.route("**/api/session", (route) =>
    route.fulfill({
      json: {
        authenticated: true,
        csrf: "fixture-csrf",
        user: {
          issuer: "https://accounts.example",
          name: "Alice",
          subject: "alice",
        },
      },
    }),
  );
}

test("the owner picker scopes creation and import to the selected organization", async ({
  page,
}) => {
  await session(page);
  const id = "01900000-0000-7000-8000-000000000010";
  await page.route("**/api/organizations", (route) =>
    route.fulfill({
      json: {
        next: null,
        items: [
          {
            kind: "member",
            actor: "actor-id",
            role: "owner",
            can_create: true,
            organization: { id, name: "Crab engineering", description: "" },
          },
        ],
      },
    }),
  );
  await page.route(
    `**/api/organizations/${id}/repositories/creation`,
    (route) =>
      route.fulfill({
        json: {
          owner: `org-${id.replaceAll("-", "")}`,
          default_branch: "main",
          visibilities: ["public"],
        },
      }),
  );
  await page.route("**/api/repos?*", (route) =>
    route.fulfill({ json: { repositories: [], next: null } }),
  );
  await page.goto("/-/repositories");
  await page.getByRole("button", { name: "Owner · Personal" }).click();
  await page.getByRole("button", { name: "Crab engineering" }).click();
  await page.getByRole("button", { name: "New repository" }).click();
  await expect(page.getByRole("dialog")).toContainText(
    "owned by Crab engineering",
  );
  await expect(page.getByRole("radio", { name: "Public" })).toBeChecked();
  await expect(page.getByRole("radio", { name: "Private" })).toHaveCount(0);
  await page.keyboard.press("Escape");
  await page.getByRole("button", { name: "Import from Git" }).click();
  await expect(page.getByLabel("Organization", { exact: true })).toHaveValue(
    "Crab engineering",
  );
});

test("personal import retries namespace resolution before submitting", async ({
  page,
}) => {
  await session(page);
  let namespaceAvailable = false;
  let submitted = false;
  await page.route("**/api/repos/personal?*", (route) =>
    route.fulfill({ json: { repositories: [], next: null } }),
  );
  await page.route("**/api/repos/creation", (route) =>
    namespaceAvailable
      ? route.fulfill({
          json: {
            owner,
            default_branch: "main",
            visibilities: ["private", "public"],
          },
        })
      : route.fulfill({
          status: 503,
          json: { error: { message: "Namespace lookup is unavailable" } },
        }),
  );
  await page.route("**/api/imports/git", (route) => {
    const input = route.request().postDataJSON();
    expect(input).toMatchObject({
      source: "https://github.com/example/notes.git",
      owner,
      name: "notes",
      visibility: "private",
    });
    submitted = true;
    return route.fulfill({
      status: 202,
      json: {
        ...input,
        id: input.request_id,
        state: "queued",
        message: "Queued for import",
        resumable: false,
      },
    });
  });

  await page.goto("/-/repositories/personal");
  await page.getByRole("button", { name: "Import from Git" }).click();
  await page
    .getByLabel("Git source")
    .fill("https://github.com/example/notes.git");
  const start = page.getByRole("button", { name: "Start import" });
  await expect(start).toBeDisabled();
  await expect(page.getByText("Namespace lookup is unavailable")).toBeVisible();
  namespaceAvailable = true;
  await page.getByRole("button", { name: "Retry namespace lookup" }).click();
  const namespace = page.getByLabel("Personal namespace");
  await expect(namespace).toHaveValue(owner);
  await expect(namespace).toBeDisabled();
  await start.click();
  await expect.poll(() => submitted).toBe(true);
});

test("new repository recovers one submission after a reload", async ({
  page,
}) => {
  await session(page);
  let ready = false;
  const repository = {
    id: "6a1b1f4f-2681-458b-b872-6b329c6d79d9",
    owner,
    name: "my-library",
    description: "First repository",
    access: "write",
    can_admin: true,
    archive_version: 0,
    archived: false,
    protection_version: 0,
    protected_branches: [],
  };
  let submission: unknown;
  await page.route(
    (url) => url.pathname === "/api/repos",
    (route) =>
      route.fulfill({ json: { repositories: ready ? [repository] : [] } }),
  );
  await page.route("**/api/repos/creation", (route) => {
    const request = route.request();
    if (request.method() === "GET") {
      return route.fulfill({
        json: {
          owner,
          default_branch: "main",
          visibilities: ["private", "public"],
        },
      });
    }
    expect(request.headers()["x-csrf-token"]).toBe("fixture-csrf");
    const body = request.postDataJSON();
    if (!ready) {
      expect(body).toMatchObject({
        name: "my-library",
        description: "First repository",
        visibility: "private",
      });
      submission = body;
      ready = true;
      return route.abort("failed");
    }
    expect(body).toEqual(submission);
    return route.fulfill({ status: 201, json: repository });
  });
  await page.route(
    (url) => url.pathname === `/api/repos/${owner}/my-library/refs`,
    (route) =>
      route.fulfill({ json: { head: null, default_branch: "main", refs: [] } }),
  );
  await page.goto("/-/repositories");
  await page
    .getByRole("button", { name: "New repository", exact: true })
    .click();
  await page.getByLabel("Repository name").fill("my-library");
  await page.getByLabel("Description (optional)").fill("First repository");
  await page
    .getByRole("button", { name: "Create repository", exact: true })
    .click();
  await expect(
    page.getByText("The response was lost.", { exact: false }),
  ).toBeVisible();
  await page.reload();
  await page
    .getByRole("button", { name: "New repository", exact: true })
    .click();
  await expect(page.getByLabel("Repository name")).toHaveValue("my-library");
  await expect(page.getByLabel("Description (optional)")).toHaveValue(
    "First repository",
  );
  await page
    .getByRole("button", { name: "Create repository", exact: true })
    .click();
  await expect(page).toHaveURL(`/${owner}/my-library`);
  await expect(page.locator(".global-repository h1")).toContainText(
    "my-library",
  );
});

test("a reloaded public import keeps its visibility and takes a fresh source token", async ({
  page,
}) => {
  await session(page);
  let saved: {
    id: string;
    source: string;
    owner: string;
    name: string;
    visibility: "public";
  };
  let state = "running";
  await page.route(
    (url) => url.pathname === "/api/repos",
    (route) =>
      route.fulfill({
        json: {
          repositories:
            state === "succeeded"
              ? [
                  {
                    id: "6a1b1f4f-2681-458b-b872-6b329c6d79d9",
                    owner: saved.owner,
                    name: saved.name,
                    description: "Imported repo",
                    access: "write",
                    can_admin: true,
                    archive_version: 0,
                    archived: false,
                    protection_version: 0,
                    protected_branches: [],
                  },
                ]
              : [],
        },
      }),
  );
  await page.route("**/api/repos/creation", (route) =>
    route.fulfill({
      json: {
        owner,
        default_branch: "main",
        visibilities: ["private", "public"],
      },
    }),
  );
  await page.route(`**/api/repos/${owner}/repo/refs`, (route) =>
    route.fulfill({ json: { head: null, default_branch: "main", refs: [] } }),
  );
  await page.route("**/api/imports/git", (route) => {
    const input = route.request().postDataJSON();
    expect(input.token).toBe("fixture-token");
    expect(input.owner).toBe(owner);
    expect(input.visibility).toBe("public");
    saved = {
      id: input.request_id,
      source: input.source,
      owner: input.owner,
      name: input.name,
      visibility: input.visibility,
    };
    return route.fulfill({
      status: 202,
      json: {
        ...saved,
        state: "queued",
        message: "Queued for import",
        resumable: false,
      },
    });
  });
  await page.route("**/api/imports/git/*", (route) => {
    expect(route.request().url()).toContain(saved.id);
    return route.fulfill({
      json: {
        ...saved,
        state,
        resumable: state === "interrupted",
        repository:
          state === "succeeded"
            ? { owner: saved.owner, name: saved.name }
            : undefined,
        message:
          state === "interrupted"
            ? "The import worker stopped reporting progress."
            : "Copying Git history",
      },
    });
  });
  await page.route("**/api/imports/git/*/resume", (route) => {
    expect(route.request().url()).toContain(`/${saved.id}/resume`);
    expect(route.request().headers()["x-csrf-token"]).toBe("fixture-csrf");
    expect(route.request().postDataJSON()).toEqual({
      token: "new-source-token",
    });
    state = "succeeded";
    return route.fulfill({
      status: 202,
      json: {
        ...saved,
        state: "queued",
        message: "Queued to resume import",
        resumable: false,
      },
    });
  });
  await page.goto("/-/repositories");
  await page
    .getByRole("button", { name: "Import from Git", exact: true })
    .click();
  await page
    .getByLabel("Git source")
    .fill("https://github.com/example/repo.git");
  await expect(page.getByLabel("Personal namespace")).toHaveValue(owner);
  await page.getByLabel("Source token").fill("fixture-token");
  const visibility = page
    .getByRole("region", { name: "Bring a repository from Git" })
    .getByRole("combobox", { name: "Visibility" });
  await expect(visibility).toHaveText("Private");
  await visibility.click();
  await page.getByRole("option", { name: "Public", exact: true }).click();
  await page.getByRole("button", { name: "Start import", exact: true }).click();
  await expect(page.getByText("Queued for import")).toBeVisible();
  state = "interrupted";
  await page.reload();
  await expect(
    page.getByText("The import worker stopped reporting progress."),
  ).toBeVisible();
  await expect(page.getByText(`${owner}/repo`, { exact: true })).toBeVisible();
  await expect(page.getByLabel("Source token")).toHaveValue("");
  const savedImport = await page.evaluate(() =>
    sessionStorage.getItem(
      'crab:git-import:personal:["https://accounts.example","alice"]',
    ),
  );
  expect(savedImport).toBe(saved!.id);
  await expect(page.getByLabel("Git source")).toBeDisabled();
  await expect(visibility).toBeDisabled();
  await expect(visibility).toHaveText("Public");
  await expect(page.getByLabel("Repository name")).toHaveValue("repo");
  await page.getByLabel("Source token").fill("new-source-token");
  await page
    .getByRole("button", { name: "Resume import", exact: true })
    .click();
  await expect(page).toHaveURL(`/${owner}/repo`);
  await expect(page.locator(".global-repository h1")).toContainText("repo");
});
