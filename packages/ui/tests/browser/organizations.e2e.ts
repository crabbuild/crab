import { expect, test, type Page } from "@playwright/test";
import {
  expectNoAccessibilityViolations,
  selectDarkTheme,
} from "./accessibility";
import type {
  Invitation,
  MemberRepositoryAccess,
  OrganizationMembership,
  Organization,
  OrganizationProjectPolicy,
  OrganizationRole,
  Reinstatement,
} from "@/lib/contracts";

const id = "01900000-0000-7000-8000-000000000010";
const alice = "01900000-0000-7000-8000-000000000011";
const bob = "01900000-0000-7000-8000-000000000012";
const root = `/-/organizations/${id}`;
async function fixture(page: Page) {
  const state: {
    organization: Organization;
    role: OrganizationRole;
    members: OrganizationMembership[];
    invitation: Invitation | null;
    reinstatement?: Reinstatement;
    writes: unknown[];
    fail: boolean;
    metadataVisible: boolean;
    projectPolicy: OrganizationProjectPolicy;
    policy: {
      version: number;
      member_creation: boolean;
      member_creation_visibility: "private" | "public" | "any";
      member_transfer: boolean;
      member_deletion: boolean;
      private_forks: boolean;
      member_visibility: boolean;
      baseline: "read" | "write" | "admin" | null;
      default_branch: string;
    };
  } = {
    organization: {
      id,
      handle: "crab-engineering",
      name: "Crab engineering",
      description: "Repository collaboration",
      version: 1,
      created_at_ms: 1700000000000,
      updated_at_ms: 1700000000000,
    },
    role: "owner",
    members: [
      {
        actor: alice,
        role: "owner",
        version: 1,
        profile: { id: alice, handle: "alice", name: "Alice" },
      },
    ],
    invitation: null,
    writes: [],
    fail: false,
    metadataVisible: true,
    policy: {
      version: 0,
      member_creation: false,
      member_creation_visibility: "private",
      member_transfer: false,
      member_deletion: false,
      private_forks: false,
      member_visibility: false,
      baseline: null,
      default_branch: "main",
    },
    projectPolicy: {
      version: 0,
      role: "read",
      affected_projects: 1,
      scope: "fixture-scope",
    },
  };
  await page.route("**/api/**", async (route) => {
    const request = route.request();
    const url = new URL(request.url());
    const path = url.pathname;
    const method = request.method();
    if (path === "/api/notifications/unread")
      return route.fulfill({ json: { unread: false } });
    if (path === "/api/session")
      return route.fulfill({
        json: {
          authenticated: true,
          mode: "oidc",
          csrf: "csrf",
          user: { issuer: "fixture", subject: "alice", name: "Alice" },
        },
      });
    if (path === "/api/repos")
      return route.fulfill({ json: { repositories: [] } });
    if (method !== "GET") {
      expect(request.headers()["x-csrf-token"]).toBe("csrf");
      state.writes.push(request.postDataJSON());
      if (state.fail)
        return route.fulfill({
          status: 409,
          json: {
            error: {
              code: "conflict",
              message: "This invitation changed; reload before saving",
            },
          },
        });
    }
    const base = `/api/organizations/${id}`;
    if (path === `${base}/project-policy`)
      return route.fulfill({ json: state.projectPolicy });
    if (path === `${base}/projects/commands` && method === "POST") {
      const { command } = request.postDataJSON();
      expect(command.kind).toBe("set_default_access");
      const { version, scope } = state.projectPolicy;
      if (command.version !== version || command.scope !== scope)
        return route.fulfill({
          json: {
            outcome: "rejected",
            reason:
              "The default role or affected Projects changed. Review the current settings before saving.",
          },
        });
      state.projectPolicy = {
        ...state.projectPolicy,
        role: command.role,
        version: version + 1,
      };
      return route.fulfill({
        json: { outcome: "applied", project: null, version: version + 1 },
      });
    }
    if (path === `${base}/repository-policy`) {
      if (method === "PUT") {
        const input = request.postDataJSON();
        expect(input.version).toBe(state.policy.version);
        state.policy = { ...input, version: input.version + 1 };
      }
      return route.fulfill({ json: state.policy });
    }
    if (path === `/api/organizations/by-handle/${state.organization.handle}`) {
      if (!state.metadataVisible) return route.fulfill({ status: 404 });
      return route.fulfill({
        json: {
          organization: state.organization,
          role: state.role,
          actor: alice,
        },
      });
    }
    if (path === base) {
      if (method === "PATCH") {
        const input = request.postDataJSON() as Partial<Organization>;
        if (input.version !== state.organization.version)
          return route.fulfill({
            status: 409,
            json: {
              error: {
                code: "conflict",
                message: "Organization profile changed",
              },
            },
          });
        state.organization = {
          ...state.organization,
          ...(request.postDataJSON() as Partial<Organization>),
          version: state.organization.version + 1,
        };
        return route.fulfill({ json: state.organization });
      }
      return route.fulfill({
        json: {
          organization: state.organization,
          role: state.role,
          actor: alice,
        },
      });
    }
    if (path === `${base}/members`)
      return route.fulfill({
        json: {
          items: state.members,
          next: null,
          revision: state.organization.version,
          can_manage: state.role === "owner",
        },
      });
    if (path === `${base}/members/${alice}` && method === "GET") {
      const { role, version } = state.members[0];
      return route.fulfill({
        json: { member: { actor: alice, role, active: true, version } },
      });
    }
    if (path === `${base}/members/removal` && method === "POST")
      return route.fulfill({
        status: 400,
        json: {
          error: {
            code: "invalid_request",
            message: "An organization must retain at least one owner",
          },
        },
      });
    if (path === `${base}/invitations/candidates`)
      return route.fulfill({
        json: {
          items: [{ id: bob, name: "Bob", handle: "bob" }],
          truncated: false,
        },
      });
    if (path === `${base}/invitations`)
      return route.fulfill({
        json: {
          items:
            state.invitation?.state === "pending"
              ? [
                  {
                    invitation: state.invitation,
                    profile: { id: bob, name: "Bob", handle: "bob" },
                  },
                ]
              : [],
          next: null,
          revision: 1,
        },
      });
    if (path === `${base}/invitations/${bob}`) {
      if (method === "GET")
        return route.fulfill({
          json: {
            organization: state.organization,
            invitation: state.invitation,
            reinstatement: state.reinstatement,
          },
        });
      if (method === "PUT") {
        const input = request.postDataJSON() as {
          version: number;
          role: OrganizationRole;
          reinstate_version?: number;
        };
        state.invitation = {
          recipient: bob,
          role: input.role,
          member_version: input.reinstate_version ?? 0,
          version: input.version + 1,
          state: "pending",
          created_at_ms: Date.now(),
          expires_at_ms: Date.now() + 7 * 86400000,
          reinstates: input.reinstate_version !== undefined,
        };
      }
      if (method === "DELETE" && state.invitation)
        state.invitation = {
          ...state.invitation,
          version: state.invitation.version + 1,
          state: "cancelled",
        };
      return route.fulfill({ json: state.invitation });
    }
    if (path === `${base}/invitation`) {
      if (method === "POST" && state.invitation) {
        const input = request.postDataJSON() as { accept: boolean };
        state.invitation = {
          ...state.invitation,
          state: input.accept ? "accepted" : "declined",
          version: state.invitation.version + 1,
        };
        return route.fulfill({ json: state.invitation });
      }
      return route.fulfill({
        json: {
          organization: state.metadataVisible ? state.organization : null,
          invitation: state.invitation,
        },
      });
    }
    throw new Error(`Unexpected request: ${method} ${path}`);
  });
  return state;
}

