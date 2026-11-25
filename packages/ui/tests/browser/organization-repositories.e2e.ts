import { expect, test, type Page } from "@playwright/test";
import { expectNoAccessibilityViolations } from "./accessibility";

test("organization repository creation recovers its owner-scoped request after reload", async ({
  page,
}) => {
  const organization = "01900000-0000-7000-8000-000000000010";
  const alice = "01900000-0000-7000-8000-000000000011";
  const bob = "01900000-0000-7000-8000-000000000012";
  const property = "01900000-0000-7000-8000-000000000013";
  const owner = `org-${organization.replaceAll("-", "")}`;
  const repository = {
    id: "01900000-0000-7000-8000-000000000014",
    owner,
    organization_id: organization,
    name: "widgets",
    description: "Team repository",
    access: "write",
    can_admin: true,
    archived: false,
    archive_version: 0,
    protection_version: 0,
    protected_branches: [],
  };
  const orgPage = `/-/organizations/${organization}`;
  const orgApi = `/api/organizations/${organization}`;
  let actor = alice;
  let role = "owner";
  let created = false;
  let directRead = false;
  let directAdmin = false;
  let organizationVisible = true;
  let intent: unknown;
  await page.route("**/api/**", (route) => {
    const request = route.request();
    const { pathname, searchParams } = new URL(request.url());
    if (pathname === "/api/session")
      return route.fulfill({
        json: {
          authenticated: true,
          csrf: "fixture-csrf",
          mode: "oidc",
          user: {
            issuer: "https://id.example",
            subject: actor,
            name: actor === alice ? "Alice" : "Bob",
          },
        },
      });
    if (pathname === orgApi) {
      if (!organizationVisible) return route.fulfill({ status: 404 });
      return route.fulfill({
        json: {
          organization: {
            id: organization,
            name: "Crab Engineering",
            description: "",
            version: 1,
          },
          actor,
          role,
        },
      });
    }
    if (pathname === `${orgApi}/members`)
      return route.fulfill({
        json: { items: [], next: null, revision: 1, can_manage: true },
      });
    if (pathname === `${orgApi}/repositories/creation`) {
      expect(role).toBe("owner");
      if (request.method() === "GET")
        return route.fulfill({
          json: {
            owner,
            default_branch: "trunk",
            visibilities: ["private", "public"],
            properties: [
              {
                id: property,
                name: "service_area",
                description: "The team responsible for this repository.",
                kind: "text",
                options: [],
                default: "Shared",
                required: true,
                require_explicit_values: true,
                values_editable_by: "organization",
              },
            ],
          },
        });
      expect(actor).toBe(alice);
      expect(request.headers()["x-csrf-token"]).toBe("fixture-csrf");
      if (!created) {
        intent = request.postDataJSON();
        expect(intent).toMatchObject({
          name: repository.name,
          description: repository.description,
          visibility: "public",
          properties: { [property]: "Tools" },
        });
        created = true;
        return route.abort("failed");
      }
      expect(request.postDataJSON()).toEqual(intent);
      return route.fulfill({ status: 201, json: repository });
    }
    if (pathname === "/api/repos")
      return route.fulfill({
        json: {
          repositories:
            created &&
            (searchParams.get("organization") === organization ||
              searchParams.get("owner") === owner) &&
            (role === "owner" || directRead)
              ? [{ ...repository, can_admin: role === "owner" || directAdmin }]
              : [],
        },
      });
    if (pathname === `/api/repos/${owner}/widgets/refs`)
      return route.fulfill({
        json: { head: null, unborn_head: "refs/heads/trunk", refs: [] },
      });
    return route.fulfill({
      status: 404,
      json: { error: { message: "Fixture route unavailable" } },
    });
  });

  await page.goto(orgPage);
  await page
    .getByRole("navigation", { name: "Organization", exact: true })
    .getByRole("link", { name: "Repositories", exact: true })
    .click();
  await expect(
    page.getByRole("heading", { name: "Organization repositories" }),
  ).toBeVisible();
  await expectNoAccessibilityViolations(page);
  await page.getByRole("button", { name: "New repository" }).click();
  await expect(page.getByRole("dialog")).toContainText(
    "owned by Crab Engineering",
  );
  await expect(page.getByRole("dialog")).toContainText("Default branch trunk");
  await expect
    .poll(() =>
      page.getByRole("dialog").evaluate((dialog) => {
        const bounds = dialog.getBoundingClientRect();
        return (
          dialog.scrollWidth <= dialog.clientWidth &&
          bounds.top >= 0 &&
          bounds.bottom <= window.innerHeight
        );
      }),
    )
    .toBe(true);
  await page.getByLabel("Repository name").fill("widgets");
  await page.getByLabel("Description (optional)").fill("Team repository");
  await page.getByRole("radio", { name: "Public" }).click();
  await expect(page.getByLabel("service_area")).toHaveAttribute(
    "placeholder",
    "Choose a value",
  );
  await expect(
    page.getByRole("button", { name: "Create repository", exact: true }),
  ).toBeDisabled();
  await page.getByLabel("service_area").fill("Tools");
  await page
    .getByRole("button", { name: "Create repository", exact: true })
    .click();
  await expect(
    page.getByText("The response was lost.", { exact: false }),
  ).toBeVisible();

  actor = bob;
  await page.reload();
  await page.getByRole("button", { name: "New repository" }).click();
  await expect(page.getByLabel("Repository name")).toHaveValue("");
  actor = alice;
  await page.reload();
  await page.getByRole("button", { name: "New repository" }).click();
  await expect(page.getByLabel("Repository name")).toHaveValue("widgets");
  await expect(page.getByLabel("Description (optional)")).toHaveValue(
    "Team repository",
  );
  await expect(page.getByRole("radio", { name: "Public" })).toBeChecked();
  await expect(page.getByLabel("service_area")).toHaveValue("Tools");
  await page
    .getByRole("button", { name: "Create repository", exact: true })
    .click();
  await expect(page).toHaveURL(new RegExp(`/${owner}/widgets$`));
  const banner = page.getByRole("banner");
  await expect(page).toHaveTitle("Crab Engineering/widgets · Crab");
  await banner
    .getByRole("link", { name: "Crab Engineering", exact: true })
    .click();
  await expect(page).toHaveURL(
    new RegExp(`/crab-engineering--${organization}\\?tab=repositories$`),
  );

  actor = bob;
  role = "member";
  await page.goto(`${orgPage}?tab=repositories`);
  await expect(
    page.getByText("No repositories are visible to you in this organization."),
  ).toBeVisible();
  await expect(
    page.getByRole("button", { name: "New repository" }),
  ).toHaveCount(0);
  directRead = true;
  await page.reload();
  await expect(page.getByRole("link", { name: "widgets" })).toHaveAttribute(
    "href",
    `/${owner}/widgets`,
  );
  const settings = page.getByRole("link", { name: "Settings for widgets" });
  await expect(settings).toHaveCount(0);
  directAdmin = true;
  await page.reload();
  await settings.click();
  await expect(page).toHaveURL(new RegExp(`/${owner}/widgets/settings$`));
  organizationVisible = false;
  await page.reload();
  await expect(banner.getByRole("heading", { name: /widgets/ })).toBeVisible();
  await expect(
    banner.getByRole("link", { name: "Crab Engineering", exact: true }),
  ).toHaveCount(0);
});

