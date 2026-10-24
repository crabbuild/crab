import { expect, test, type Page } from "@playwright/test";
import {
  expectNoAccessibilityViolations,
  selectDarkTheme,
  contrastRatio,
} from "./accessibility";

const id = "01900000-0000-7000-8000-000000000001";
interface Profile {
  id: string;
  handle: string;
  name: string;
  version: number;
  created_at_ms: number;
  updated_at_ms: number;
}
async function fixture(page: Page) {
  const state: {
    profile: Profile | null;
    ready: boolean;
    loseReply: boolean;
    writes: unknown[];
    reads: string[];
  } = { profile: null, ready: true, loseReply: false, writes: [], reads: [] };
  await page.route("**/api/**", async (route) => {
    const request = route.request(),
      path = new URL(request.url()).pathname;
    if (path === "/api/session")
      return route.fulfill({
        json: {
          authenticated: true,
          mode: "oidc",
          csrf: "csrf",
          user: { issuer: "fixture", subject: "alice", name: "Alice" },
        },
      });
    if (path === "/api/repos") {
      state.reads.push(request.url());
      return route.fulfill({ json: { repositories: [] } });
    }
    if (path === `/api/actors/${id}`)
      return route.fulfill({ json: state.profile });
    expect(path).toBe("/api/profile");
    if (!state.ready)
      return route.fulfill({
        status: 503,
        json: {
          error: {
            code: "directory_unavailable",
            message:
              "Profiles are not ready. Contact your server administrator.",
          },
        },
      });
    if (request.method() === "GET")
      return route.fulfill({
        headers: { "Server-Timing": "directory;dur=2" },
        json: { profile: state.profile },
      });
    expect(request.headers()["x-csrf-token"]).toBe("csrf");
    const input = request.postDataJSON() as {
      request_id?: string;
      handle: string;
      name: string;
      version?: number;
    };
    state.writes.push(input);
    if (input.handle === "taken")
      return route.fulfill({
        status: 409,
        json: {
          error: {
            code: "handle_taken",
            message: "This username is already taken",
          },
        },
      });
    if (
      request.method() === "PATCH" &&
      input.version !== state.profile?.version
    )
      return route.fulfill({
        status: 409,
        json: {
          error: {
            code: "conflict",
            message: "This content changed; reload before saving your draft",
          },
        },
      });
    state.profile = {
      id,
      handle: input.handle,
      name: input.name,
      version:
        request.method() === "POST" ? 1 : (state.profile?.version ?? 0) + 1,
      created_at_ms: 1700000000000,
      updated_at_ms: 1700000000000,
    };
    if (state.loseReply) {
      state.loseReply = false;
      return route.abort("failed");
    }
    return route.fulfill({
      status: request.method() === "POST" ? 201 : 200,
      json: state.profile,
    });
  });
  return state;
}

test("mobile header and footer stay stable while the signed-in session loads", async ({
  page,
}) => {
  await fixture(page);
  let release = () => {};
  const pending = new Promise<void>((resolve) => {
    release = resolve;
  });
  await page.route("**/api/session", async (route) => {
    await pending;
    return route.fulfill({
      json: {
        authenticated: true,
        mode: "oidc",
        csrf: "csrf",
        user: { issuer: "fixture", subject: "alice", name: "Alice" },
      },
    });
  });
  try {
    await page.setViewportSize({ width: 390, height: 844 });
    await page.goto("/-/profile");
    await expect(
      page.getByText("Checking your session…", { exact: true }),
    ).toBeVisible();
    const header = page.locator(".global-header");
    const before = (await header.boundingBox())!;
    release();
    await expect(
      page.getByRole("button", { name: "Open user menu", exact: true }),
    ).toBeVisible();
    await expect(page.getByLabel("Username", { exact: true })).toBeVisible();
    await expect
      .poll(async () => (await header.boundingBox())!.height)
      .toBe(before.height);
    expect((await page.locator("main").boundingBox())!.y).toBe(
      before.y + before.height,
    );
    await expect
      .poll(() =>
        page
          .locator(".site-footer")
          .evaluate((node) => node.getBoundingClientRect().bottom),
      )
      .toBe(844);
  } finally {
    release();
  }
});

