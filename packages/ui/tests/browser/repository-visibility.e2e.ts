import { expect, test, type Page } from "@playwright/test";
import { expectNoAccessibilityViolations } from "./accessibility";
import { conversationPage } from "./conversation";
import { emptyReactions } from "./fixtures";

test("an owner confirms publication and a signed-out visitor can find the code", async ({
  page,
  browser,
  baseURL,
}) => {
  const id = "01900000-0000-7000-8000-000000000021";
  const organization = "01900000-0000-7000-8000-000000000010";
  const settings = "/org-crab/library/settings";
  const endpoint = `/api/repositories/${id}/visibility`;
  let setting: { version: number; value: "private" | "public" } = {
    version: 0,
    value: "private",
  };
  const discussion = {
    number: 1,
    author: { name: "Alice", profile: null },
    state: "open",
    created_at: 1_700_000_000_000,
    updated_at: 1_700_000_000_000,
    labels: [],
    assignees: [],
    reactions: emptyReactions(),
    version: 1,
    can_edit: false,
    can_assign: false,
    can_label: false,
  };
  const issue = {
    ...discussion,
    kind: "issue",
    title: "Public issue",
    body: "Issue description",
  };
  const pull = {
    ...discussion,
    kind: "pull",
    title: "Public pull request",
    body: "Pull request description",
    base_ref: "refs/heads/main",
    head_ref: "refs/heads/feature",
    base_oid: "a".repeat(40),
    head_oid: "b".repeat(40),
    branches_available: true,
    rollup: null,
  };
  let guestPullReads = 0;
  const privatePullReads: string[] = [];
  async function install(target: Page, admin: boolean) {
    await target.route("**/api/**", async (route) => {
      const request = route.request();
      const { pathname } = new URL(request.url());
      if (!admin && pathname.endsWith("/pulls/1")) guestPullReads += 1;
      if (
        !admin &&
        /\/pulls\/1\/(reviewers|merge-candidate|auto-merge|pending-review)$|\/check-runs(?:\/\d+)?$/.test(
          pathname,
        )
      )
        privatePullReads.push(pathname);
      if (pathname === "/api/session")
        return route.fulfill({
          json: {
            authenticated: admin,
            mode: "oidc",
            ...(admin
              ? {
                  csrf: "csrf",
                  user: { issuer: "test", subject: "alice", name: "Alice" },
                }
              : {}),
          },
        });
      if (pathname === "/api/repos")
        return route.fulfill({
          json: {
            repositories:
              admin || setting.value === "public"
                ? [
                    {
                      id,
                      owner: "org-crab",
                      name: "library",
                      organization_id: organization,
                      visibility: setting.value,
                      public_read_only: !admin,
                      can_admin: admin,
                      can_configure_visibility: admin,
                      archived: false,
                      archive_version: 0,
                    },
                  ]
                : [],
            next: null,
          },
        });
      if (pathname === endpoint) {
        expect(admin).toBe(true);
        if (request.method() === "GET")
          return route.fulfill({
            json: {
              setting,
              can_change: true,
              private_forks: 2,
            },
          });
        expect(request.headers()["x-csrf-token"]).toBe("csrf");
        expect(request.postDataJSON()).toEqual({
          version: 0,
          value: "public",
          confirmation: "org-crab/library",
        });
        setting = { version: 1, value: "public" };
        return route.fulfill({ json: setting });
      }
      if (pathname.endsWith("/refs"))
        return route.fulfill({
          json: { head: null, default_branch: "main", refs: [] },
        });
      if (pathname === "/api/repos/org-crab/library/properties") {
        expect(request.method()).toBe("GET");
        expect(admin).toBe(false);
        return route.fulfill({
          json: { properties: [{ name: "service_tier", value: "standard" }] },
        });
      }
      if (pathname.endsWith("/issues/pins"))
        return route.fulfill({ json: { items: [], can_manage: false } });
      if (pathname.endsWith("/issues") || pathname.endsWith("/pulls"))
        return route.fulfill({
          json: {
            items: [pathname.endsWith("/issues") ? issue : pull],
            next: null,
            counts: { open: 1, closed: 0, merged: 0 },
          },
        });
      if (pathname.endsWith("/issues/1")) return route.fulfill({ json: issue });
      if (pathname.endsWith("/pulls/1")) return route.fulfill({ json: pull });
      if (pathname.endsWith("/conversation"))
        return conversationPage(route, {
          comments: [{ ...discussion, body: "A published reply" }],
        });
      if (pathname.endsWith("/settings/merge-methods"))
        return route.fulfill({
          json: { version: 0, enabled: ["merge_commit"] },
        });
      return route.fulfill({ status: 404 });
    });
  }

  await install(page, true);
  await page.goto(settings);
  await page.getByRole("button", { name: "Change visibility" }).click();
  const dialog = page.getByRole("dialog");
  await expect(dialog).toContainText(
    "Existing private forks will become separate private repositories.",
  );
  const publish = dialog.getByRole("button", {
    name: "Make repository public",
  });
  await expect(publish).toBeDisabled();
  await dialog
    .getByLabel("Type org-crab/library to confirm")
    .fill("org-crab/library");
  await publish.click();
  await expect(page.getByText("This repository is public.")).toBeVisible();

  const guestContext = await browser.newContext({ baseURL });
  const guest = await guestContext.newPage();
  try {
    await install(guest, false);
    await guest.clock.install();
    await guest.goto("/");
    await guest
      .getByRole("link", { name: "Browse public repositories" })
      .click();
    await expect(
      guest.getByRole("heading", { name: "Public repositories" }),
    ).toBeVisible();
    await guest.getByRole("link", { name: "library", exact: true }).click();
    await expect(
      guest.getByRole("navigation", { name: "Repository", exact: true }),
    ).toBeVisible();
    await expect(
      guest.getByRole("link", { name: "Sign in", exact: true }),
    ).toBeVisible();
    await expect(
      guest.getByRole("link", { name: "Settings", exact: true }),
    ).toHaveCount(0);
    await expectNoAccessibilityViolations(guest);
    const tabs = guest.getByRole("navigation", {
      name: "Repository",
      exact: true,
    });
    await expect(
      tabs.getByRole("link", { name: "Properties", exact: true }),
    ).toHaveCount(0);
    await guest.goto("/org-crab/library/settings/properties");
    await expect(
      guest.getByRole("heading", { name: "Repository properties" }),
    ).toHaveCount(0);
    await guest.goto("/org-crab/library");
    await tabs.getByRole("link", { name: "Issues", exact: true }).click();
    await expect(guest.getByRole("button", { name: "New issue" })).toHaveCount(
      0,
    );
    await guest
      .getByRole("link", { name: "Public issue", exact: true })
      .click();
    await expect(guest.getByText("Issue description")).toBeVisible();
    await expect(guest.getByText("A published reply")).toBeVisible();
    await expect(guest.getByRole("button", { name: "Edit issue" })).toHaveCount(
      0,
    );
    await tabs
      .getByRole("link", { name: "Pull requests", exact: true })
      .click();
    await guest
      .getByRole("link", { name: "Public pull request", exact: true })
      .click();
    await expect(guest.getByText("Pull request description")).toBeVisible();
    await expect(
      guest.getByRole("link", { name: "Checks", exact: true }),
    ).toHaveCount(0);
    await expect(
      guest.getByRole("button", { name: "Comment", exact: true }),
    ).toHaveCount(0);
    await guest.clock.fastForward(30_000);
    expect(guestPullReads).toBe(1);
    await guest.goto("/org-crab/library/pull/1/checks");
    await expect(guest.getByText("Pull request description")).toBeVisible();
    expect(privatePullReads).toEqual([]);
  } finally {
    await guestContext.close();
  }
});
