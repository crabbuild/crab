import { expect, test, type Page } from "@playwright/test";
import {
  expectNoAccessibilityViolations,
  selectDarkTheme,
} from "./accessibility";
import type {
  Member,
  Policy,
  Submission,
  View,
} from "../../src/features/organizations/teams/model";
const id = "01900000-0000-7000-8000-000000000010";
const alice = "01900000-0000-7000-8000-000000000011";
const bob = "01900000-0000-7000-8000-000000000012";
const root = `/-/organizations/${id}`;
async function fixture(page: Page, local = false) {
  const state: {
    teams: View[];
    members: Map<string, Member[]>;
    writes: { body: Submission; cancel: boolean; path: string }[];
    policy: Policy;
    lost: boolean;
    denied: boolean;
    revoked: boolean;
    conflict: boolean;
    account: string;
  } = {
    teams: [],
    members: new Map(),
    writes: [],
    policy: { version: 1, member_creation: true },
    lost: false,
    denied: false,
    revoked: false,
    conflict: false,
    account: "alice",
  };
  const receipts = new Map<
    string,
    { outcome: "applied" | "cancelled"; id: string }
  >();
  const profile = (actor: string) => ({
    id: actor,
    name: actor === alice ? "Alice" : "Bob",
    handle: actor === alice ? "alice" : "bob",
  });
  await page.route("**/api/**", async (route) => {
    const request = route.request(),
      url = new URL(request.url()),
      path = url.pathname;
    const base = `/api/organizations/${id}`;
    if (path === "/api/session")
      return route.fulfill({
        json: {
          authenticated: true,
          mode: local ? "local" : "oidc",
          csrf: local ? null : "csrf",
          user: local
            ? null
            : {
                issuer: "https://identity.example",
                subject: state.account,
                name: state.account,
              },
        },
      });
    if (path === "/api/repos")
      return route.fulfill({ json: { repositories: [] } });
    if (path === base) {
      if (state.revoked)
        return route.fulfill({
          status: 404,
          json: { error: { message: "Organization not found" } },
        });
      return route.fulfill({
        json: {
          organization: {
            id,
            name: "Crab engineering",
            description: "Repository collaboration",
            version: 1,
            created_at_ms: 1,
            updated_at_ms: 1,
          },
          actor: alice,
          role: "owner",
        },
      });
    }
    if (path === `${base}/members`)
      return route.fulfill({
        json: { items: [], next: null, revision: 1, can_manage: true },
      });
    if (path === `${base}/teams/commands` || path === `${base}/teams/cancel`) {
      expect(request.headers()["x-csrf-token"]).toBe(local ? "" : "csrf");
      const body = request.postDataJSON() as Submission,
        cancel = path.endsWith("/cancel");
      state.writes.push({ body, cancel, path });
      let receipt = receipts.get(body.request_id);
      if (body.command.kind === "edit")
        expect(Object.keys(body.command).sort()).toEqual(
          [
            "kind",
            "team",
            "version",
            "name",
            "description",
            "visibility",
          ].sort(),
        );
      const command = body.command,
        target =
          command.kind === "create"
            ? body.request_id
            : command.kind === "policy"
              ? id
              : command.team;
      if (!receipt && cancel) receipt = { outcome: "cancelled", id: target };
      if (!receipt && state.denied)
        return route.fulfill({
          status: 403,
          json: { error: { message: "Team permission changed" } },
        });
      if (!receipt && state.conflict && command.kind === "edit") {
        const current = state.teams.find(
          (view) => view.team.id === command.team,
        )!;
        current.team = {
          ...current.team,
          name: "Saved elsewhere",
          description: "Another maintainer changed this",
          version: current.team.version + 1,
        };
        state.conflict = false;
        return route.fulfill({
          status: 409,
          headers: { "X-Crab-Write-Outcome": "rejected" },
          json: { error: { code: "conflict", message: "Team changed" } },
        });
      }
      if (!receipt) {
        if (command.kind === "create") {
          state.teams.push({
            team: {
              id: target,
              name: command.name,
              description: command.description,
              visibility: command.visibility,
              version: 1,
              created_at_ms: 1,
              updated_at_ms: 1,
            },
            role: "maintainer",
            can_manage: true,
          });
          state.members.set(target, [
            {
              actor: alice,
              role: "maintainer",
              version: 1,
              profile: profile(alice),
            },
          ]);
        } else if (command.kind === "edit") {
          const current = state.teams.find((view) => view.team.id === target)!;
          current.team = {
            ...current.team,
            ...command,
            version: current.team.version + 1,
          };
        } else if (command.kind === "member") {
          const members = state.members.get(target)!;
          const current = members.find(
            (member) => member.actor === command.member,
          );
          if (current) {
            current.role = command.role;
            current.version += 1;
          } else
            members.push({
              actor: command.member,
              role: command.role,
              version: 1,
              profile: profile(command.member),
            });
          state.teams.find((view) => view.team.id === target)!.team.version +=
            1;
        } else if (command.kind === "delete")
          state.teams = state.teams.filter((view) => view.team.id !== target);
        else
          state.policy = {
            member_creation: command.member_creation,
            version: state.policy.version + 1,
          };
        receipt = { outcome: "applied", id: target };
      }
      receipts.set(body.request_id, receipt);
      if (state.lost) {
        state.lost = false;
        return route.abort("failed");
      }
      return route.fulfill({ json: receipt });
    }
    if (state.revoked && path.startsWith(`${base}/teams`))
      return route.fulfill({
        status: 404,
        json: { error: { message: "Team not found" } },
      });
    if (path === `${base}/teams/policy`)
      return route.fulfill({ json: state.policy });
    if (path === `${base}/teams`)
      return route.fulfill({
        json: {
          items: state.teams
            .filter((view) =>
              view.team.name
                .toLowerCase()
                .includes((url.searchParams.get("search") ?? "").toLowerCase()),
            )
            .map((view) => ({
              ...view,
              members: state.members
                .get(view.team.id)!
                .filter((member) => member.role).length,
            })),
          next: null,
          revision: 1,
          can_create: true,
        },
      });
    const match = new RegExp(
      `^${base}/teams/([^/]+)(?:/(members|candidates))?$`,
    ).exec(path);
    if (match) {
      const team = match[1],
        view = state.teams.find((view) => view.team.id === team);
      if (!view)
        return route.fulfill({
          status: 404,
          json: { error: { message: "Team not found" } },
        });
      if (!match[2]) return route.fulfill({ json: view });
      const members = state.members.get(team)!;
      const items =
        match[2] === "members"
          ? members.filter((member) => member.role)
          : [alice, bob].map(
              (actor) =>
                members.find((member) => member.actor === actor) ?? {
                  actor,
                  role: null,
                  version: 0,
                  profile: profile(actor),
                },
            );
      return route.fulfill({
        json: { items, next: null, revision: 1, can_manage: true },
      });
    }
    return route.fulfill({
      status: 404,
      json: { error: { message: "Missing fixture route" } },
    });
  });
  return state;
}
async function create(page: Page, name = "Platform") {
  await page.getByRole("button", { name: "New team", exact: true }).click();
  await page.getByLabel("Team name", { exact: true }).fill(name);
  await page
    .getByLabel("Description", { exact: true })
    .fill("Repository maintainers");
  await page.getByRole("button", { name: "Create team", exact: true }).click();
}
for (const mobile of [false, true])
  test(`native team controls support membership, conflict review and deletion ${mobile ? "on mobile" : "on desktop"}`, async ({
    page,
  }) => {
    if (mobile) await page.setViewportSize({ width: 390, height: 844 });
    const state = await fixture(page);
    await page.goto(`${root}?tab=teams&powerbar=1`);
    if (mobile) await selectDarkTheme(page);
    await create(page);
    await expect(
      page.getByRole("status", { name: "Team submission result" }),
    ).toContainText("Create team completed.");
    await page.getByRole("link", { name: "Open team", exact: true }).click();
    await expect(
      page.getByRole("heading", { name: "Team members", exact: true }),
    ).toBeVisible();
    await page
      .getByRole("button", { name: "Add a member", exact: true })
      .click();
    await page
      .getByRole("region", { name: "Add organization members" })
      .getByRole("button", { name: "Add member", exact: true })
      .click();
    await expect(
      page.getByRole("status", { name: "Team submission result" }),
    ).toContainText("Add team member completed.");
    await page
      .getByRole("combobox", { name: "Team role for Bob" })
      .selectOption("maintainer");
    await page
      .getByRole("listitem")
      .filter({ has: page.getByRole("link", { name: "Bob", exact: true }) })
      .filter({
        has: page.getByRole("button", { name: "Save role", exact: true }),
      })
      .getByRole("button", { name: "Save role", exact: true })
      .click();
    await expect(
      page.getByRole("status", { name: "Team submission result" }),
    ).toContainText("Update team member role completed.");
    await page
      .getByRole("link", { name: "Settings", exact: true })
      .last()
      .click();
    await page.getByLabel("Team name", { exact: true }).fill("My draft");
    state.conflict = true;
    await page
      .getByRole("button", { name: "Save team changes", exact: true })
      .click();
    await expect(
      page.getByText("Your draft is preserved.", { exact: false }),
    ).toBeVisible();
    await expect(page.getByLabel("Team name", { exact: true })).toHaveValue(
      "My draft",
    );
    await page
      .getByRole("button", { name: "Continue with my draft", exact: true })
      .click();
    await page
      .getByRole("button", { name: "Save team changes", exact: true })
      .click();
    await expect(
      page.getByRole("status", { name: "Team submission result" }),
    ).toContainText("Update team completed.");
    await expect(
      page.getByRole("heading", { name: "My draft", exact: true }),
    ).toBeVisible();
    await expectNoAccessibilityViolations(page);
    expect(
      await page.evaluate(
        () => document.documentElement.scrollWidth <= window.innerWidth,
      ),
    ).toBe(true);
    await page
      .getByRole("button", { name: "Delete team", exact: true })
      .click();
    await page
      .getByRole("button", { name: "Confirm delete team", exact: true })
      .click();
    await expect(
      page.getByRole("status", { name: "Team submission result" }),
    ).toContainText("Delete team completed.");
    await page.getByRole("link", { name: "All teams", exact: false }).click();
    await expect(
      page.getByText("No teams to show.", { exact: false }),
    ).toBeVisible();
    expect(state.teams).toEqual([]);
  });
