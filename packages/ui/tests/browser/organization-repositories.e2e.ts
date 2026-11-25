import { expect, test } from "@playwright/test";
import { expectNoAccessibilityViolations } from "./accessibility";

test("organization repository creation recovers its owner-scoped request after reload", async ({
  page,
}) => {
  const organization = "01900000-0000-7000-8000-000000000010";
  const alice = "01900000-0000-7000-8000-000000000011";
  const bob = "01900000-0000-7000-8000-000000000012";
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
        return route.fulfill({ json: { owner, default_branch: "trunk" } });
      expect(actor).toBe(alice);
      expect(request.headers()["x-csrf-token"]).toBe("fixture-csrf");
      if (!created) {
        intent = request.postDataJSON();
        expect(intent).toMatchObject({
          name: repository.name,
          description: repository.description,
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
  await page.getByLabel("Repository name").fill("widgets");
  await page.getByLabel("Description (optional)").fill("Team repository");
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

test("a current organization owner can resume a shared import", async ({
  page,
}) => {
  const organization = "01900000-0000-7000-8000-000000000020";
  const alice = "01900000-0000-7000-8000-000000000021";
  const bob = "01900000-0000-7000-8000-000000000022";
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
  let role = "owner";
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
    const job = { id: jobId, source, owner, name: repository.name };
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
        token: "initial-token",
      });
      jobId = input.request_id;
      return route.fulfill({
        status: 202,
        json: { ...interrupted, id: jobId },
      });
    }
    if (pathname === `${imports}/${jobId}`)
      return route.fulfill({ json: interrupted });
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

  role = "owner";
  await page.reload();
  await expect(
    page.getByText("The import worker stopped reporting progress."),
  ).toBeVisible();
  await expect(page.getByLabel("Source token")).toHaveValue("");
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
    forked_from: { id, owner, name: "library" },
  };
  let jobId: string | undefined;
  let reads = 0;
  const job = () => ({
    id: jobId,
    owner: fork.owner,
    name: fork.name,
    source: `${owner}/library`,
    fork_source: id,
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
            searchParams.get("owner") === fork.owner ? fork : source,
          ],
        },
      });
    if (pathname.endsWith("/refs"))
      return route.fulfill({
        json: { head: null, unborn_head: "refs/heads/main", refs: [] },
      });
    if (pathname === `/api/repos/${owner}/library/forks`) {
      expect(req.headers()["x-crab-repository-id"]).toBe(id);
      const input = req.postDataJSON();
      expect(input).toEqual({
        request_id: expect.any(String),
        name: "library",
        destination: "personal",
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

  await page.goto(`/${owner}/library`);
  await page
    .getByRole("button", { name: "Fork repository", exact: true })
    .click();
  const destination = page.getByRole("combobox", { name: "Fork owner" });
  await destination.click();
  await page.getByRole("option", { name: "Personal", exact: true }).click();
  await expect(page.getByLabel("Repository name")).toHaveValue("library");
  await page.getByRole("button", { name: "Create fork", exact: true }).click();
  await expect(
    page.getByText("Fork interrupted", { exact: true }),
  ).toBeVisible();
  await expect(
    page.getByRole("link", { name: "Fork status link" }),
  ).toHaveCount(0);
  await page.reload();
  await expect(destination).toHaveText("Personal");
  await page.getByRole("button", { name: "Resume fork", exact: true }).click();
  await expect(page).toHaveURL(/\/user-personal\/library$/);
  expect(reads).toBeGreaterThan(0);
});