test("organization owners save repository policy from settings", async ({
  page,
}) => {
  const state = await fixture(page);
  await page.goto(root);
  await page.getByRole("link", { name: "Settings", exact: true }).click();
  await page.getByRole("link", { name: "Member privileges" }).click();
  await page.getByLabel("Default branch for new repositories").fill("trunk");
  await page
    .getByRole("combobox", { name: "Who can create repositories" })
    .click();
  await page.getByRole("option", { name: "All organization members" }).click();
  await page
    .getByRole("combobox", { name: "Visibilities members can create" })
    .click();
  await page.getByRole("option", { name: "Public and private" }).click();
  await page
    .getByRole("combobox", { name: "Who can transfer repositories" })
    .click();
  await page
    .getByRole("option", {
      name: "Owners and members with repository Admin access",
    })
    .click();
  await page
    .getByRole("combobox", { name: "Who can delete repositories" })
    .click();
  await page
    .getByRole("option", {
      name: "Owners and members with repository Admin access",
    })
    .click();
  await page.setViewportSize({ width: 390, height: 844 });
  const basePermissions = page.getByRole("combobox", {
    name: "Base repository permissions",
  });
  await basePermissions.click();
  await page.getByRole("option", { name: "Admin", exact: true }).click();
  await page
    .getByRole("checkbox", { name: "Allow forking of private repositories" })
    .check();
  await page
    .getByRole("checkbox", {
      name: "Allow private forks in other organizations",
    })
    .check();
  const savePolicy = page.getByRole("button", {
    name: "Save repository policy",
  });
  const review = page.getByRole("dialog", {
    name: "Change base repository permissions?",
  });
  await savePolicy.click();
  await expect(review).toContainText("Change from None to Admin");
  await expectNoAccessibilityViolations(page);
  await review.getByRole("button", { name: "Cancel", exact: true }).click();
  await expect(basePermissions).toBeFocused();
  expect(state.writes).toHaveLength(0);
  await savePolicy.click();
  await review
    .getByRole("button", { name: "Change base permissions to Admin" })
    .click();
  await expect(page.getByText("Repository policy saved.")).toBeVisible();
  await expect(review).toHaveCount(0);
  expect(state.policy).toEqual({
    version: 1,
    member_creation: true,
    member_creation_visibility: "any",
    member_transfer: true,
    member_deletion: true,
    private_forks: true,
    external_forks: true,
    member_visibility: false,
    baseline: "admin",
    default_branch: "trunk",
  });
  await expectNoAccessibilityViolations(page);
});