test("an eligible member imports with the allowed visibility and an owner can resume", async ({
  page,
}) => {
  const organization = "01900000-0000-7000-8000-000000000020";
  const alice = "01900000-0000-7000-8000-000000000021";
  const bob = "01900000-0000-7000-8000-000000000022";
  const property = "01900000-0000-7000-8000-000000000023";
  const properties = { [property]: "Tools" };
  const owner = `org-${organization.replaceAll("-", "")}`;
  const orgPage = `/-/organizations/${organization}?tab=repositories`;
  const orgApi = `/api/organizations/${organization}`;
  const imports = `${orgApi}/repositories/imports/git`;
  const source = "https://github.com/example/widgets.git";
  const repository = {
    id: "01900000-0000-7000-8000-000000000024",
    organization,
    owner,
    name: "widgets",
    description: "",
    access: "write",
    can_admin: true,
    archived: false,
    archive_version: 0,
    protection_version: 0,
    protected_branches: [],
  };
  let jobId: string | undefined;
  let complete = false;
  let actor = alice;
  let role = "member";
  await page.route("**/api/**", (route) => {
    const req = route.request();
    const { pathname } = new URL(req.url());
    if (pathname === "/api/session")
      return route.fulfill({
        json: {
          authenticated: true,
          csrf: "fixture-csrf",
          mode: "oidc",
          user: {
            issuer: "https://id.example",
            subject: actor,
            name: actor === alice ? "Alice" : "Bob",
          },
        },
      });
    if (pathname === orgApi)
      return route.fulfill({
        json: {
          organization: {
            id: organization,
            name: "Crab Engineering",
            description: "",
            version: 1,
          },
          actor,
          role,
          can_create: actor === alice || role === "owner",
        },
      });
    if (pathname === `${orgApi}/repositories/creation`)
      return route.fulfill({
        json: {
          visibilities: role === "owner" ? ["private", "public"] : ["public"],
          properties: [
            {
              id: property,
              name: "service_area",
              description: "Responsible group",
              kind: "text",
              options: [],
              default: "Shared",
              required: true,
              require_explicit_values: true,
              values_editable_by: "organization",
            },
          ],
        },
      });
    if (pathname === "/api/repos") {
      return route.fulfill({
        json: { repositories: complete ? [repository] : [] },
      });
    }
    if (pathname === `/api/repos/${owner}/widgets/refs`) {
      return route.fulfill({
        json: { head: null, default_branch: "trunk", refs: [] },
      });
    }
    const job = {
      id: jobId,
      source,
      owner,
      name: repository.name,
      visibility: "public",
      properties,
    };
    const interrupted = {
      ...job,
      state: "interrupted",
      resumable: true,
      message: "The import worker stopped reporting progress.",
    };
    if (pathname === imports && req.method() === "POST") {
      expect(actor).toBe(alice);
      expect(req.headers()["x-csrf-token"]).toBe("fixture-csrf");
      const input = req.postDataJSON();
      expect(input).toEqual({
        request_id: expect.any(String),
        source,
        name: "widgets",
        visibility: "public",
        properties,
        token: "initial-token",
      });
      jobId = input.request_id;
      return route.fulfill({
        status: 202,
        json: { ...interrupted, id: jobId },
      });
    }
    if (pathname === `${imports}/${jobId}`)
      return actor === alice || role === "owner"
        ? route.fulfill({ json: interrupted })
        : route.fulfill({
            status: 404,
            json: { error: { message: "Import job not found" } },
          });
    if (pathname === `${imports}/${jobId}/resume`) {
      expect(actor).toBe(bob);
      expect(req.headers()["x-csrf-token"]).toBe("fixture-csrf");
      expect(req.postDataJSON()).toEqual({ token: "replacement-token" });
      complete = true;
      return route.fulfill({
        status: 202,
        json: {
          ...job,
          state: "succeeded",
          resumable: false,
          message: "Repository imported",
          repository,
        },
      });
    }
    return route.fulfill({
      status: 404,
      json: { error: { message: "Fixture route unavailable" } },
    });
  });

  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto(orgPage);
  await page.getByRole("button", { name: "Import from Git" }).click();
  const target = page.getByRole("textbox", {
    name: "Organization",
    exact: true,
  });
  await expect(target).toBeDisabled();
  await expect(target).toHaveValue("Crab Engineering");
  await expectNoAccessibilityViolations(page);
  expect(
    await page.evaluate(
      () =>
        document.documentElement.scrollWidth <=
        document.documentElement.clientWidth,
    ),
  ).toBe(true);
  await page.getByLabel("Git source").fill(source);
  await page.getByLabel("Source token").fill("initial-token");
  await expect(
    page.getByRole("button", { name: "Start import" }),
  ).toBeDisabled();
  await page.getByLabel("service_area").fill("Tools");
  const visibility = page
    .getByRole("region", { name: "Bring a repository from Git" })
    .getByRole("combobox", { name: "Visibility" });
  await expect(visibility).toHaveText("Public");
  await visibility.click();
  await expect(
    page.getByRole("option", { name: "Private", exact: true }),
  ).toHaveCount(0);
  await page.keyboard.press("Escape");
  await page.getByRole("button", { name: "Start import" }).click();
  const statusLink = page.getByRole("link", { name: "Import status link" });
  await expect(statusLink).toBeVisible();
  const sharedLink = await statusLink.getAttribute("href");
  expect(sharedLink).toBe(`${orgPage}&import=${jobId}`);

  actor = bob;
  role = "member";
  await page.goto(sharedLink!);
  await expect(
    page.getByRole("heading", { name: "Organization repositories" }),
  ).toBeVisible();
  await expect(
    page.getByRole("button", { name: "Import from Git" }),
  ).toHaveCount(0);
  await expect(page.getByRole("button", { name: "Resume import" })).toHaveCount(
    0,
  );
  await expect(page.getByRole("alert")).toContainText("Import job not found");

  role = "owner";
  await page.reload();
  await expect(
    page.getByText("The import worker stopped reporting progress."),
  ).toBeVisible();
  await expect(page.getByLabel("Source token")).toHaveValue("");
  await expect(visibility).toBeDisabled();
  await expect(visibility).toHaveText("Public");
  await expect(page.locator("dl")).toContainText("Tools");
  await expect(page.getByLabel("service_area")).toHaveCount(0);
  await page.getByLabel("Source token").fill("replacement-token");
  await page.getByRole("button", { name: "Resume import" }).click();
  await expect(page).toHaveURL(`/${owner}/widgets`);
  await expect(page.locator(".global-repository h1")).toContainText("widgets");
});

