import { expect, test } from "@playwright/test";

test("workspace search clears old results when access changes between pages", async ({
  page,
}) => {
  const organization = "01905010-0000-7000-8000-000000000001";
  const repository = "01905010-0000-7000-8000-000000000002";
  const commit = "1".repeat(40);
  let revoked = false;
  await page.route(
    (url) => url.pathname === "/api/code-search",
    (route) => {
      const filters = new URL(route.request().url()).searchParams;
      expect(filters.get("organization")).toBe(organization);
      expect(filters.get("q")).toBe("manifest");
      expect(filters.get("directory")).toBe("src");
      expect(filters.get("kind")).toBe("path");
      expect(filters.get("case_sensitive")).toBe("true");
      if (filters.has("cursor")) {
        expect(filters.get("cursor")).toBe("signed-page");
        revoked = true;
        return route.fulfill({
          status: 409,
          json: {
            error: { message: "This list changed; reload from the first page" },
          },
        });
      }
      return route.fulfill({
        json: {
          scanned: revoked ? 0 : 32,
          skipped: 0,
          next: revoked ? null : "signed-page",
          groups: revoked
            ? []
            : [
                {
                  id: repository,
                  owner: "team",
                  name: "private",
                  commit,
                  files: [
                    {
                      path: "src/manifest.toml",
                      path_hex:
                        Buffer.from("src/manifest.toml").toString("hex"),
                      lines: [],
                      more_lines: false,
                    },
                  ],
                },
              ],
        },
      });
    },
  );
  await page.goto(`/-/code-search?organization=${organization}`);
  const form = page.getByRole("search", { name: "Search workspace code" });
  await form.getByLabel("Search text").fill("manifest");
  await form.getByLabel("Search in").selectOption("path");
  await form.getByLabel("Directory (optional)").fill("src");
  await form.getByRole("checkbox", { name: "Match case" }).check();
  await form.getByRole("button", { name: "Search", exact: true }).click();

  const results = page.getByRole("region", { name: "Code search results" });
  const match = results.getByRole("link", { name: "src/manifest.toml" });
  await expect(match).toHaveAttribute(
    "href",
    `/team/private/blob/${commit}/src/manifest.toml?repository_id=${repository}`,
  );
  await results.getByRole("button", { name: "Scan next page" }).click();
  await expect(match).toHaveCount(0);
  await expect(results.getByRole("alert")).toContainText("This list changed");
  await results.getByRole("button", { name: "Start over" }).click();
  await expect(results.getByText("No further matches.")).toBeVisible();
  await expect(match).toHaveCount(0);
});