test("private fork settings respect the organization policy", async ({
  page,
}) => {
  const state = await fixture(page);
  const repo = {
    id: "01900000-0000-7000-8000-000000000020",
    owner: "org-crab",
    name: "library",
    organization_id: id,
    can_admin: true,
  };
  const settings = `/${repo.owner}/${repo.name}/settings`;
  const endpoint = `/api/repositories/${repo.id}/forking`;
  let forking = { version: 0, allowed: true };
  await page.route("**/api/**", async (route) => {
    const request = route.request();
    const { pathname } = new URL(request.url());
    if (pathname === "/api/repos") {
      return route.fulfill({
        json: {
          repositories: [
            {
              ...repo,
              forking: {
                ...forking,
                organization_allows: state.policy.private_forks,
              },
            },
          ],
        },
      });
    }
    if (pathname.endsWith("/refs"))
      return route.fulfill({ json: { head: null, refs: [] } });
    if (pathname.endsWith("/settings/merge-methods"))
      return route.fulfill({ json: { version: 0, enabled: ["merge_commit"] } });
    if (pathname === endpoint) {
      expect(request.method()).toBe("PUT");
      expect(request.headers()["x-csrf-token"]).toBe("csrf");
      expect(state.policy.private_forks).toBe(true);
      expect(request.postDataJSON()).toEqual({ allowed: false, version: 0 });
      forking = { allowed: false, version: 1 };
      return route.fulfill({ json: forking });
    }
    return route.fallback();
  });

  await page.goto(settings);
  const repositoryPolicy = page.getByRole("checkbox", {
    name: "Allow forking",
    exact: true,
  });
  await expect(repositoryPolicy).toBeDisabled();
  await expect(repositoryPolicy).not.toBeChecked();
  await page.goto(`${root}?tab=privileges`);
  const organizationPolicy = page.getByRole("checkbox", {
    name: "Allow forking of private repositories",
  });
  await organizationPolicy.click();
  await page.getByRole("button", { name: "Save repository policy" }).click();
  await expect(page.getByText("Repository policy saved.")).toBeVisible();
  await page.goto(settings);
  await expect(repositoryPolicy).toBeChecked();
  await repositoryPolicy.click();
  await page.getByRole("button", { name: "Save forking setting" }).click();
  await expect.poll(() => forking).toEqual({ version: 1, allowed: false });
  await page.reload();
  await expect(repositoryPolicy).toBeEnabled();
  await expect(repositoryPolicy).not.toBeChecked();
  await expectNoAccessibilityViolations(page);
});

test("organization owners delete and restore repositories", async ({
  page,
}) => {
  await fixture(page);
  await page.setViewportSize({ width: 390, height: 844 });
  const repo = {
    id: "01900000-0000-7000-8000-000000000021",
    owner: "org-crab",
    name: "library",
    organization_id: id,
    can_admin: true,
    archive_version: 0,
  };
  const repository = `/${repo.owner}/${repo.name}`;
  const deletion = `/api/repositories/${repo.id}/deletion`;
  const deleted = `/api/organizations/${id}/deleted-repositories`;
  const deletedAt = Date.now();
  let submission: { request_id: string } | null = null;
  let removed = false;
  await page.route("**/api/**", async (route) => {
    const request = route.request();
    const { pathname } = new URL(request.url());
    if (pathname === "/api/repos")
      return route.fulfill({ json: { repositories: removed ? [] : [repo] } });
    if (pathname.endsWith("/refs"))
      return route.fulfill({ json: { head: null, refs: [] } });
    if (pathname.endsWith("/settings/merge-methods"))
      return route.fulfill({ json: { version: 0, enabled: ["merge_commit"] } });
    if (pathname.endsWith("/pulls/recent-pushes"))
      return route.fulfill({ json: { items: [] } });
    if (pathname === deletion) {
      if (request.method() === "GET")
        return route.fulfill({
          json: { version: 0, can_delete: true, forks: 1 },
        });
      expect(request.method()).toBe("DELETE");
      expect(request.headers()["x-csrf-token"]).toBe("csrf");
      if (!submission) {
        submission = request.postDataJSON();
        expect(submission).toEqual({
          request_id: expect.any(String),
          version: 0,
          confirmation: "org-crab/library",
        });
        removed = true;
        return route.abort("failed");
      }
      expect(request.postDataJSON()).toEqual(submission);
      return route.fulfill({ json: { organization: id } });
    }
    if (pathname === deleted) {
      return route.fulfill({
        json: {
          items: removed
            ? [
                {
                  ...repo,
                  deletion_id: submission?.request_id,
                  deleted_at_ms: deletedAt,
                  expires_at_ms: deletedAt + 90 * 86_400_000,
                  can_restore: true,
                  forks: 1,
                },
              ]
            : [],
          next: null,
        },
      });
    }
    if (pathname === `${deleted}/${repo.id}`) {
      expect(request.method()).toBe("PUT");
      expect(request.headers()["x-csrf-token"]).toBe("csrf");
      expect(request.postDataJSON()).toEqual({
        deletion_id: submission?.request_id,
      });
      removed = false;
      return route.fulfill({ json: repo });
    }
    return route.fallback();
  });

  await page.goto(`${repository}/settings`);
  await page.getByRole("button", { name: "Delete this repository" }).click();
  const dialog = page.getByRole("dialog");
  await expect(dialog).toContainText("This also deletes 1 private fork.");
  const remove = dialog.getByRole("button", { name: "Delete repository" });
  await expect(remove).toBeDisabled();
  await dialog
    .getByLabel("Type org-crab/library to confirm deletion")
    .fill("org-crab/library");
  await remove.click();
  await expect(dialog.getByRole("alert")).toBeVisible();
  await remove.click();
  await expect(page).toHaveURL(/tab=repositories$/);
  await expect(
    page.getByRole("link", { name: "library", exact: true }),
  ).toHaveCount(0);
  await page.getByRole("link", { name: "Settings", exact: true }).click();
  await page
    .getByRole("link", { name: "Deleted repositories", exact: true })
    .click();
  await page.getByRole("button", { name: "Restore library" }).click();
  await expect(dialog).toContainText("Team grants need to be added again.");
  await expectNoAccessibilityViolations(page);
  await dialog.getByRole("button", { name: "Restore repository" }).click();
  await page.getByRole("status").getByRole("link", { name: "library" }).click();
  await expect(page).toHaveURL(new RegExp(`${repository}$`));
});