test("a personal fork keeps its job scope across reload and resume", async ({
  page,
}) => {
  const organization = "01900000-0000-7000-8000-000000000030";
  const id = "01900000-0000-7000-8000-000000000031";
  const owner = `org-${organization.replaceAll("-", "")}`;
  const source = {
    id,
    owner,
    name: "library",
    organization_id: organization,
    visibility: "private",
    can_fork: true,
    can_fork_to_organization: true,
    can_admin: true,
  };
  const fork = {
    id: "01900000-0000-7000-8000-000000000032",
    owner: "user-personal",
    name: "library",
    visibility: "private",
    fork: true,
    forked_from: { id, owner, name: "library" },
    protected_branches: [],
    access: "write",
  };
  const main = { name: "refs/heads/main", oid: "a".repeat(40) };
  const feature = { name: "refs/heads/feature", oid: "b".repeat(40) };
  const forkRefs = [main];
  let upstreamAccess = true;
  let copyRequestId: string | undefined;
  let jobId: string | undefined;
  let reads = 0;
  const job = () => ({
    id: jobId,
    owner: fork.owner,
    name: fork.name,
    source: `${owner}/library`,
    fork_source: id,
    default_branch_only: true,
    state: "interrupted",
    message: "Fork interrupted",
    resumable: true,
  });
  await page.route("**/api/**", (route) => {
    const req = route.request();
    const { pathname, searchParams } = new URL(req.url());
    if (pathname === "/api/session")
      return route.fulfill({
        json: {
          authenticated: true,
          csrf: "csrf",
          mode: "oidc",
          user: { issuer: "fixture", subject: "alice", name: "Alice" },
        },
      });
    if (pathname === `/api/organizations/${organization}`)
      return route.fulfill({
        json: {
          organization: {
            id: organization,
            name: "Crab Engineering",
            version: 1,
          },
          role: "owner",
          actor: id,
        },
      });
    if (pathname === "/api/repos")
      return route.fulfill({
        json: {
          repositories: [
            searchParams.get("owner") === fork.owner
              ? {
                  ...fork,
                  forked_from: upstreamAccess ? fork.forked_from : undefined,
                }
              : source,
          ],
        },
      });
    if (pathname.endsWith("/refs"))
      return route.fulfill({
        json: {
          head: main,
          refs: pathname.startsWith(`/api/repos/${owner}/`)
            ? [main, feature]
            : forkRefs,
        },
      });
    if (pathname === `/api/repos/${fork.owner}/library/fork-sync`) {
      expect(req.headers()["x-crab-repository-id"]).toBe(fork.id);
      const input = req.postDataJSON();
      expect(input).toEqual({
        branch: feature.name,
        fork_oid: null,
        upstream_oid: feature.oid,
        request_id: copyRequestId ?? expect.any(String),
      });
      if (!copyRequestId) {
        copyRequestId = input.request_id;
        return route.fulfill({
          status: 503,
          json: { error: { message: "Connection interrupted" } },
        });
      }
      forkRefs.push(feature);
      return route.fulfill({
        json: { branch: feature.name, commit: feature.oid },
      });
    }
    if (pathname === `/api/repos/${owner}/library/forks`) {
      expect(req.headers()["x-crab-repository-id"]).toBe(id);
      if (req.method() === "GET") {
        const query = searchParams.get("q");
        return route.fulfill({
          json: {
            repositories: query === "missing" ? [] : [{ ...fork, fork: true }],
            next: null,
          },
        });
      }
      const input = req.postDataJSON();
      expect(input).toEqual({
        request_id: expect.any(String),
        name: "library",
        destination: "personal",
        description: "Work on release tooling",
        default_branch_only: true,
      });
      jobId = input.request_id;
      return route.fulfill({ status: 202, json: job() });
    }
    if (pathname === `/api/imports/git/${jobId}`) {
      reads += 1;
      return route.fulfill({ json: job() });
    }
    if (pathname === `/api/imports/git/${jobId}/resume`) {
      expect(req.postDataJSON()).toEqual({});
      return route.fulfill({
        status: 202,
        json: {
          ...job(),
          state: "succeeded",
          resumable: false,
          repository: fork,
        },
      });
    }
    return route.fulfill({
      status: 404,
      json: { error: { message: "Fixture route unavailable" } },
    });
  });

  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto(`/${owner}/library`);
  await page
    .getByRole("button", { name: "Fork repository", exact: true })
    .click();
  const destination = page.getByRole("combobox", { name: "Fork owner" });
  await destination.click();
  await page.getByRole("option", { name: "Personal", exact: true }).click();
  await expect(page.getByLabel("Repository name")).toHaveValue("library");
  const defaultBranch = page.getByRole("checkbox", {
    name: "Copy only the default branch",
  });
  await expect(defaultBranch).toBeChecked();
  await defaultBranch.uncheck();
  await expect(defaultBranch).not.toBeChecked();
  await defaultBranch.check();
  await page.getByLabel("Description").fill("Work on release tooling");
  await expectNoAccessibilityViolations(page);
  await page.getByRole("button", { name: "Create fork", exact: true }).click();
  await expect(
    page.getByText("Fork interrupted", { exact: true }),
  ).toBeVisible();
  await expect(
    page.getByRole("link", { name: "Fork status link" }),
  ).toHaveCount(0);
  await page.reload();
  await expect(destination).toHaveText("Personal");
  await expect(defaultBranch).toBeChecked();
  await expect(defaultBranch).toBeDisabled();
  await page.getByRole("button", { name: "Resume fork", exact: true }).click();
  await expect(page).toHaveURL(/\/user-personal\/library$/);
  expect(reads).toBeGreaterThan(0);
  await page.getByRole("link", { name: "View forks" }).click();
  await expect(
    page.getByRole("heading", { name: "Forks", exact: true }),
  ).toBeVisible();
  const results = page.getByRole("list", { name: "Repository results" });
  const forkResult = results.getByRole("link", {
    name: "user-personal / library",
  });
  await expect(forkResult).toBeVisible();
  const directorySearch = page.getByRole("searchbox", {
    name: "Search repositories",
  });
  await directorySearch.fill("missing");
  await directorySearch.press("Enter");
  await expect(
    page.getByText("No repositories match this search."),
  ).toBeVisible();
  await page.getByRole("link", { name: "Clear search" }).click();
  await expect(forkResult).toBeVisible();
  await expectNoAccessibilityViolations(page);
  await forkResult.click();
  await expect(page).toHaveURL(/\/user-personal\/library$/);
  await page.goto(`/${fork.owner}/library/branches`);
  await page.getByRole("button", { name: "Copy from upstream" }).click();
  const copyDialog = page.getByRole("dialog", {
    name: "Copy an upstream branch",
  });
  await expect(
    copyDialog.getByRole("button", { name: "Copy main" }),
  ).toHaveCount(0);
  const search = copyDialog.getByRole("textbox", {
    name: "Search upstream branches",
  });
  await search.fill("missing");
  await expect(
    copyDialog.getByText("No upstream branches match your search."),
  ).toBeVisible();
  await search.fill("feat");
  const copy = copyDialog.getByRole("button", { name: "Copy feature" });
  await expect(copy).toBeVisible();
  await expectNoAccessibilityViolations(page);
  await copy.click();
  await expect(copyDialog.getByRole("alert")).toHaveText(
    "Connection interrupted",
  );
  await copy.click();
  await expect(
    page.getByRole("link", { name: "feature", exact: true }),
  ).toBeVisible();
  upstreamAccess = false;
  await page.goto(`/${fork.owner}/library`);
  await expect(page.getByText("Private fork", { exact: true })).toBeVisible();
  await expect(page.getByRole("link", { name: "View forks" })).toHaveCount(0);
  await expect(page.getByText("Forked from")).toHaveCount(0);
  await page.goto(`/${fork.owner}/library/branches`);
  await expect(
    page.getByRole("link", { name: "feature", exact: true }),
  ).toBeVisible();
  await expect(
    page.getByRole("button", { name: "Copy from upstream" }),
  ).toHaveCount(0);
});

