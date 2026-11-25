import { chooseSelectOption } from "./select";
import { expect, test } from "@playwright/test";
import { expectNoAccessibilityViolations } from "./accessibility";

test("repository search pages on the server and restores a stale continuation", async ({
  page,
}) => {
  const organization = "01900000-0000-7000-8000-000000000010";
  const requests: URLSearchParams[] = [];
  const repo = (name: string, archived = false) => ({
    id: name,
    owner: "team",
    name,
    description: archived ? "Archived project notes" : "Project repository",
    archived,
  });
  let stale = false;
  await page.route("**/api/**", (route) => {
    const { pathname, searchParams } = new URL(route.request().url());
    if (pathname === "/api/session") {
      return route.fulfill({
        json: {
          authenticated: true,
          mode: "oidc",
          csrf: "fixture",
          user: {
            issuer: "https://example.test",
            subject: "alice",
            name: "Alice",
          },
        },
      });
    }
    if (pathname === "/api/notifications/unread")
      return route.fulfill({ json: { unread: false } });
    if (pathname === `/api/organizations/${organization}`) {
      return route.fulfill({
        json: {
          organization: {
            id: organization,
            name: "Team",
            version: 1,
            description: "",
          },
          actor: "alice",
          role: "member",
        },
      });
    }
    if (
      pathname ===
      `/api/organizations/${organization}/repository-properties/filters`
    ) {
      return route.fulfill({
        json: {
          version: 1,
          definitions: [
            {
              id: "region",
              name: "region",
              kind: "single_select",
              options: ["US East", "EU West"],
              description: "",
              default: null,
              values_editable_by: "organization",
            },
          ],
        },
      });
    }
    if (pathname !== "/api/repos")
      return route.fulfill({ status: 404, json: {} });
    requests.push(searchParams);
    expect(searchParams.get("limit")).toBe("32");
    const q = searchParams.get("q");
    const after = searchParams.get("after");
    if (searchParams.get("organization")) {
      expect(searchParams.get("organization")).toBe(organization);
      if (q === 'project props.region:"US East"') {
        return route.fulfill({
          json: after
            ? { repositories: [repo("org-matching-project")], next: null }
            : { repositories: [], next: "property-token" },
        });
      }
      if (
        q === 'project props.region:"US East" archived:true' ||
        q === 'project props.region:"US East" archived:true fork:only'
      ) {
        expect(after).toBeNull();
        return route.fulfill({
          json: {
            repositories: [{ ...repo("archive-fork", true), fork: true }],
            next: null,
          },
        });
      }
      const publicQuery =
        'project props.region:"US East" archived:true fork:only is:public';
      if (q === publicQuery) {
        return route.fulfill({
          json: {
            repositories: [
              {
                ...repo(
                  after ? "archive-public-two" : "archive-public-one",
                  true,
                ),
                fork: true,
                visibility: "public",
              },
            ],
            next: after ? null : "visibility-token",
          },
        });
      }
      if (q === `${publicQuery} sort:created-desc`) {
        expect(after).toBeNull();
        return route.fulfill({
          json: {
            repositories: [
              {
                ...repo("archive-newest", true),
                fork: true,
                visibility: "public",
                created_at_ms: Date.UTC(2026, 0, 15),
              },
            ],
            next: null,
          },
        });
      }
      return route.fulfill({
        json: { repositories: [repo("org-project")], next: null },
      });
    }
    if (q === "project" && after) {
      expect(after).toBe("page-token");
      return stale
        ? route.fulfill({
            status: 409,
            json: {
              error: {
                message: "This list changed; reload from the first page",
              },
            },
          })
        : route.fulfill({
            json: { repositories: [repo("project-notes", true)], next: null },
          });
    }
    if (q === "project") {
      return route.fulfill({
        json: { repositories: [repo("project")], next: "page-token" },
      });
    }
    return route.fulfill({
      json: { repositories: q ? [] : [repo("toolbox")], next: null },
    });
  });

  await page.goto("/");
  await expect(
    page.getByRole("link", { name: "team / toolbox" }),
  ).toBeVisible();
  const search = page.getByRole("searchbox", { name: "Search repositories" });
  await search.fill("project");
  await search.press("Enter");
  await expect(page).toHaveURL(/\?q=project$/);
  await expect(
    page.getByRole("link", { name: "team / project", exact: true }),
  ).toBeVisible();
  await expectNoAccessibilityViolations(page);
  await page.getByRole("link", { name: "Next page" }).click();
  await expect(
    page.getByRole("link", { name: "team / project-notes" }),
  ).toBeVisible();
  await expect(
    page
      .getByRole("list", { name: "Repository results" })
      .getByText("Archived", { exact: true }),
  ).toBeVisible();
  stale = true;
  await page.reload();
  await expect(page.getByRole("alert")).toContainText(
    "reload from the first page",
  );
  await page.getByRole("link", { name: "Back to first page" }).click();
  await expect(
    page.getByRole("link", { name: "team / project", exact: true }),
  ).toBeVisible();
  await expect(search).toHaveValue("project");

  await search.fill("unmatched");
  await search.press("Enter");
  await expect(
    page.getByText("No repositories match this search."),
  ).toBeVisible();
  await page.getByRole("link", { name: "Clear search" }).click();
  await expect(
    page.getByRole("link", { name: "team / toolbox" }),
  ).toBeVisible();

  await page.goto(
    `/-/organizations/${organization}?tab=repositories&q=project`,
  );
  await expect(page.getByRole("link", { name: "org-project" })).toBeVisible();
  expect(requests.at(-1)?.get("q")).toBe("project");
  expect(requests.at(-1)?.get("organization")).toBe(organization);
  await page.getByRole("button", { name: "Filter by property" }).click();
  await chooseSelectOption(
    page.getByRole("combobox", { name: "Property", exact: true }),
    "region",
  );
  await chooseSelectOption(
    page.getByRole("combobox", { name: "Value", exact: true }),
    "US East",
  );
  await expectNoAccessibilityViolations(page);
  await page.getByRole("button", { name: "Add filter", exact: true }).click();
  await expect(page.getByRole("status")).toContainText(
    "More repositories are available",
  );
  expect(requests.at(-1)?.has("after")).toBe(false);
  await page.getByRole("link", { name: "Next page", exact: true }).click();
  await expect(
    page.getByRole("link", { name: "org-matching-project" }),
  ).toBeVisible();
  await expect(search).toHaveValue('project props.region:"US East"');
  await page.reload();
  await expect(
    page.getByRole("link", { name: "org-matching-project" }),
  ).toBeVisible();
  expect(requests.at(-1)?.get("after")).toBe("property-token");
  await chooseSelectOption(
    page.getByRole("combobox", { name: "Archive status" }),
    "true",
  );
  await expect(search).toHaveValue(
    'project props.region:"US East" archived:true',
  );
  await chooseSelectOption(
    page.getByRole("combobox", { name: "Fork status" }),
    "only",
  );
  await expect(search).toHaveValue(
    'project props.region:"US East" archived:true fork:only',
  );
  await expect(
    page.getByRole("link", { name: "archive-fork", exact: true }),
  ).toBeVisible();
  await expect(page.getByText("Fork", { exact: true })).toBeVisible();
  await chooseSelectOption(
    page.getByRole("combobox", { name: "Visibility" }),
    "public",
  );
  await page.getByRole("link", { name: "Next page", exact: true }).click();
  await expect(
    page.getByRole("link", { name: "archive-public-two" }),
  ).toBeVisible();
  expect(requests.at(-1)?.get("after")).toBe("visibility-token");
  await chooseSelectOption(
    page.getByRole("combobox", { name: "Sort repositories" }),
    "created-desc",
  );
  await expect(
    page.getByRole("link", { name: "archive-newest" }),
  ).toBeVisible();
  await expect(search).toHaveValue(
    'project props.region:"US East" archived:true fork:only is:public sort:created-desc',
  );
  await expect(
    page
      .getByRole("list", { name: "Repository results" })
      .getByText("Public", { exact: true }),
  ).toBeVisible();
  await expect(page.locator("relative-time")).toHaveAttribute(
    "datetime",
    "2026-01-15T00:00:00.000Z",
  );
  await expectNoAccessibilityViolations(page);
});