test("shared form tokens align controls, preserve contrast in both themes and follow user font scaling", async ({
  page,
}) => {
  await fixture(page);
  await page.setViewportSize({ width: 1280, height: 900 });
  await page.goto("/-/profile");
  const username = page.getByLabel("Username", { exact: true });
  const name = page.getByLabel("Display name", { exact: true });
  const submit = page.getByRole("button", {
    name: "Create profile",
    exact: true,
  });
  await expect(username).toBeVisible();
  await expect(
    page.getByRole("heading", { name: "Public profile", exact: true }),
  ).toHaveCSS("margin-bottom", "12px");
  await expect(page.locator(".account-settings")).toHaveCSS(
    "padding-left",
    "24px",
  );

  for (const theme of ["Light", "Dark"]) {
    await page.getByRole("button", { name: theme, exact: true }).click();
    const background = await page
      .locator("body")
      .evaluate((node) => getComputedStyle(node).backgroundColor);
    await expect(page.locator('meta[name="theme-color"]')).toHaveAttribute(
      "content",
      background,
    );
    const control = await username.evaluate((node) => ({
      border: getComputedStyle(node).borderTopColor,
      background: getComputedStyle(document.body).backgroundColor,
      font: getComputedStyle(node).fontSize,
      radius: getComputedStyle(node).borderTopLeftRadius,
    }));
    expect(
      contrastRatio(control.border, control.background),
    ).toBeGreaterThanOrEqual(3);
    expect(control.font).toBe("14px");
    expect(control.radius).toBe("8px");
    await expectNoAccessibilityViolations(page);
  }
  const bounds = await Promise.all([
    username.boundingBox(),
    name.boundingBox(),
    submit.boundingBox(),
  ]);
  expect(new Set(bounds.map((rect) => rect!.x)).size).toBe(1);
  expect(new Set(bounds.map((rect) => rect!.height)).size).toBe(1);

  await page.evaluate(() => {
    document.documentElement.style.fontSize = "20px";
  });
  await expect(username).toHaveCSS("font-size", "17.5px");
  await page.evaluate(() => {
    document.documentElement.style.removeProperty("font-size");
  });
  await page.setViewportSize({ width: 390, height: 844 });
  await expect(page.locator(".account-settings")).toHaveCSS(
    "padding-left",
    "16px",
  );
  await expect(username).toHaveCSS("font-size", "16px");
  expect(await page.evaluate(() => document.documentElement.scrollWidth)).toBe(
    390,
  );
  await expectNoAccessibilityViolations(page);
});

test("profile enrollment retains a lost reply, reloads native identity and opens its stable profile route", async ({
  page,
}) => {
  const state = await fixture(page);
  state.loseReply = true;
  await page.goto("/?powerbar=1");
  await page
    .getByRole("button", { name: "Open user menu", exact: true })
    .click();
  await page
    .getByRole("menuitem", { name: "Your profile", exact: true })
    .click();
  await expect(
    page.getByRole("heading", { name: "Public profile", exact: true }),
  ).toBeVisible();
  await expect(
    page.getByRole("menuitem", { name: "Your profile", exact: true }),
  ).toHaveCount(0);
  expect(
    state.reads.every((url) => !new URL(url).searchParams.has("owner")),
  ).toBe(true);
  await page.getByLabel("Username", { exact: true }).fill("alice");
  await page.getByLabel("Display name", { exact: true }).fill("阿丽絲 🦀");
  await expectNoAccessibilityViolations(page);
  await page
    .getByRole("button", { name: "Create profile", exact: true })
    .click();
  await expect(page.getByRole("alert")).toContainText("response was lost");
  await expect(page.getByLabel("Username", { exact: true })).toHaveValue(
    "alice",
  );
  await page
    .getByRole("button", { name: "Create profile", exact: true })
    .click();
  await expect(page.getByRole("status")).toHaveText("Profile created.");
  expect(state.writes[0]).toEqual(state.writes[1]);
  await page.reload();
  await expect(page.getByLabel("Display name", { exact: true })).toHaveValue(
    "阿丽絲 🦀",
  );
  await page.getByLabel("Username", { exact: true }).fill("alice-renamed");
  await page
    .getByRole("button", { name: "Update profile", exact: true })
    .click();
  await expect(page.getByRole("status")).toHaveText("Profile updated.");
  await selectDarkTheme(page);
  await page.setViewportSize({ width: 390, height: 844 });
  await expectNoAccessibilityViolations(page);
  await page.getByRole("link", { name: "View profile", exact: true }).click();
  await expect(page).toHaveURL(new RegExp(`/-/users/${id}\\?powerbar=1$`));
  await expect(
    page.getByRole("heading", { name: "阿丽絲 🦀", exact: true }),
  ).toBeVisible();
  await expect(page.getByText("@alice-renamed", { exact: true })).toBeVisible();
  expect(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= innerWidth,
    ),
  ).toBe(true);
  await expectNoAccessibilityViolations(page);
});