test("organization Project policy requires a new confirmation when the inheriting set changes", async ({
  page,
}) => {
  const state = await fixture(page);
  await page.goto(`${root}?tab=project-policy`);
  const access = page.getByRole("combobox", { name: "Default Project access" });
  const review = page.getByRole("button", { name: "Review change" });
  const dialog = page.getByRole("dialog", {
    name: "Change default Project access?",
  });
  const confirm = dialog.getByRole("button", {
    name: "Change default to No access",
  });

  await access.click();
  await page.getByRole("option", { name: "No access", exact: true }).click();
  await review.click();
  await expect(dialog).toContainText("This affects 1 existing Project");
  expect(state.writes).toHaveLength(0);
  state.projectPolicy = {
    ...state.projectPolicy,
    affected_projects: 2,
    scope: "updated-scope",
  };
  await confirm.click();
  await expect(
    page.getByRole("region", { name: "Project policy" }).getByRole("alert"),
  ).toContainText("affected Projects changed");
  await expect(access).toHaveText("Read");
  expect(state.projectPolicy.role).toBe("read");

  await access.click();
  await page.getByRole("option", { name: "No access", exact: true }).click();
  await review.click();
  await expect(dialog).toContainText("This affects 2 existing Projects");
  await confirm.click();
  await expect.poll(() => state.projectPolicy.role).toBe("none");
  await expect(access).toHaveText("No access");
  await expect(access).toBeEnabled();
  await expect(review).toBeDisabled();
});

test("audit filters submit the selected action with a repository name", async ({
  page,
}) => {
  await fixture(page);
  await page.route(`**/api/organizations/${id}/audit-log*`, (route) =>
    route.fulfill({ json: { items: [], next: null } }),
  );
  await page.goto(`${root}?tab=settings`);
  await page.getByRole("link", { name: "Audit log", exact: true }).click();
  const action = page.locator("#audit-action");
  await action.click();
  await page
    .getByRole("option", { name: "Repository access changed", exact: true })
    .click();
  await page.locator("#audit-repository").fill("library");
  await expect(action).toHaveText("Repository access changed");
  await page.getByRole("button", { name: "Apply filters" }).click();
  await expect(page).toHaveURL(
    /action=repository_access_changed.*repository=library/,
  );
});

test("owners see identity-provider management and blocked access in People", async ({
  page,
}) => {
  await fixture(page);
  await page.route(
    `**/api/organizations/${id}/members/${alice}/repository-access?*`,
    (route) =>
      route.fulfill({
        json: {
          account: "suspended",
          managed: true,
          items: [],
          next: null,
        } satisfies MemberRepositoryAccess,
      }),
  );
  await page.goto(`${root}?tab=people`);
  await page.getByRole("button", { name: "Actions for Alice" }).click();
  await page.getByRole("menuitem", { name: "View repository access" }).click();
  const dialog = page.getByRole("dialog", { name: "Repository access" });
  await expect(
    dialog.getByText("Provisioned by identity provider"),
  ).toBeVisible();
  await expect(dialog.getByRole("alert")).toContainText(
    "Sign-in and user Git credentials are blocked for this account",
  );
  await expectNoAccessibilityViolations(page);
});

test("owners find an outside collaborator, inspect grants, and invite them", async ({
  page,
}) => {
  const state = await fixture(page);
  const people = `/api/organizations/${id}/outside-collaborators`;
  const profile = { id: bob, name: "Bob", handle: "bob" };
  const key = "c".repeat(64);
  const repository = "01900000-0000-7000-8000-000000000021";
  await page.route(
    (url) => url.pathname === people,
    (route) => {
      const q = new URL(route.request().url()).searchParams.get("q");
      return route.fulfill({
        json: {
          items:
            !q || q === "bob"
              ? [{ key, name: profile.name, profile, repository_count: 1 }]
              : [],
          next: null,
          revision: 1,
        },
      });
    },
  );
  await page.route(
    (url) => url.pathname === `${people}/${key}/repositories`,
    (route) =>
      route.fulfill({
        json: {
          items: [
            {
              id: repository,
              owner: "org-crab",
              name: "library",
              access: "write",
            },
          ],
          next: null,
          revision: 1,
        },
      }),
  );
  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto(`${root}?tab=people`);
  await page.getByRole("link", { name: "Outside collaborators" }).click();
  const search = page.getByRole("search", {
    name: "Outside collaborator search",
  });
  await search.getByRole("searchbox").fill("missing");
  await search.getByRole("button", { name: "Search" }).click();
  await expect(page.getByText("No matching collaborators")).toBeVisible();
  await search.getByRole("searchbox").fill("bob");
  await search.getByRole("button", { name: "Search" }).click();
  const access = page.getByRole("button", {
    name: "View repository access for Bob",
  });
  await access.focus();
  await page.keyboard.press("Enter");
  const grants = page.getByRole("list", { name: "Collaborator repositories" });
  await expect(grants.getByText("Write", { exact: true })).toBeVisible();
  await expect(
    grants.getByRole("link", { name: "Manage access to library" }),
  ).toHaveAttribute(
    "href",
    `/org-crab/library/settings/access?repository_id=${repository}`,
  );
  expect(
    await page.evaluate(() => document.documentElement.scrollWidth),
  ).toBeLessThanOrEqual(390);
  await expectNoAccessibilityViolations(page);
  await selectDarkTheme(page);
  await expectNoAccessibilityViolations(page);
  await page.getByRole("button", { name: "Invite to organization" }).click();
  const invite = page.getByRole("dialog", { name: "Invite a member" });
  await expect(invite.getByLabel("Username")).toHaveValue("bob");
  await invite.getByRole("button", { name: "Create invitation" }).click();
  await expect(
    page
      .getByRole("status")
      .filter({ hasText: "Invitation sent to the collaborator." }),
  ).toBeVisible();
  expect(state.invitation?.recipient).toBe(bob);

  state.role = "member";
  await page.goto(`${root}?tab=people`);
  await expect(
    page.getByRole("heading", { name: "Members", exact: true }),
  ).toBeVisible();
  await expect(
    page.getByRole("link", { name: "Outside collaborators" }),
  ).toHaveCount(0);
});

