import { expect, test } from "@playwright/test";
import {
  expectNoAccessibilityViolations,
  selectDarkTheme,
} from "./accessibility";

test("unified GitHub-style settings lets users configure profile, account, appearance, notifications, git access, and saved replies", async ({
  page,
}) => {
  const orgId = "01900000-0000-7000-8000-000000000010";
  const now = Date.UTC(2026, 10, 25, 18, 0, 0);
  let savedAvatar: {
    variant: string;
    seed: string;
    colors: string[];
  } | null = null;

  await page.route("**/api/**", (route) => {
    const request = route.request();
    const { pathname } = new URL(request.url());
    if (pathname === "/api/session") {
      return route.fulfill({
        json: {
          authenticated: true,
          mode: "oidc",
          csrf: "fixture-csrf",
          can_sign_up: false,
          ...(savedAvatar ? { avatar: savedAvatar } : {}),
          user: {
            issuer: "https://accounts.example",
            subject: "forhappy",
            name: "Haiping Fu",
          },
        },
      });
    }
    if (pathname === "/api/notifications/unread") {
      return route.fulfill({ json: { unread: false } });
    }
    if (pathname === "/api/profile/avatar") {
      if (request.method() === "PUT") {
        expect(request.headers()["x-csrf-token"]).toBe("fixture-csrf");
        const body = request.postDataJSON() as { avatar: typeof savedAvatar };
        savedAvatar = body.avatar;
        return route.fulfill({ json: { avatar: savedAvatar } });
      }
      return route.fulfill({ json: { avatar: savedAvatar } });
    }
    if (pathname === "/api/profile") {
      return route.fulfill({
        json: {
          ...(savedAvatar ? { avatar: savedAvatar } : {}),
          profile: {
            id: "01900000-0000-7000-8000-000000000001",
            handle: "forhappy",
            name: "Haiping Fu",
            version: 1,
            created_at_ms: now - 86_400_000,
            updated_at_ms: now - 3_600_000,
            ...(savedAvatar ? { avatar: savedAvatar } : {}),
          },
        },
      });
    }
    if (pathname === "/api/notifications/email") {
      return route.fulfill({
        json: {
          configured: true,
          address: "forhappy@crab.build",
          enabled: true,
          version: 1,
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
    if (pathname === "/api/repos" || pathname === "/api/repos/personal") {
      return route.fulfill({
        json: {
          repositories: [
            {
              id: "00000000-0000-4000-8000-000000000001",
              owner: "forhappy",
              name: "crab",
              description: "Serverless Git",
              visibility: "public",
              archived: false,
              fork: false,
              access: "write",
              can_admin: true,
              archive_version: 0,
              protection_version: 0,
              protected_branches: [],
            },
          ],
          next: null,
        },
      });
    }
    if (pathname === "/api/profile/review-availability") {
      return route.fulfill({
        json: { version: 1, busy: false, expires_at_ms: null },
      });
    }
    if (pathname === "/api/profile/saved-replies") {
      return route.fulfill({ json: { items: [] } });
    }
    return route.fulfill({ status: 404, json: {} });
  });

  await page.setViewportSize({ width: 1440, height: 900 });
  await page.goto("/-/settings");

  await expect(page).toHaveTitle("Settings · Crab");
  await expect(
    page.getByRole("heading", { name: "Public profile", exact: true }),
  ).toBeVisible();
  await expect(
    page.getByRole("heading", { name: "Profile picture", exact: true }),
  ).toBeVisible();

  // Customize Boring Avatar via Edit dialog
  await page.getByRole("button", { name: "Edit", exact: true }).click();
  await expect(
    page.getByRole("heading", { name: "Customize profile picture" }),
  ).toBeVisible();
  await page.getByLabel("Avatar name", { exact: true }).fill("Maria Mitchell");
  await page.getByRole("radio", { name: "Marble", exact: true }).click();
  await page.getByRole("radio", { name: "Ocean", exact: true }).click();
  await expectNoAccessibilityViolations(page);
  await page.getByRole("button", { name: "Save avatar", exact: true }).click();
  await expect(page.getByRole("status")).toContainText(
    "Profile picture updated.",
  );
  expect(savedAvatar).toEqual({
    variant: "marble",
    seed: "Maria Mitchell",
    colors: ["#0A0310", "#49007E", "#FF005B", "#FF7D10", "#FFB238"],
  });

  // Switch settings context dropdown loads organizations lazily
  await page.getByRole("button", { name: "Switch settings context" }).click();
  await expect(page.getByRole("menuitem", { name: "crabbuild" })).toBeVisible();
  await page.keyboard.press("Escape");

  // Account section
  const nav = page.getByRole("complementary", { name: "Account settings" });
  await nav.getByRole("link", { name: "Account", exact: true }).click();
  await expect(page).toHaveURL(/\/-\/settings\/account$/);
  await expect(
    page.getByRole("heading", { name: "Account", exact: true }),
  ).toBeVisible();
  await page.getByRole("button", { name: "Activity feed" }).click();
  await expect(
    page.getByRole("button", { name: "Activity feed" }),
  ).toHaveAttribute("aria-pressed", "true");

  // Appearance section
  await nav.getByRole("link", { name: "Appearance", exact: true }).click();
  await expect(page).toHaveURL(/\/-\/settings\/appearance$/);
  await page.getByRole("radio", { name: /Dark default/ }).click();
  await expect(page.locator("html")).toHaveClass(/\bdark\b/);
  await expectNoAccessibilityViolations(page);

  // Notifications section
  await nav.getByRole("link", { name: "Notifications", exact: true }).click();
  await expect(page).toHaveURL(/\/-\/settings\/notifications$/);
  await expect(page.getByText("forhappy@crab.build")).toBeVisible();

  // Git access section
  await nav.getByRole("link", { name: "Git access", exact: true }).click();
  await expect(page).toHaveURL(/\/-\/settings\/git-access$/);
  await expect(
    page.getByRole("heading", { name: "Repository Git token" }),
  ).toBeVisible();

  // Organizations & Repositories sections
  await nav.getByRole("link", { name: "Organizations", exact: true }).click();
  await expect(page).toHaveURL(/\/-\/settings\/organizations$/);
  await expect(page.getByRole("link", { name: "crabbuild" })).toBeVisible();

  await nav.getByRole("link", { name: "Repositories", exact: true }).click();
  await expect(page).toHaveURL(/\/-\/settings\/repositories$/);
  await expect(
    page.getByRole("link", { name: "crab", exact: true }),
  ).toBeVisible();

  // Review availability & Saved replies remain wired
  await nav
    .getByRole("link", { name: "Review availability", exact: true })
    .click();
  await expect(page).toHaveURL(/\/-\/settings\/review-availability$/);
  await nav.getByRole("link", { name: "Saved replies", exact: true }).click();
  await expect(page).toHaveURL(/\/-\/settings\/saved-replies$/);

  await selectDarkTheme(page);
  await page.setViewportSize({ width: 390, height: 844 });
  await expectNoAccessibilityViolations(page);
  expect(await page.evaluate(() => document.documentElement.scrollWidth)).toBe(
    390,
  );
});