test("profile conflicts keep the draft until a saved version is explicitly chosen", async ({
  page,
}) => {
  const state = await fixture(page);
  state.profile = {
    id,
    handle: "alice",
    name: "Alice",
    version: 1,
    created_at_ms: 1700000000000,
    updated_at_ms: 1700000000000,
  };
  await page.goto("/-/profile");
  await page.getByLabel("Display name", { exact: true }).fill("My draft");
  state.profile = { ...state.profile, name: "Newer saved name", version: 2 };
  await page
    .getByRole("button", { name: "Update profile", exact: true })
    .click();
  const conflict = page.getByRole("region", {
    name: "Review saved profile",
    exact: true,
  });
  await expect(conflict).toContainText("Newer saved name");
  await expect(page.getByLabel("Display name", { exact: true })).toHaveValue(
    "My draft",
  );
  await conflict
    .getByRole("button", { name: "Keep my draft", exact: true })
    .click();
  await page
    .getByRole("button", { name: "Update profile", exact: true })
    .click();
  await expect(page.getByRole("status")).toHaveText("Profile updated.");
  expect(state.writes.at(-1)).toMatchObject({ version: 2, name: "My draft" });
  await page.getByLabel("Username", { exact: true }).fill("taken");
  await page
    .getByRole("button", { name: "Update profile", exact: true })
    .click();
  await expect(page.getByRole("alert")).toContainText(
    "username is already taken",
  );
  expect(state.profile.name).toBe("My draft");
  await expectNoAccessibilityViolations(page);
});

test("profile preparation errors can be retried without claiming an enrollment", async ({
  page,
}) => {
  const state = await fixture(page);
  state.ready = false;
  await page.goto("/-/profile");
  await expect(page.getByRole("alert")).toContainText("Profiles are not ready");
  await expect(
    page.getByRole("button", { name: "Create profile", exact: true }),
  ).toHaveCount(0);
  state.ready = true;
  await page.getByRole("button", { name: "Try again", exact: true }).click();
  await expect(
    page.getByRole("button", { name: "Create profile", exact: true }),
  ).toBeVisible();
  expect(state.writes).toEqual([]);
});

test("account menu keeps sign-out failures recoverable and clears the session after success", async ({
  page,
}) => {
  await fixture(page);
  let authenticated = true;
  let fail = true;
  await page.route("**/api/session", (route) =>
    route.fulfill({
      json: {
        authenticated,
        mode: "oidc",
        csrf: authenticated ? "csrf" : null,
        user: authenticated
          ? { issuer: "fixture", subject: "alice", name: "Alice" }
          : null,
      },
    }),
  );
  await page.route("**/auth/logout", (route) => {
    expect(route.request().method()).toBe("POST");
    expect(route.request().headers()["x-csrf-token"]).toBe("csrf");
    if (fail) return route.fulfill({ status: 403 });
    authenticated = false;
    return route.fulfill({ status: 204 });
  });
  await page.goto("/");
  const account = page.getByRole("button", {
    name: "Open user menu",
    exact: true,
  });
  await account.focus();
  await account.press("Enter");
  await expect(
    page.getByRole("menuitem", { name: "Your profile", exact: true }),
  ).toBeFocused();
  await expect(page.getByText("Signed in as", { exact: true })).toBeVisible();
  await page.getByRole("menuitem", { name: "Sign out", exact: true }).click();
  await expect(page.getByRole("alert")).toContainText("Sign-out failed");
  await expect(account).toBeEnabled();
  fail = false;
  await page.setViewportSize({ width: 390, height: 844 });
  await selectDarkTheme(page);
  await account.click();
  await expectNoAccessibilityViolations(page);
  await page.getByRole("menuitem", { name: "Sign out", exact: true }).click();
  await expect(
    page.getByRole("button", { name: "Continue to sign in", exact: true }),
  ).toBeVisible();
  await expect(account).toHaveCount(0);
});

test("local sessions retain profile access without presenting SSO sign-out or Git tokens", async ({
  page,
}) => {
  await fixture(page);
  await page.route("**/api/session", (route) =>
    route.fulfill({
      json: { authenticated: true, mode: "local", csrf: null, user: null },
    }),
  );
  await page.goto("/");
  await page
    .getByRole("button", { name: "Open user menu", exact: true })
    .click();
  await expect(page.getByText("Local session", { exact: true })).toBeVisible();
  await expect(
    page.getByRole("menuitem", { name: "Sign out", exact: true }),
  ).toHaveCount(0);
  await expect(page.getByText("Git access", { exact: true })).toHaveCount(0);
  await page
    .getByRole("menuitem", { name: "Your profile", exact: true })
    .click();
  await expect(
    page.getByRole("heading", { name: "Public profile", exact: true }),
  ).toBeVisible();
});