test("bulk collaborator removal refreshes stale plans and recovers a lost reply", async ({
  page,
}) => {
  await fixture(page);
  const rootApi = `/api/organizations/${id}/outside-collaborators`;
  const first = "a".repeat(64);
  const second = "b".repeat(64);
  const people = [
    { key: first, name: "Northwind", profile: null, repository_count: 1 },
    { key: second, name: "Vendor team", profile: null, repository_count: 2 },
  ];
  let version = 7;
  let writes = 0;
  await page.route(
    (url) => url.pathname === rootApi,
    (route) =>
      route.fulfill({ json: { items: people, next: null, revision: version } }),
  );
  const plan = () => ({
    organization_revision: 1,
    catalog_revision: version,
    collaborators: [first, second],
    confirmation: "fixture-confirmation",
  });
  let accepted: ReturnType<typeof plan> | undefined;
  await page.route(
    (url) => url.pathname === `${rootApi}/removal-preview`,
    (route) => {
      expect(route.request().postDataJSON()).toEqual({
        collaborators: [first, second],
      });
      return route.fulfill({
        json: {
          plan: plan(),
          impact: { collaborators: 2, repositories: 2, grants: 3 },
          repository_names: ["library", "private-tool"],
        },
      });
    },
  );
  await page.route(
    (url) => url.pathname === `${rootApi}/removals`,
    (route) => {
      const request = route.request();
      expect(request.method()).toBe("POST");
      expect(request.headers()["x-csrf-token"]).toBe("csrf");
      expect(request.postDataJSON()).toEqual(accepted ?? plan());
      writes += 1;
      version += 1;
      if (writes === 1)
        return route.fulfill({
          status: 409,
          json: {
            error: { code: "conflict", message: "Repository access changed." },
          },
        });
      if (!accepted) {
        accepted = request.postDataJSON();
        people.length = 0;
        return route.abort("failed");
      }
      return route.fulfill({
        json: { collaborators: 2, repositories: 2, grants: 3 },
      });
    },
  );
  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto(`${root}?tab=outside-collaborators`);
  const trigger = page.getByRole("button", {
    name: "Remove from all repositories",
  });
  const dialog = page.getByRole("dialog", {
    name: "Remove outside collaborators",
  });
  async function selectCollaborators() {
    await page.getByRole("checkbox", { name: "Select Northwind" }).check();
    await page.getByRole("checkbox", { name: "Select Vendor team" }).check();
    await trigger.click();
    await expect(dialog.getByText("Northwind")).toBeVisible();
    await expect(dialog.getByText("Vendor team")).toBeVisible();
  }
  await selectCollaborators();
  await dialog.getByRole("button", { name: "Cancel" }).click();
  await expect(dialog).toHaveCount(0);
  expect(writes).toBe(0);
  await trigger.click();
  await expectNoAccessibilityViolations(page);
  await dialog
    .getByRole("button", { name: "Remove outside collaborators" })
    .click();
  await expect(dialog.getByRole("alert")).toContainText(
    "Repository access changed.",
  );
  await dialog.getByRole("button", { name: "Reload collaborators" }).click();
  await expect(trigger).toHaveCount(0);
  await selectCollaborators();
  await dialog
    .getByRole("button", { name: "Remove outside collaborators" })
    .click();
  await expect(
    dialog.getByRole("button", { name: "Retry removal" }),
  ).toBeEnabled();
  await page.reload();
  await dialog.getByRole("button", { name: "Retry removal" }).click();
  await expect(
    page.getByRole("status").filter({
      hasText: "Removed 3 direct repository grants for 2 collaborators.",
    }),
  ).toBeVisible();
  await expect(page.getByText("No outside collaborators")).toBeVisible();
  expect(writes).toBe(3);
});

