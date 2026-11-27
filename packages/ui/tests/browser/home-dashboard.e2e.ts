import { expect, test } from "@playwright/test";
import {
  expectNoAccessibilityViolations,
  selectDarkTheme,
} from "./accessibility";

test("home dashboard renders GitHub-style layout, filters, feed, drawer, and command search", async ({
  page,
}) => {
  const orgId = "01900000-0000-7000-8000-000000000010";
  const repoId = "00000000-0000-4000-8000-00000000000a";
  const now = Date.UTC(2026, 10, 25, 18, 0, 0);

  await page.route("**/api/**", (route) => {
    const { pathname, searchParams } = new URL(route.request().url());
    if (pathname === "/api/session") {
      return route.fulfill({
        json: {
          authenticated: true,
          mode: "oidc",
          csrf: "fixture-csrf",
          can_sign_up: false,
          user: {
            issuer: "https://example.test",
            subject: "forhappy",
            name: "Haiping Fu",
          },
        },
      });
    }
    if (pathname === "/api/profile") {
      return route.fulfill({
        json: {
          profile: {
            id: "01900000-0000-7000-8000-000000000001",
            handle: "forhappy",
            name: "Haiping Fu",
            version: 1,
            created_at_ms: now - 86_400_000,
            updated_at_ms: now - 3_600_000,
          },
        },
      });
    }
    if (pathname === "/api/notifications/unread") {
      return route.fulfill({ json: { unread: true } });
    }
    if (pathname === "/api/notifications") {
      return route.fulfill({
        json: {
          items: [
            {
              id: "notice-1",
              repository: { id: repoId, owner: "crabbuild", name: "crab" },
              title: "Review requested on cellar PR #192",
              href: "/crabbuild/crab/pull/192",
              sender: "Haoxincode",
              reason: "review_requested",
              read: false,
              saved: false,
              done: false,
              updated: {
                id: "notice-1-pos",
                created_at_ms: now - 1_200_000,
              },
            },
          ],
          next: null,
        },
      });
    }
    if (pathname === "/api/organizations") {
      return route.fulfill({
        json: {
          items: [
            {
              kind: "member",
              actor: "actor-1",
              role: "owner",
              can_create: true,
              can_create_private: true,
              can_create_public: true,
              organization: {
                id: orgId,
                handle: "crabbuild",
                name: "crabbuild",
                description: "Crab engineering",
                version: 1,
                created_at_ms: now - 86_400_000,
                updated_at_ms: now - 86_400_000,
              },
            },
          ],
          next: null,
        },
      });
    }
    if (pathname === "/api/repos") {
      const repos = [
        "compass",
        "crab",
        "prolly",
        "fern",
        "cellar",
        "crab-enterprise",
        "trail",
        "canopy",
        "cellule",
        "silo",
        "extra-repo",
      ].map((name, index) => ({
        id: `00000000-0000-4000-8000-${String(index + 1).padStart(12, "0")}`,
        owner: "crabbuild",
        name,
        description: `${name} repository`,
        visibility: "public",
        archived: false,
        fork: false,
        access: "write",
        can_admin: true,
        archive_version: 0,
        protection_version: 0,
        protected_branches: [],
      }));
      return route.fulfill({ json: { repositories: repos, next: null } });
    }
    if (pathname === "/api/discussion-search") {
      const kind = searchParams.get("kind");
      if (kind === "pull") {
        return route.fulfill({
          json: {
            items: [
              {
                repository: { id: repoId, owner: "crabbuild", name: "cellar" },
                kind: "pull",
                number: 192,
                title: "Release SQL query slots after physical completion",
                state: "open",
                draft: false,
                created_at: now - 7_200_000,
                updated_at: now - 1_320_000,
                sort_value: now - 1_320_000,
                comments: 13,
                author: {
                  name: "Haiping Fu",
                  profile: {
                    id: "01900000-0000-7000-8000-000000000001",
                    handle: "forhappy",
                    name: "Haiping Fu",
                  },
                },
              },
            ],
            next: null,
          },
        });
      }
      return route.fulfill({
        json: {
          items: [
            {
              repository: { id: repoId, owner: "crabbuild", name: "crab" },
              kind: "issue",
              number: 471,
              title: "Feature request: Official MCP server for Crab",
              state: "open",
              draft: false,
              created_at: now - 86_400_000,
              updated_at: now - 3_600_000,
              sort_value: now - 3_600_000,
              comments: 1,
              author: {
                name: "Haoxincode",
                profile: null,
              },
            },
          ],
          next: null,
        },
      });
    }
    if (pathname === "/api/personal-work") {
      return route.fulfill({
        json: {
          items: [
            {
              repository: { id: repoId, owner: "crabbuild", name: "crab" },
              kind: searchParams.get("kind") ?? "pull",
              number: 686,
              title: "chore: prepare v2.0.0-alpha.2",
              state: "open",
              draft: false,
              created_at: now - 172_800_000,
              updated_at: now - 172_800_000,
              sort_value: now - 172_800_000,
              comments: 0,
              author: {
                name: "forhappy",
                profile: null,
              },
            },
          ],
          next: null,
        },
      });
    }
    return route.fulfill({ status: 404, json: {} });
  });

  await page.setViewportSize({ width: 1440, height: 900 });
  await page.goto("/");

  // Header & Greeting
  await expect(page).toHaveTitle("Dashboard · Crab");
  await expect(page.getByRole("heading", { name: /forhappy!/ })).toBeVisible();

  // Simplified GitHub-style toolbar: no Git access, code theme, or standalone theme toggles
  const header = page.locator(".global-header");
  await expect(header.getByText("Git access", { exact: true })).toHaveCount(0);
  await expect(header.locator('[aria-label^="Code theme:"]')).toHaveCount(0);
  await expect(header.locator('[aria-label="Dark"]')).toHaveCount(0);
  const searchButton = header.getByRole("button", {
    name: "Search Crab",
    exact: true,
  });
  await expect(searchButton).toBeVisible();
  expect(
    await searchButton.evaluate((el) => ({
      justifyContent: getComputedStyle(el).justifyContent,
      textAlign: getComputedStyle(el).textAlign,
      width: el.getBoundingClientRect().width,
    })),
  ).toMatchObject({
    justifyContent: "flex-start",
    textAlign: "left",
  });
  expect((await searchButton.boundingBox())!.width).toBeLessThanOrEqual(220);

  // User profile button renders a full-bleed large SVG avatar and neutral dropdown links
  const userMenuButton = header.getByRole("button", {
    name: "Open user menu",
    exact: true,
  });
  const avatarBox = await userMenuButton.locator("svg").boundingBox();
  expect(avatarBox).not.toBeNull();
  expect(avatarBox!.width).toBeGreaterThanOrEqual(34);
  expect(avatarBox!.height).toBeGreaterThanOrEqual(34);

  await userMenuButton.click();
  const profileItem = page.getByRole("menuitem", {
    name: "Your profile",
    exact: true,
  });
  await expect(profileItem).toBeVisible();
  const bodyColor = await page
    .locator("body")
    .evaluate((el) => getComputedStyle(el).color);
  await expect(profileItem).toHaveCSS("color", bodyColor);
  await expectNoAccessibilityViolations(page);
  await page.keyboard.press("Escape");

  // Workspace search composer keeps a single outer focus boundary with no inner box
  const composerInput = page.getByLabel("Workspace search query");
  await composerInput.focus();
  expect(
    await composerInput.evaluate((el) => {
      const style = getComputedStyle(el);
      return {
        borderWidth: style.borderTopWidth,
        boxShadow: style.boxShadow,
        outlineStyle: style.outlineStyle,
      };
    }),
  ).toEqual({
    borderWidth: "0px",
    boxShadow: "none",
    outlineStyle: "none",
  });

  // Left sidebar: top repositories, Show more, and live filter
  const topRepos = page.getByRole("list", { name: "Top repositories" });
  await expect(topRepos.getByRole("listitem")).toHaveCount(10);
  await page.getByRole("button", { name: "Show more" }).click();
  await expect(topRepos.getByRole("listitem")).toHaveCount(11);
  await page.getByRole("button", { name: "Filter top repositories" }).click();
  await page.getByRole("searchbox", { name: "Find a repository" }).fill("silo");
  await expect(topRepos.getByRole("listitem")).toHaveCount(1);
  await expect(topRepos.getByText("crabbuild/silo")).toBeVisible();

  // Pull requests & Issues cards with author and comment count
  await expect(
    page.getByRole("link", {
      name: "Release SQL query slots after physical completion",
    }),
  ).toBeVisible();
  await expect(page.getByLabel("13 comments")).toBeVisible();
  await expect(
    page.getByRole("link", {
      name: "Feature request: Official MCP server for Crab",
    }),
  ).toBeVisible();

  // Filter Pull requests card to Created by me
  await page.getByRole("button", { name: "Filter pull requests" }).click();
  await page.getByRole("menuitem", { name: "Created by me" }).click();
  await expect(
    page.getByRole("link", { name: "chore: prepare v2.0.0-alpha.2" }),
  ).toBeVisible();

  // Right rail updates
  await expect(
    page.getByRole("heading", { name: "Latest updates" }),
  ).toBeVisible();
  await expect(
    page.getByRole("link", { name: "View notifications" }),
  ).toBeVisible();

  // Pin tip dismissal & Feed switch
  await page.getByRole("button", { name: "Got it" }).click();
  await expect(page.getByText("Pin your homepage")).toHaveCount(0);
  await page.getByRole("button", { name: "Feed", exact: true }).click();
  await expect(page).toHaveURL(/tab=feed/);
  await expect(page.getByRole("heading", { name: "Feed" })).toBeVisible();
  await page.getByRole("button", { name: "Home", exact: true }).click();

  // Global navigation drawer & '/' command palette
  await page.getByRole("button", { name: "Open navigation menu" }).click();
  await expect(
    page.getByRole("dialog", { name: "Global navigation" }),
  ).toBeVisible();
  await page.keyboard.press("Escape");
  await expect(
    page.getByRole("dialog", { name: "Global navigation" }),
  ).toHaveCount(0);

  await page.keyboard.press("/");
  const paletteInput = page
    .getByRole("dialog")
    .getByRole("searchbox", { name: "Search Crab" });
  await expect(paletteInput).toBeVisible();
  await page.keyboard.press("Escape");
  await expect(paletteInput).toHaveCount(0);

  // Accessibility in light + dark mode and mobile viewport
  await expectNoAccessibilityViolations(page);
  await selectDarkTheme(page);
  await expectNoAccessibilityViolations(page);
  await page.setViewportSize({ width: 390, height: 844 });
  expect(await page.evaluate(() => document.documentElement.scrollWidth)).toBe(
    390,
  );
});