test("lost acknowledgements retain the exact submission across navigation and access removal", async ({
  page,
}) => {
  const state = await fixture(page);
  await page.goto(`${root}?tab=teams`);
  state.lost = true;
  await create(page, "Secret draft");
  await expect(
    page.getByText("Team submission needs confirmation", { exact: true }),
  ).toBeVisible();
  const original = state.writes[0].body;
  await page.getByRole("link", { name: "People", exact: true }).click();
  state.revoked = true;
  await page
    .getByRole("button", { name: "Refresh organization", exact: true })
    .click();
  await expect(
    page.getByText("Organization not found", { exact: true }),
  ).toBeVisible();
  await page
    .getByRole("link", { name: "Crab repositories", exact: true })
    .first()
    .click();
  await page
    .getByRole("button", { name: "Cancel or recover submission", exact: true })
    .click();
  // Resolve on another route without unmounting the app-level submission owner.
  await expect(
    page.getByRole("status", { name: "Team submission result" }),
  ).toContainText("Create team completed.");
  expect(state.writes.at(-1)).toEqual({
    body: original,
    cancel: true,
    path: `/api/organizations/${id}/teams/cancel`,
  });
  expect(state.teams).toHaveLength(1);
});
test("unproven denials retain the draft until atomic cancellation confirms it", async ({
  page,
}) => {
  const state = await fixture(page);
  await page.goto(`${root}?tab=teams`);
  state.denied = true;
  await create(page, "Cancelled secret");
  await expect(
    page.getByText("Team submission needs confirmation", { exact: true }),
  ).toBeVisible();
  await expect(page.getByLabel("Team name", { exact: true })).toBeDisabled();
  await page
    .getByRole("button", { name: "Cancel or recover submission", exact: true })
    .click();
  await expect(
    page.getByRole("status", { name: "Team submission result" }),
  ).toContainText("Submission cancelled.");
  await expect(page.getByLabel("Team name", { exact: true })).toBeEnabled();
  expect(state.writes[1].body).toEqual(state.writes[0].body);
  expect(state.teams).toHaveLength(0);
});