test("owners can review and convert multiple selected members", async ({
  page,
}) => {
  const state = await fixture(page);
  const profiles = [
    { id: bob, name: "Bob", handle: "bob" },
    {
      id: "01900000-0000-7000-8000-000000000013",
      name: "Casey",
      handle: "casey",
    },
  ];
  state.members.push(
    ...profiles.map((profile) => ({
      actor: profile.id,
      role: "member" as const,
      version: 3,
      profile,
    })),
  );
  const base = `/api/organizations/${id}`;
  let attempts = 0;
  await page.route(
    (url) => url.pathname === `${base}/members/conversion`,
    (route) => {
      expect(route.request().method()).toBe("POST");
      expect(route.request().headers()["x-csrf-token"]).toBe("csrf");
      expect(route.request().postDataJSON()).toEqual({
        members: profiles.map((profile) => ({
          actor: profile.id,
          version: 3 + attempts,
        })),
      });
      attempts += 1;
      if (attempts === 1) {
        for (const member of state.members) {
          if (member.actor !== alice) member.version = 4;
        }
        return route.fulfill({
          status: 409,
          json: { error: { code: "conflict", message: "Member changed" } },
        });
      }
      state.members = state.members.filter((member) => member.actor === alice);
      return route.fulfill({ json: { converted: profiles.length } });
    },
  );
  await page.route(
    (url) => url.pathname === `${base}/outside-collaborators`,
    (route) =>
      route.fulfill({
        json: {
          items:
            attempts === 2
              ? profiles.map((profile, index) => ({
                  key: String(index).repeat(64),
                  name: profile.name,
                  profile,
                  repository_count: 2,
                }))
              : [],
          next: null,
          revision: 2,
        },
      }),
  );
  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto(`${root}?tab=people`);
  const all = page.getByRole("checkbox", { name: "Select page" });
  const trigger = page.getByRole("button", {
    name: "2 selected",
    exact: true,
  });
  const dialog = page.getByRole("dialog", {
    name: "Convert to outside collaborators",
  });
  async function reviewSelection() {
    await all.check();
    await page.getByRole("checkbox", { name: "Select Alice" }).uncheck();
    await expect(all).toHaveAttribute("aria-checked", "mixed");
    await trigger.click();
    await page
      .getByRole("menuitem", { name: "Convert to outside collaborators" })
      .click();
    await expect(dialog).toContainText("team-derived repository roles");
    await expect(dialog.getByText("Bob", { exact: true })).toBeVisible();
    await expect(dialog.getByText("Casey", { exact: true })).toBeVisible();
    await expect(dialog.getByText("Alice", { exact: true })).toHaveCount(0);
  }
  await reviewSelection();
  await expectNoAccessibilityViolations(page);
  await dialog
    .getByRole("button", { name: "Convert to outside collaborators" })
    .click();
  await expect(dialog.getByRole("alert")).toContainText("Member changed");
  await dialog.getByRole("button", { name: "Reload members" }).click();
  await expect(trigger).toHaveCount(0);
  await reviewSelection();
  await dialog
    .getByRole("button", { name: "Convert to outside collaborators" })
    .click();
  await expect(
    page
      .getByRole("status")
      .filter({ hasText: "Converted 2 members to outside collaborators." }),
  ).toBeVisible();
  await expect(
    page.getByRole("link", { name: "Bob", exact: true }),
  ).toHaveCount(0);
  await expect(
    page.getByRole("link", { name: "Casey", exact: true }),
  ).toHaveCount(0);
  await page.getByRole("link", { name: "Outside collaborators" }).click();
  for (const profile of profiles) {
    await expect(
      page.getByRole("link", { name: profile.name, exact: true }),
    ).toBeVisible();
    await expect(
      page.getByRole("button", {
        name: `View repository access for ${profile.name}`,
      }),
    ).toHaveText("2 repositories");
  }
});

test("owners review and remove selected members from People", async ({
  page,
}) => {
  const state = await fixture(page);
  const profiles = [
    { id: bob, name: "Bob", handle: "bob" },
    {
      id: "01900000-0000-7000-8000-000000000013",
      name: "Casey",
      handle: "casey",
    },
  ];
  state.members.push(
    ...profiles.map((profile, index) => ({
      actor: profile.id,
      role: "member" as const,
      version: index + 2,
      profile,
    })),
  );
  let writes = 0;
  await page.route(
    (url) => url.pathname === `/api/organizations/${id}/members/removal`,
    (route) => {
      expect(route.request().method()).toBe("POST");
      expect(route.request().headers()["x-csrf-token"]).toBe("csrf");
      expect(route.request().postDataJSON()).toEqual({
        members: profiles.map((profile, index) => ({
          actor: profile.id,
          version: index + 2,
        })),
      });
      writes += 1;
      state.members = state.members.filter((member) => member.actor === alice);
      return route.fulfill({ json: { removed: 2 } });
    },
  );
  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto(`${root}?tab=people`);
  await page.getByRole("checkbox", { name: "Select Bob" }).check();
  await page.getByRole("checkbox", { name: "Select Casey" }).check();
  const trigger = page.getByRole("button", { name: "2 selected", exact: true });
  const remove = page.getByRole("menuitem", {
    name: "Remove from organization",
  });
  const dialog = page.getByRole("dialog", { name: "Remove from organization" });
  await trigger.press("Enter");
  await remove.press("Enter");
  await expect(dialog).toContainText(
    "personal private forks of organization repositories",
  );
  await expect(dialog.getByText("Bob", { exact: true })).toBeVisible();
  await expect(dialog.getByText("Casey", { exact: true })).toBeVisible();
  await expectNoAccessibilityViolations(page);
  await dialog.getByRole("button", { name: "Cancel", exact: true }).click();
  await expect(trigger).toBeFocused();
  expect(writes).toBe(0);
  await trigger.click();
  await remove.click();
  await dialog
    .getByRole("button", { name: "Remove members", exact: true })
    .click();
  await expect(
    page.getByRole("status").filter({ hasText: "Removed 2 members." }),
  ).toBeVisible();
  await expect(
    page.getByRole("checkbox", { name: "Select Alice" }),
  ).toBeVisible();
  for (const profile of profiles) {
    await expect(
      page.getByRole("checkbox", { name: `Select ${profile.name}` }),
    ).toHaveCount(0);
  }
  expect(writes).toBe(1);
});

