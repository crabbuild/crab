import { expect, test } from "@playwright/test";
import {
  expectNoAccessibilityViolations,
  selectDarkTheme,
} from "./accessibility";

const main = "a".repeat(40);
const oldTag = "b".repeat(40);

test("publishes and browses GitHub-style releases and source tags", async ({
  page,
}) => {
  const releases = [
    {
      number: 1,
      tag_name: "v0.9.0",
      tag_oid: oldTag as string | null,
      target_oid: main,
      title: "Crab 0.9",
      body: "## Highlights\n\n- Browse code without cloning.",
      prerelease: true,
      draft: false,
      version: 1,
      author: "Alice",
      created_at: 1_700_000_000_000,
      published_at: 1_700_000_000_000 as number | null,
      updated_at: 1_700_000_000_000,
    },
  ];
  let publishedTag: { name: string; oid: string } | undefined;
  let submission: Record<string, unknown> | undefined;

  await page.route("**/api/**", async (route) => {
    const url = new URL(route.request().url());
    if (url.pathname === "/api/session")
      return route.fulfill({
        json: { authenticated: true, mode: "local", user: null, csrf: null },
      });
    if (url.pathname === "/api/repos")
      return route.fulfill({
        json: {
          repositories: [
            {
              owner: "team",
              name: "project",
              description: "A repository for our team.",
              access: "write",
              can_admin: true,
              archive_version: 0,
              archived: false,
              protection_version: 0,
              protected_branches: [],
            },
          ],
        },
      });
    if (url.pathname.endsWith("/refs"))
      return route.fulfill({
        json: {
          head: { name: "refs/heads/main", oid: main },
          unborn_head: null,
          refs: [
            { name: "refs/heads/main", oid: main },
            { name: "refs/tags/v0.9.0", oid: oldTag, peeled: main },
            ...(publishedTag
              ? [{ name: publishedTag.name, oid: publishedTag.oid }]
              : []),
          ],
          generation: publishedTag ? 2 : 1,
        },
      });
    if (
      url.pathname.endsWith("/releases") &&
      route.request().method() === "POST"
    ) {
      submission = route.request().postDataJSON() as Record<string, unknown>;
      expect(submission.request_id).toMatch(
        /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/,
      );
      expect(submission).toMatchObject({
        tag_name: "v1.0.0",
        target_oid: main,
        title: "Crab 1.0",
        body: "## Changes\n\nA stable release.",
        prerelease: true,
        draft: true,
      });
      const draft = Boolean(submission.draft);
      publishedTag = draft
        ? undefined
        : { name: "refs/tags/v1.0.0", oid: main };
      const release = {
        number: 2,
        tag_name: "v1.0.0",
        tag_oid: draft ? null : main,
        target_oid: main,
        title: "Crab 1.0",
        body: "## Changes\n\nA stable release.",
        prerelease: true,
        draft,
        version: 1,
        author: "Alice",
        created_at: 1_710_000_000_000,
        published_at: draft ? null : 1_710_000_000_000,
        updated_at: 1_710_000_000_000,
      };
      releases.unshift(release);
      return route.fulfill({ status: 201, json: release });
    }
    if (url.pathname.endsWith("/releases")) {
      const query = url.searchParams.get("query")?.toLowerCase();
      const items = query
        ? releases.filter((release) =>
            [
              release.tag_name,
              release.title,
              release.body,
              release.author,
            ].some((value) => value.toLowerCase().includes(query)),
          )
        : releases;
      return route.fulfill({ json: { items, next: null } });
    }
    const detail = url.pathname.match(/\/releases\/(\d+)$/);
    if (detail) {
      const index = releases.findIndex(
        (release) => release.number === Number(detail[1]),
      );
      if (index < 0)
        return route.fulfill({
          status: 404,
          json: { error: { message: "Release not found" } },
        });
      if (route.request().method() === "PATCH") {
        const edit = route.request().postDataJSON() as Record<string, unknown>;
        expect(edit.version).toBe(releases[index].version);
        const draft = Boolean(edit.draft);
        if (releases[index].draft && !draft)
          publishedTag = { name: "refs/tags/v1.0.0", oid: main };
        releases[index] = {
          ...releases[index],
          title: String(edit.title),
          body: String(edit.body),
          prerelease: Boolean(edit.prerelease),
          draft,
          tag_oid: draft ? releases[index].tag_oid : main,
          published_at: draft ? null : 1_720_000_000_000,
          version: releases[index].version + 1,
          updated_at: 1_720_000_000_000,
        };
        return route.fulfill({ json: releases[index] });
      }
      if (route.request().method() === "DELETE") {
        expect(route.request().postDataJSON()).toEqual({
          version: releases[index].version,
        });
        releases.splice(index, 1);
        return route.fulfill({ status: 204 });
      }
      return route.fulfill({
        json: releases[index],
      });
    }
    return route.fulfill({
      status: 404,
      json: { error: { message: "Not found" } },
    });
  });

  await page.goto("/team/project?view=releases");
  await expect(
    page.getByRole("heading", { name: "Releases", level: 2 }),
  ).toBeVisible();
  await expect(
    page.getByRole("navigation", { name: "Releases and tags" }),
  ).toContainText("ReleasesTags");
  await expect(page.getByRole("heading", { name: "Crab 0.9" })).toBeVisible();
  await expect(
    page.getByRole("search", { name: "Search releases" }),
  ).toBeVisible();
  await expect(
    page.getByRole("heading", { name: "Release list" }),
  ).toBeVisible();
  await expect(page.getByText("Browse code without cloning.")).toBeVisible();
  await expect(page.getByText("Pre-release")).toBeVisible();
  await expect(
    page.getByRole("link", { name: "Source code (zip)" }),
  ).toHaveAttribute("href", /archive\?rev=refs%2Ftags%2Fv0\.9\.0/);
  await page.getByLabel("Find a release").fill("browse code");
  await page.getByLabel("Find a release").press("Enter");
  await expect(page).toHaveURL(/query=browse\+code/);
  await expect(page.getByRole("heading", { name: "Crab 0.9" })).toBeVisible();
  await page.getByLabel("Find a release").fill("missing release");
  await page.getByLabel("Find a release").press("Enter");
  await expect(page.getByText("There aren’t any releases here")).toBeVisible();
  await page.getByLabel("Find a release").fill("");
  await page.getByLabel("Find a release").press("Enter");
  await expectNoAccessibilityViolations(page);

  await page.getByRole("link", { name: "Tags", exact: true }).click();
  await expect(page).toHaveURL(/view=tags/);
  await expect(
    page.getByRole("heading", { name: "Tags", level: 2 }),
  ).toBeVisible();
  await expect(
    page.getByRole("link", { name: "Download zip" }),
  ).toHaveAttribute("href", /archive\?rev=refs%2Ftags%2Fv0\.9\.0/);
  await page.getByRole("link", { name: "Releases", exact: true }).click();

  await page.getByRole("link", { name: "Draft a new release" }).click();
  await expect(
    page.getByRole("heading", { name: "New release" }),
  ).toBeVisible();
  await page.getByLabel("Tag name").fill("v1.0.0");
  await page.getByLabel("Target").selectOption("refs/heads/main");
  await page.getByLabel("Release title").fill("Crab 1.0");
  await page
    .getByRole("textbox", { name: "Release notes", exact: true })
    .fill("## Changes\n\nA stable release.");
  await page.getByLabel("Set as a pre-release").check();
  await expectNoAccessibilityViolations(page);
  await page.getByRole("button", { name: "Save draft" }).click();

  await expect(page).toHaveURL(/view=releases&release=2/);
  await expect(page.getByRole("heading", { name: "Crab 1.0" })).toBeVisible();
  await expect(page.getByText("A stable release.")).toBeVisible();
  await expect(page.getByText("Draft", { exact: true })).toBeVisible();
  await expect(
    page.getByRole("link", { name: "Source code (zip)" }),
  ).toHaveCount(0);
  expect(publishedTag).toBeUndefined();
  expect(submission).toBeDefined();

  await page.getByRole("button", { name: "Edit Crab 1.0" }).click();
  await expect(page).toHaveURL(/release=2&action=edit/);
  await expect(page.getByLabel("Release title")).toHaveValue("Crab 1.0");
  await page.getByLabel("Release title").fill("Crab 1.0 final");
  await page
    .getByRole("textbox", { name: "Release notes", exact: true })
    .fill("Updated release notes.");
  await page.getByLabel("Set as a pre-release").uncheck();
  await page.getByRole("button", { name: "Save draft" }).click();
  await expect(page).toHaveURL(/view=releases&release=2$/);
  await expect(
    page.getByRole("heading", { name: "Crab 1.0 final" }),
  ).toBeVisible();
  await expect(page.getByText("Updated release notes.")).toBeVisible();
  await expect(page.getByText("Pre-release")).toHaveCount(0);
  await expect(page.getByText("Draft", { exact: true })).toBeVisible();

  await page.getByRole("button", { name: "Edit Crab 1.0 final" }).click();
  await page.getByRole("button", { name: "Publish release" }).click();
  await expect(page.getByText("Draft", { exact: true })).toHaveCount(0);
  await expect(
    page.getByRole("link", { name: "Source code (zip)" }),
  ).toBeVisible();
  expect(publishedTag).toEqual({ name: "refs/tags/v1.0.0", oid: main });

  await page.getByRole("button", { name: "Edit Crab 1.0 final" }).click();
  await page.getByLabel("Release title").fill("Crab 1.0 patched");
  await page.getByRole("button", { name: "Update release" }).click();
  await expect(
    page.getByRole("heading", { name: "Crab 1.0 patched" }),
  ).toBeVisible();
  await selectDarkTheme(page);
  await page.setViewportSize({ width: 390, height: 800 });
  await expectNoAccessibilityViolations(page);
  expect(
    await page.evaluate(() => document.documentElement.scrollWidth),
  ).toBeLessThanOrEqual(390);

  await page.getByRole("button", { name: "Delete Crab 1.0 patched" }).click();
  await expect(page.getByText("Delete this release?")).toBeVisible();
  await expect(page.getByText(/Tag v1\.0\.0 will remain/)).toBeVisible();
  await expectNoAccessibilityViolations(page);
  await page.getByRole("button", { name: "Delete this release" }).click();
  await expect(page).toHaveURL(/view=releases$/);
  await expect(page.getByRole("heading", { name: "Crab 0.9" })).toBeVisible();
  await expect(
    page.getByRole("heading", { name: "Crab 1.0 patched" }),
  ).toHaveCount(0);
  await page.getByRole("link", { name: "Tags", exact: true }).click();
  await expect(page.getByRole("link", { name: "v1.0.0" })).toBeVisible();
});
