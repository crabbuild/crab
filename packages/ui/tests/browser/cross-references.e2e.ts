import { expect, test } from "@playwright/test";
import { mockPullRequest } from "./pulls/fixture";

test("cross-repository references persist typed targets and bare links require one exact match", async ({
  page,
}) => {
  const fixture = await mockPullRequest(page);
  const targetId = "00000000-0000-4000-8000-000000000088";
  const target = "other-team/docs-kit";
  const destination = `/api/discussion-references/${targetId}/pull/1`;
  const exactSearches: string[] = [];
  const collision = [
    {
      subject: { kind: "issue", number: 1 },
      title: "Update the guide",
      state: "open",
    },
    {
      subject: { kind: "pull", number: 1 },
      title: "Improve the guide",
      state: "merged",
    },
  ];
  let saved: { title: string; body: string } | undefined;
  let repositoryId = targetId;
  const submissions: string[] = [];

  await page.route(
    `**/api/repos/${target}/discussion-search/references?*`,
    async (route) => {
      const query = new URL(route.request().url()).searchParams;
      const search = query.get("search") ?? "";
      if (query.get("exact") === "true") exactSearches.push(search);
      const items =
        search === "1"
          ? collision
          : search === "3"
            ? [
                {
                  subject: { kind: "issue", number: 3 },
                  title: "Unique issue",
                  state: "open",
                },
              ]
            : [];
      await route.fulfill({ json: { repository_id: repositoryId, items } });
    },
  );
  await page.route("**/api/repos/team/project/issues**", async (route) => {
    const request = route.request();
    const path = new URL(request.url()).pathname;
    if (path.endsWith("/issues") && request.method() === "POST") {
      saved = request.postDataJSON() as NonNullable<typeof saved>;
      submissions.push(saved.body);
      if (submissions.length === 1) {
        repositoryId = "00000000-0000-4000-8000-000000000077";
        return route.abort("failed");
      }
      return route.fulfill({
        status: 201,
        json: { ...fixture.pull(), ...saved, number: 2 },
      });
    }
    if (path.endsWith("/issues/2") && saved) {
      return route.fulfill({
        json: { ...fixture.pull(), ...saved, number: 2 },
      });
    }
    return route.fallback();
  });
  // The target name now points at the fixture repository, whose ID differs
  // from the one authorized by the reference resolver.
  await page.route(`**${destination}`, (route) =>
    route.fulfill({
      status: 303,
      headers: {
        location: `/team/project/pull/1?repository_id=${targetId}`,
      },
    }),
  );

  await page.goto("/team/project/issues/new");
  await page
    .getByRole("textbox", { name: "Title", exact: true })
    .fill("References");
  const editor = page.getByRole("textbox", {
    name: "Description",
    exact: true,
  });
  await editor.fill(`See ${target}#1`);
  const picker = page.getByRole("listbox", {
    name: "Issues and pull requests",
  });
  await expect(
    picker.getByRole("option", { name: /Update the guide/ }),
  ).toBeVisible();
  await picker.getByRole("option", { name: /Improve the guide/ }).click();
  const explicit = `See [${target}#1](${destination}) `;
  await expect(editor).toHaveValue(explicit);

  const body = `${explicit}\n\nPlain ${target}#3 and ambiguous ${target}#1.\n\nCode: \`${target}#8\`.`;
  await editor.fill(body);
  await page.getByRole("tab", { name: "Preview" }).click();
  const preview = page.getByRole("tabpanel", { name: "Preview" });
  await expect(
    preview.getByRole("link", { name: `${target}#3` }),
  ).toHaveAttribute("href", `/api/discussion-references/${targetId}/issue/3`);
  await expect
    .poll(() => [...new Set(exactSearches)].sort())
    .toEqual(["1", "3"]);
  await expect(preview.getByRole("link", { name: `${target}#1` })).toHaveCount(
    1,
  );
  await expect(preview.locator("code")).toHaveText(`${target}#8`);

  await page.getByRole("button", { name: "Create issue" }).click();
  await page.getByRole("button", { name: "Retry original submission" }).click();
  await expect(page).toHaveURL(/\/team\/project\/issues\/2$/);
  expect(submissions).toEqual([saved?.body, saved?.body]);
  expect(saved?.body).toContain(`[${target}#1](${destination})`);
  expect(saved?.body).toContain(
    `[${target}#3](/api/discussion-references/${targetId}/issue/3)`,
  );
  await page.reload();
  const picked = page.getByRole("link", { name: `${target}#1` });
  await expect(picked).toHaveAttribute("href", destination);
  await picked.click();
  await expect(page).toHaveURL(
    `/team/project/pull/1?repository_id=${targetId}`,
  );
  await expect(
    page.getByRole("heading", { name: "Repository not found" }),
  ).toBeVisible();
});