test("owners invite by stable account, preserve a conflicted form, and cancel without granting membership", async ({
  page,
}) => {
  const state = await fixture(page);
  await page.goto(`${root}?tab=people`);
  await page
    .getByRole("button", { name: "Invite member", exact: true })
    .click();
  const dialog = page.getByRole("dialog");
  await dialog.getByLabel("Username", { exact: true }).fill("bob");
  await dialog.getByRole("button", { name: "Bob @bob", exact: true }).click();
  await expect(
    dialog.getByRole("button", { name: "Create invitation", exact: true }),
  ).toBeEnabled();
  await expectNoAccessibilityViolations(page);
  state.fail = true;
  await dialog
    .getByRole("button", { name: "Create invitation", exact: true })
    .click();
  await expect(dialog.getByRole("alert")).toContainText("invitation changed");
  await expect(dialog.getByLabel("Username", { exact: true })).toHaveValue(
    "bob",
  );
  state.fail = false;
  await dialog
    .getByRole("button", { name: "Reload current data", exact: true })
    .click();
  await dialog
    .getByRole("button", { name: "Create invitation", exact: true })
    .click();
  await expect(dialog).toHaveCount(0);
  await expect(page.getByRole("status")).toContainText("Invitation created.");
  await page.getByRole("link", { name: "Invitations", exact: true }).click();
  await expect(
    page.getByRole("link", { name: "People", exact: true }),
  ).toHaveAttribute("aria-current", "page");
  await expect(
    page.getByRole("button", { name: "Cancel invitation", exact: true }),
  ).toBeVisible();
  expect(state.writes.slice(0, 2)).toEqual([
    { version: 0, role: "member" },
    { version: 0, role: "member" },
  ]);
  expect(state.members).toHaveLength(1);
  await page
    .getByRole("button", { name: "Cancel invitation", exact: true })
    .click();
  await expect(
    page.getByText("No pending invitations.", { exact: true }),
  ).toBeVisible();
  expect(state.invitation?.state).toBe("cancelled");
  await page.goto(`${root}?tab=people`);
  await page.getByRole("button", { name: "Actions for Alice" }).click();
  await page.getByRole("menuitem", { name: "Leave organization…" }).click();
  await page
    .getByRole("button", { name: "Leave organization", exact: true })
    .click();
  await expect(page.getByRole("alert").last()).toContainText(
    "retain at least one owner",
  );
  expect(state.members).toHaveLength(1);
});

test("an invitee accepts through their own endpoint and reload shows the persisted decision", async ({
  page,
}) => {
  const state = await fixture(page);
  state.invitation = {
    recipient: bob,
    role: "member",
    version: 4,
    member_version: 0,
    state: "pending",
    created_at_ms: Date.now(),
    expires_at_ms: Date.now() + 7 * 86400000,
  };
  await page.goto(`${root}/invitation?powerbar=1`);
  await expect(
    page.getByRole("heading", {
      name: "Invitation to Crab engineering",
      exact: true,
    }),
  ).toBeVisible();
  await page
    .getByRole("button", { name: "Join organization", exact: true })
    .click();
  await expect(
    page.getByText("You accepted this invitation.", { exact: true }),
  ).toBeVisible();
  expect(state.writes).toEqual([{ version: 4, accept: true }]);
  await page.reload();
  await expect(
    page.getByRole("link", { name: "View organization", exact: true }),
  ).toBeVisible();
  await expect(
    page.getByRole("button", { name: "Join organization", exact: true }),
  ).toHaveCount(0);
});

test("owners can choose to restore a former member's previous access", async ({
  page,
}) => {
  const state = await fixture(page);
  state.reinstatement = { role: "owner", version: 4 };
  await page.goto(`${root}?tab=people`);
  await page
    .getByRole("button", { name: "Invite member", exact: true })
    .click();
  const dialog = page.getByRole("dialog");
  await dialog.getByLabel("Username", { exact: true }).fill("bob");
  await dialog.getByRole("button", { name: "Bob @bob", exact: true }).click();
  await expect(
    dialog.getByRole("button", { name: "Invite and start fresh" }),
  ).toBeEnabled();
  await dialog.getByRole("combobox", { name: "Previous access" }).click();
  await page.getByRole("option", { name: "Restore previous access" }).click();
  await expect(
    dialog.getByText("Rejoin as an owner", { exact: false }),
  ).toBeVisible();
  await expect(
    dialog.getByRole("combobox", { name: "Organization role" }),
  ).toHaveCount(0);
  await dialog.getByRole("button", { name: "Invite and reinstate" }).click();
  await expect(page.getByRole("status")).toContainText("Invitation created.");
  expect(state.writes).toEqual([
    { version: 0, role: "owner", reinstate_version: 4 },
  ]);
  await page.goto(`${root}/invitation`);
  await expect(
    page.getByText(
      "Accepting restores your eligible team and repository access.",
    ),
  ).toBeVisible();
});

test("organization settings and read-only people remain accessible at mobile widths in both themes", async ({
  page,
}) => {
  const state = await fixture(page);
  await page.goto(`${root}?tab=settings`);
  await page
    .getByLabel("Organization display name", { exact: true })
    .fill("Crab research");
  state.organization = {
    ...state.organization,
    name: "Concurrent saved name",
    version: 2,
  };
  await page
    .getByRole("button", { name: "Update profile", exact: true })
    .click();
  await expect(
    page.getByText("Concurrent saved name", { exact: true }),
  ).toBeVisible();
  await expect(
    page.getByLabel("Organization display name", { exact: true }),
  ).toHaveValue("Crab research");
  await page
    .getByRole("button", {
      name: "Keep draft with current version",
      exact: true,
    })
    .click();
  await page
    .getByRole("button", { name: "Update profile", exact: true })
    .click();
  await expect(
    page.getByRole("status").filter({ hasText: /^Organization updated\.$/ }),
  ).toBeVisible();
  await page.reload();
  await expect(
    page.getByLabel("Organization display name", { exact: true }),
  ).toHaveValue("Crab research");
  await page.setViewportSize({ width: 390, height: 844 });
  await expectNoAccessibilityViolations(page);
  await selectDarkTheme(page);
  await page.setViewportSize({ width: 320, height: 844 });
  await page.getByRole("link", { name: "Member privileges" }).click();
  await expect(
    page.getByRole("heading", { name: "Member privileges" }),
  ).toBeVisible();
  expect(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= innerWidth,
    ),
  ).toBe(true);
  await expectNoAccessibilityViolations(page);
  await page.getByRole("link", { name: "General", exact: true }).click();
  await page
    .getByLabel("Organization handle", { exact: true })
    .fill("crab-research");
  await page.getByLabel("Website", { exact: true }).fill("https://crab.build");
  await page.getByLabel("Email", { exact: true }).fill("ops@crab.build");
  await page.getByLabel("Location", { exact: true }).fill("San Francisco");
  await page
    .getByRole("button", { name: "Update profile", exact: true })
    .click();
  await expect(page).toHaveURL(/\/crab-research\?tab=settings$/);
  await page.getByRole("link", { name: "Overview", exact: true }).click();
  await expect(page.getByText("@crab-research")).toBeVisible();
  await expect(page.getByText("San Francisco")).toBeVisible();
  await expect(
    page.getByRole("link", { name: "crab.build", exact: true }),
  ).toBeVisible();
  await expect(
    page.getByRole("link", { name: "ops@crab.build", exact: true }),
  ).toBeVisible();
  await page.goto(`${root}?tab=settings`);
  state.role = "member";
  await page.reload();
  await expect(page.getByRole("alert")).toContainText(
    "This organization page is unavailable.",
  );
  await page.goto(`${root}?tab=people`);
  await expect(
    page.getByRole("link", { name: "Settings", exact: true }),
  ).toHaveCount(0);
  await page.getByRole("button", { name: "Actions for Alice" }).click();
  await expect(page.getByRole("menuitem")).toHaveText(["Leave organization…"]);
  await page.keyboard.press("Escape");
  await expect(
    page.getByRole("link", { name: "Alice", exact: true }),
  ).toBeVisible();
  expect(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= innerWidth,
    ),
  ).toBe(true);
});

