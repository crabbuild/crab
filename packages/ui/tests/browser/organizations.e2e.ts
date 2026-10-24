import { expect, test, type Page } from "@playwright/test";
import {
  expectNoAccessibilityViolations,
  selectDarkTheme,
} from "./accessibility";
import type {
  Invitation,
  Membership,
  Organization,
  OrganizationRole,
} from "../../src/features/organizations/model";

const id = "01900000-0000-7000-8000-000000000010";
const alice = "01900000-0000-7000-8000-000000000011";
const bob = "01900000-0000-7000-8000-000000000012";
const root = `/-/organizations/${id}`;
async function fixture(page: Page) {
  const state: {
    organization: Organization;
    role: OrganizationRole;
    members: Membership[];
    invitation: Invitation | null;
    writes: unknown[];
    fail: boolean;
    metadataVisible: boolean;
  } = {
    organization: {
      id,
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
  };
  await page.route("**/api/**", async (route) => {
    const request = route.request();
    const url = new URL(request.url());
    const path = url.pathname;
    const method = request.method();
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
    if (path === `${base}/members/${alice}` && method === "DELETE")
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
          },
        });
      if (method === "PUT") {
        const input = request.postDataJSON() as {
          version: number;
          role: OrganizationRole;
        };
        state.invitation = {
          recipient: bob,
          role: input.role,
          version: input.version + 1,
          state: "pending",
          created_at_ms: Date.now(),
          expires_at_ms: Date.now() + 7 * 86400000,
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

test("owners invite by stable account, preserve a conflicted form, and cancel without granting membership", async ({
  page,
}) => {
  const state = await fixture(page);
  await page.goto(`${root}?tab=invitations`);
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
  await page.goto(root);
  await page.getByRole("button", { name: "Remove", exact: true }).click();
  await page
    .getByRole("button", { name: "Remove member", exact: true })
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

test("organization settings and read-only people remain accessible at mobile widths in both themes", async ({
  page,
}) => {
  const state = await fixture(page);
  await page.goto(`${root}?tab=settings`);
  await page.getByLabel("Display name", { exact: true }).fill("Crab research");
  state.organization = {
    ...state.organization,
    name: "Concurrent saved name",
    version: 2,
  };
  await page.getByRole("button", { name: "Save changes", exact: true }).click();
  await expect(
    page.getByText("Concurrent saved name", { exact: true }),
  ).toBeVisible();
  await expect(page.getByLabel("Display name", { exact: true })).toHaveValue(
    "Crab research",
  );
  await page
    .getByRole("button", {
      name: "Keep draft with current version",
      exact: true,
    })
    .click();
  await page.getByRole("button", { name: "Save changes", exact: true }).click();
  await expect(
    page.getByRole("status").filter({ hasText: /^Organization updated\.$/ }),
  ).toBeVisible();
  await page.reload();
  await expect(page.getByLabel("Display name", { exact: true })).toHaveValue(
    "Crab research",
  );
  await page.setViewportSize({ width: 390, height: 844 });
  await expectNoAccessibilityViolations(page);
  await selectDarkTheme(page);
  await expectNoAccessibilityViolations(page);
  state.role = "member";
  await page.goto(root);
  await expect(
    page.getByRole("link", { name: "Settings", exact: true }),
  ).toHaveCount(0);
  await expect(
    page.getByRole("button", { name: "Remove", exact: true }),
  ).toHaveCount(0);
  await expect(
    page.getByRole("link", { name: "Alice", exact: true }),
  ).toBeVisible();
  expect(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= innerWidth,
    ),
  ).toBe(true);
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