async function chooseOrganizationFork(
  page: Page,
  visibility: "private" | "public",
) {
  const sourceId = "01900000-0000-7000-8000-000000000040";
  const targetId = "01900000-0000-7000-8000-000000000041";
  const sourceOrg = { id: sourceId, name: "Source engineering" };
  const target = { id: targetId, name: "Release engineering" };
  const owner = `org-${sourceId.replaceAll("-", "")}`;
  const source = {
    id: "01900000-0000-7000-8000-000000000042",
    owner,
    name: "library",
    organization_id: sourceId,
    access: "read",
    visibility,
    public_read_only: visibility === "public",
    can_fork: true,
    can_fork_to_other_organizations: true,
  };
  const fork = {
    ...source,
    id: "01900000-0000-7000-8000-000000000043",
    owner: `org-${targetId.replaceAll("-", "")}`,
    organization_id: targetId,
    can_fork: false,
    fork: true,
    forked_from: source,
    access: "write",
    public_read_only: false,
  };
  const jobRoot = `/api/organizations/${targetId}/repositories/imports/git`;
  let requestId: string | undefined;
  const job = () => ({
    id: requestId,
    owner: fork.owner,
    name: fork.name,
    source: `${owner}/library`,
    fork_source: source.id,
    visibility,
    default_branch_only: true,
    state: "interrupted",
    message: "Fork interrupted",
    resumable: true,
  });
  await page.route("**/api/**", (route) => {
    const request = route.request();
    const { pathname, searchParams } = new URL(request.url());
    if (pathname === "/api/session")
      return route.fulfill({
        json: {
          authenticated: true,
          csrf: "csrf",
          mode: "oidc",
          user: { issuer: "fixture", subject: "alex", name: "Alex" },
        },
      });
    if (pathname === "/api/repos")
      return route.fulfill({
        json: {
          repositories: [
            searchParams.get("owner") === fork.owner ? fork : source,
          ],
        },
      });
    if (pathname.endsWith("/refs"))
      return route.fulfill({ json: { head: null, refs: [] } });
    if (pathname === "/api/organizations")
      return route.fulfill({
        json: {
          items: [
            {
              kind: "member",
              organization: target,
              role: "member",
              actor: source.id,
              can_create: true,
              can_create_private: true,
              can_create_public: true,
            },
            {
              kind: "member",
              organization: { id: source.id, name: "Another team" },
              role: "member",
              actor: source.id,
              can_create: true,
              can_create_private: false,
              can_create_public: false,
            },
          ],
          next: null,
        },
      });
    if (
      pathname === `/api/organizations/${sourceId}` ||
      pathname === `/api/organizations/${targetId}`
    )
      return route.fulfill({
        json: {
          organization: pathname.endsWith(targetId) ? target : sourceOrg,
          role: "member",
          actor: source.id,
          can_create: pathname.endsWith(targetId),
        },
      });
    if (pathname === `/api/organizations/${targetId}/repositories/creation`)
      return route.fulfill({ json: { owner: fork.owner, properties: [] } });
    if (pathname === `/api/repos/${owner}/library/forks`) {
      expect(request.headers()["x-crab-repository-id"]).toBe(source.id);
      const input = request.postDataJSON();
      expect(input).toMatchObject({
        name: "library",
        destination: "organization",
        organization: targetId,
        default_branch_only: true,
      });
      requestId = input.request_id;
      return route.fulfill({ status: 202, json: job() });
    }
    if (requestId && pathname === `${jobRoot}/${requestId}`)
      return route.fulfill({ json: job() });
    if (requestId && pathname === `${jobRoot}/${requestId}/resume`)
      return route.fulfill({
        status: 202,
        json: {
          ...job(),
          state: "succeeded",
          repository: fork,
          resumable: false,
        },
      });
    return route.fulfill({
      status: 404,
      json: { error: { message: "Fixture route unavailable" } },
    });
  });
  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto(`/${owner}/library`);
  await page
    .getByRole("button", { name: "Fork repository", exact: true })
    .click();
  await expect(
    page.getByText(
      visibility === "public"
        ? "The fork is public and links to the source repository."
        : "The fork stays private and keeps a link to the source repository.",
    ),
  ).toBeVisible();
  await page
    .getByRole("button", { name: "Choose another organization" })
    .click();
  await expect(page.getByRole("button", { name: "Another team" })).toHaveCount(
    0,
  );
  await page.getByRole("button", { name: target.name }).click();
  const destination = page.getByRole("combobox", { name: "Fork owner" });
  await expect(destination).toHaveText(target.name);
  await expect(page.getByLabel("Repository name")).toHaveValue("library");
  await page.getByRole("button", { name: "Create fork", exact: true }).click();
  await expect(
    page.getByText("Fork interrupted", { exact: true }),
  ).toBeVisible();
  await page.reload();
  await expect(destination).toHaveText(target.name);
  await expect(destination).toBeDisabled();
  const statusLink = page.getByRole("link", { name: "Fork status link" });
  await expect(statusLink).toHaveAttribute(
    "href",
    `/-/organizations/${targetId}?tab=repositories&import=${requestId}`,
  );
  await statusLink.click();
  await page.getByRole("button", { name: "Resume fork", exact: true }).click();
  await expect(page).toHaveURL(`/${fork.owner}/library`);
}

for (const visibility of ["private", "public"] as const) {
  test(`a ${visibility} fork can choose another organization and resume there`, ({
    page,
  }) => chooseOrganizationFork(page, visibility));
}