test("members leave from their own People row or from Your organizations", async ({
  page,
}) => {
  const state = await fixture(page);
  state.role = "member";
  state.members = [{ ...state.members[0], role: "member", version: 3 }];
  let member = true;
  const removals: unknown[] = [];
  await page.route(
    (url) => url.pathname === `/api/organizations/${id}/members/removal`,
    (route) => {
      removals.push(route.request().postDataJSON());
      member = false;
      return route.fulfill({ json: { removed: 1 } });
    },
  );
  await page.route(
    (url) => url.pathname === "/api/organizations",
    (route) =>
      route.fulfill({
        json: {
          items: member
            ? [
                {
                  kind: "member",
                  organization: state.organization,
                  actor: alice,
                  role: "member",
                  can_create: false,
                  can_create_private: false,
                  can_create_public: false,
                },
              ]
            : [],
          next: null,
        },
      }),
  );
  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto(`${root}?tab=people`);
  const actions = page.getByRole("button", { name: "Actions for Alice" });
  await actions.click();
  await page.getByRole("menuitem", { name: "Leave organization…" }).click();
  const dialog = page.getByRole("dialog", { name: "Leave Crab engineering?" });
  await expect(dialog).toContainText("private forks");
  await expectNoAccessibilityViolations(page);
  await dialog.getByRole("button", { name: "Cancel", exact: true }).click();
  await expect(actions).toBeFocused();
  expect(removals).toEqual([]);

  await page.goto("/-/organizations");
  await page
    .getByRole("button", { name: "Leave Crab engineering", exact: true })
    .click();
  await dialog
    .getByRole("button", { name: "Leave organization", exact: true })
    .click();
  await expect(
    page.getByRole("status").filter({ hasText: "You left Crab engineering." }),
  ).toBeVisible();
  await expect(
    page.getByRole("link", { name: "Crab engineering", exact: true }),
  ).toHaveCount(0);
  expect(removals).toEqual([{ members: [{ actor: alice, version: 3 }] }]);
});

test("a former member can inspect their old decision without current metadata or an access link", async ({
  page,
}) => {
  const state = await fixture(page);
  state.metadataVisible = false;
  state.invitation = {
    recipient: bob,
    role: "member",
    version: 2,
    member_version: 0,
    state: "accepted",
    created_at_ms: 1700000000000,
    expires_at_ms: 1700604800000,
  };
  await page.goto(`${root}/invitation`);
  await expect(
    page.getByRole("heading", { name: "Organization invitation", exact: true }),
  ).toBeVisible();
  await expect(
    page.getByText("You accepted this invitation.", { exact: true }),
  ).toBeVisible();
  await expect(
    page.getByRole("link", { name: "View organization", exact: true }),
  ).toHaveCount(0);
  await expect(page.getByText("Crab engineering", { exact: true })).toHaveCount(
    0,
  );
  expect(state.writes).toEqual([]);
});

test("organization creation alert button preserves contrast and no underline in light and dark themes", async ({
  page,
}) => {
  await fixture(page);
  await page.route("**/api/profile", (route) =>
    route.fulfill({ json: { profile: null } }),
  );
  await page.goto("/-/organizations/new");
  const cta = page.getByRole("link", { name: "Set up your public profile" });
  await expect(cta).toBeVisible();
  for (const theme of ["light", "dark"] as const) {
    if (theme === "dark") {
      await selectDarkTheme(page);
    }
    const rest = await cta.evaluate((el) => {
      const style = getComputedStyle(el);
      return {
        color: style.color,
        backgroundColor: style.backgroundColor,
        textDecorationLine: style.textDecorationLine,
      };
    });
    expect(rest.textDecorationLine).toBe("none");
    expect(rest.color).not.toBe(rest.backgroundColor);
    await cta.hover();
    const hovered = await cta.evaluate((el) => {
      const style = getComputedStyle(el);
      return {
        color: style.color,
        backgroundColor: style.backgroundColor,
        textDecorationLine: style.textDecorationLine,
      };
    });
    expect(hovered.textDecorationLine).toBe("none");
    expect(hovered.color).not.toBe(hovered.backgroundColor);
    await expectNoAccessibilityViolations(page);
  }
});