test("a different signed-in account cannot inspect or retarget an uncertain submission", async ({
  page,
}) => {
  const state = await fixture(page);
  await page.goto(`${root}?tab=teams`);
  state.lost = true;
  await create(page, "Private original draft");
  await expect(
    page.getByText("Team submission needs confirmation", { exact: true }),
  ).toBeVisible();
  state.account = "bob";
  await page.evaluate(() =>
    window.dispatchEvent(new Event("crab-session-expired")),
  );
  await expect(
    page.getByText(
      "Sign in with the account that started the team submission",
      { exact: false },
    ),
  ).toBeVisible();
  await expect(
    page.getByRole("button", {
      name: "Retry original submission",
      exact: true,
    }),
  ).not.toBeVisible();
  await expect(
    page.getByText("Original submission", { exact: true }),
  ).not.toBeVisible();
  expect(state.writes).toHaveLength(1);
  state.account = "alice";
  await page.evaluate(() =>
    window.dispatchEvent(new Event("crab-session-expired")),
  );
  await page
    .getByRole("button", { name: "Retry original submission", exact: true })
    .click();
  await expect(
    page.getByRole("status", { name: "Team submission result" }),
  ).toContainText("Create team completed.");
  expect(state.writes[1].body).toEqual(state.writes[0].body);
});

test("local operator identity can create teams and search the native listing", async ({
  page,
}) => {
  await fixture(page, true);
  await page.goto(`${root}?tab=teams`);
  await create(page, "Platform team");
  await expect(
    page.getByRole("status", { name: "Team submission result" }),
  ).toContainText("Create team completed.");
  await page.getByLabel("Find a team", { exact: true }).fill("Security");
  await page.getByRole("button", { name: "Search", exact: true }).click();
  await expect(
    page.getByText("No teams match your search.", { exact: true }),
  ).toBeVisible();
  await page.getByLabel("Find a team", { exact: true }).fill("PLATFORM");
  await page.getByRole("button", { name: "Search", exact: true }).click();
  await expect(
    page.getByRole("link", { name: "Platform team", exact: true }),
  ).toBeVisible();
});
