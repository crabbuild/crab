import { expect, test } from "@playwright/test";
import { mockPullRequest } from "./fixture";

test("a force-push block removes the rebase action and permits an ordinary update", async ({
  page,
}) => {
  const fixture = await mockPullRequest(page);
  const pull = fixture.pull();
  const update = {
    request_id: "019b21da-55c0-7000-8000-000000000127",
    version: pull.version,
    base_oid: pull.base_oid,
    head_oid: pull.head_oid,
  };
  let saved: unknown;
  await page.route(
    (url) => url.pathname === "/api/repos/team/project/pulls/1",
    (route) => route.fulfill({ json: { ...pull, can_update_branch: true } }),
  );
  await page.route(
    (url) => url.pathname === "/api/repos/team/project/pulls/1/update-branch",
    (route) => {
      if (route.request().method() === "POST") {
        saved = route.request().postDataJSON();
        return route.fulfill({ json: { commit: "c".repeat(40) } });
      }
      return route.fulfill({
        json: { update: saved ? null : update, rebase_blocked: true },
      });
    },
  );
  await page.goto("/team/project/pull/1");
  const panel = page.getByRole("region", {
    name: "Update pull request branch",
  });
  await expect(
    panel.getByText("A branch rule blocks rebasing here."),
  ).toBeVisible();
  await expect(
    panel.getByRole("button", { name: "Branch update options" }),
  ).toHaveCount(0);
  await panel.getByRole("button", { name: "Update branch" }).click();
  await expect(panel).toBeHidden();
  expect(saved).toEqual(update);
});

test("a published resolution remains visible when the pull request moves before the switch", async ({
  page,
}) => {
  const fixture = await mockPullRequest(page);
  const pull = fixture.pull();
  const update = {
    request_id: "019b21da-55c0-7000-8000-000000000124",
    version: pull.version,
    base_oid: pull.base_oid,
    head_oid: pull.head_oid,
  };
  const file = {
    path: "README.md",
    path_hex: "524541444d452e6d64",
    text: "<<<<<<< PR head\nhead\n=======\nbase\n>>>>>>> Base branch\n",
  };
  const writes: unknown[] = [];
  await page.route(
    (url) => url.pathname === "/api/repos/team/project/pulls/1",
    (route) => route.fulfill({ json: { ...pull, can_update_branch: true } }),
  );
  await page.route(
    (url) => url.pathname === "/api/repos/team/project/pulls/1/update-branch",
    async (route) => {
      const request = route.request();
      if (request.method() === "GET") {
        const preview = new URL(request.url()).searchParams.has("conflicts");
        await route.fulfill({
          json: { update, conflicts: preview ? [file] : [] },
        });
        return;
      }
      writes.push(request.postDataJSON());
      if (writes.length !== 3) {
        await route.fulfill(
          writes.length === 1
            ? {
                status: 409,
                json: {
                  error: {
                    code: "branch_update_conflict",
                    message: "The base changes conflict with the head branch",
                  },
                },
              }
            : {
                status: 403,
                json: {
                  error: {
                    code: "repository_ruleset",
                    message: "The head branch requires a pull request",
                  },
                },
              },
        );
        return;
      }
      await route.fulfill({
        json: {
          commit: "c".repeat(40),
          branch: "refs/heads/resolved/topic",
          pull_updated: false,
        },
      });
    },
  );
  await page.goto("/team/project/pull/1");
  await page.getByRole("button", { name: "Update branch" }).click();
  await page.getByRole("button", { name: "Resolve conflicts" }).click();
  const dialog = page.getByRole("dialog", { name: "Resolve conflicts" });
  await dialog.getByRole("textbox", { name: "README.md" }).fill("resolved\n");
  await dialog.getByRole("button", { name: "Mark as resolved" }).click();
  await dialog.getByRole("button", { name: "Commit merge" }).click();
  await expect(dialog.getByRole("alert")).toHaveText(
    "The head branch requires a pull request",
  );
  await dialog
    .getByRole("checkbox", {
      name: "Create a branch and update this pull request",
    })
    .click();
  await dialog
    .getByRole("textbox", { name: "Branch name" })
    .fill("resolved/topic");
  const create = dialog.getByRole("button", {
    name: "Create branch and update PR",
  });
  await create.click();
  const notice = dialog.getByRole("alert");
  await expect(notice).toContainText(
    "changed before Crab could switch the head",
  );
  await expect(
    notice.getByRole("link", { name: "resolved/topic" }),
  ).toHaveAttribute("href", "/team/project/tree/resolved%2Ftopic");
  await expect(create).toBeDisabled();
  expect(writes[2]).toEqual({
    ...update,
    new_branch: "resolved/topic",
    resolutions: [{ path_hex: file.path_hex, text: "resolved\n" }],
  });
});

test("rebasing a fork confirms the captured commits and retries the same rewrite", async ({
  page,
}) => {
  const fixture = await mockPullRequest(page);
  const pull = fixture.pull();
  const fork = {
    id: "00000000-0000-4000-8000-000000000120",
    owner: "team",
    name: "project-fork",
  };
  const input = {
    request_id: "019b21da-55c0-7000-8000-000000000123",
    version: pull.version,
    base_oid: pull.base_oid,
    head_oid: pull.head_oid,
  };
  const rebase = { ...input, method: "rebase" };
  const writes: unknown[] = [];
  const commit = "d".repeat(40);
  let acknowledged = false;
  await page.route(
    (url) => url.pathname === "/api/repos/team/project/pulls/1",
    (route) =>
      route.fulfill({
        json: {
          ...pull,
          head_repository: fork,
          can_update_branch: true,
          head_oid: acknowledged ? commit : pull.head_oid,
        },
      }),
  );
  await page.route(
    (url) =>
      url.pathname ===
      "/api/repos/team/project-fork/upstream-pulls/1/update-branch",
    async (route) => {
      const request = route.request();
      if (request.method() === "GET") {
        await route.fulfill({ json: { update: acknowledged ? null : input } });
        return;
      }
      expect(request.headers()["x-crab-repository-id"]).toBe(fork.id);
      writes.push(request.postDataJSON());
      if (writes.length === 1) {
        await route.fulfill({
          status: 200,
          contentType: "application/json",
          body: "",
        });
        return;
      }
      acknowledged = true;
      await route.fulfill({ json: { commit } });
    },
  );
  await page.goto("/team/project/pull/1");
  const panel = page.getByRole("region", {
    name: "Update pull request branch",
  });
  const dialog = page.getByRole("dialog", { name: "Rebase branch" });
  const open = async () => {
    await panel
      .getByRole("button", { name: "Branch update options" })
      .press("Enter");
    await page
      .getByRole("menuitem", { name: "Update with rebase" })
      .press("Enter");
  };
  await open();
  await expect(dialog).toContainText("This rewrites the head branch history");
  await expect(dialog).toContainText(input.base_oid.slice(0, 7));
  await expect(dialog).toContainText(input.head_oid.slice(0, 7));
  await dialog.getByRole("button", { name: "Cancel" }).click();
  await expect(dialog).toBeHidden();
  await expect(
    panel.getByRole("button", { name: "Branch update options" }),
  ).toBeFocused();
  expect(writes).toEqual([]);
  await open();
  await dialog.getByRole("button", { name: "Rebase branch" }).click();
  await expect(panel.getByRole("alert")).toContainText("The response was lost");
  await expect(
    panel.getByRole("button", { name: "Branch update options" }),
  ).toBeHidden();
  await panel.getByRole("button", { name: "Retry rebase" }).click();
  await expect(panel).toBeHidden();
  expect(writes).toEqual([rebase, rebase]);
});

test("a missing branch update reply preserves the fork-scoped submission", async ({
  page,
}) => {
  const fixture = await mockPullRequest(page);
  const fork = {
    id: "00000000-0000-4000-8000-000000000120",
    owner: "team",
    name: "project-fork",
  };
  const pull = fixture.pull();
  const input = {
    request_id: "019b21da-55c0-7000-8000-000000000120",
    version: pull.version,
    base_oid: pull.base_oid,
    head_oid: pull.head_oid,
  };
  const commit = "c".repeat(40);
  let acknowledged = false;
  const writes: unknown[] = [];
  await page.route(
    (url) => url.pathname === "/api/repos/team/project/pulls/1",
    (route) =>
      route.fulfill({
        json: {
          ...pull,
          head_repository: fork,
          can_update_branch: true,
          head_oid: acknowledged ? commit : pull.head_oid,
        },
      }),
  );
  await page.route(
    (url) =>
      url.pathname ===
      "/api/repos/team/project-fork/upstream-pulls/1/update-branch",
    async (route) => {
      const request = route.request();
      if (request.method() === "GET") {
        await route.fulfill({ json: { update: acknowledged ? null : input } });
        return;
      }
      expect(request.headers()["x-crab-repository-id"]).toBe(fork.id);
      writes.push(request.postDataJSON());
      if (writes.length === 1) {
        await route.fulfill({
          status: 200,
          contentType: "application/json",
          body: "",
        });
        return;
      }
      acknowledged = true;
      await route.fulfill({ json: { commit } });
    },
  );
  await page.goto("/team/project/pull/1");
  const panel = page.getByRole("region", {
    name: "Update pull request branch",
  });
  const update = panel.getByRole("button", { name: "Update branch" });
  await update.click();
  await expect(panel.getByRole("alert")).toContainText("The response was lost");
  await update.click();
  await expect(panel).toBeHidden();
  expect(writes).toEqual([input, input]);
});

test("the branch update commits only after every conflicting file is resolved", async ({
  page,
}) => {
  const fixture = await mockPullRequest(page);
  const pull = fixture.pull();
  const comparison = {
    version: pull.version,
    base_oid: pull.base_oid,
    head_oid: pull.head_oid,
  };
  const update = {
    ...comparison,
    request_id: "019b21da-55c0-7000-8000-000000000121",
  };
  const preview = {
    ...comparison,
    request_id: "019b21da-55c0-7000-8000-000000000122",
  };
  const marked = "<<<<<<< PR head\nhead\n=======\nbase\n>>>>>>> Base branch\n";
  const files = [
    { path: "README.md", path_hex: "524541444d452e6d64", text: marked },
    { path: "docs/notes", path_hex: "646f63732f6e6f746573", text: marked },
  ];
  const commit = "d".repeat(40);
  const writes: unknown[] = [];
  let committed = false;
  await page.route(
    (url) => url.pathname === "/api/repos/team/project/pulls/1",
    (route) =>
      route.fulfill({
        json: {
          ...pull,
          can_update_branch: true,
          head_oid: committed ? commit : pull.head_oid,
        },
      }),
  );
  await page.route(
    (url) => url.pathname === "/api/repos/team/project/pulls/1/update-branch",
    async (route) => {
      const request = route.request();
      if (request.method() === "GET") {
        const editor = new URL(request.url()).searchParams.has("conflicts");
        await route.fulfill({
          json: editor
            ? { update: preview, conflicts: files }
            : { update: committed ? null : update },
        });
        return;
      }
      writes.push(request.postDataJSON());
      if (writes.length === 1) {
        await route.fulfill({
          status: 409,
          json: {
            error: {
              code: "branch_update_conflict",
              message: "The base changes conflict with the head branch",
            },
          },
        });
        return;
      }
      committed = true;
      await route.fulfill({ json: { commit } });
    },
  );

  await page.goto("/team/project/pull/1");
  await page.getByRole("button", { name: "Update branch" }).click();
  await page.getByRole("button", { name: "Resolve conflicts" }).click();
  const editor = page.getByRole("dialog", { name: "Resolve conflicts" });
  const readme = editor.getByRole("textbox", { name: "README.md" });
  const resolve = editor.getByRole("button", { name: "Mark as resolved" });
  const publish = editor.getByRole("button", { name: "Commit merge" });
  await expect(readme).toHaveValue(marked);
  await expect(resolve).toBeDisabled();
  await readme.fill("combined README\n");
  await resolve.click();
  await expect(publish).toBeDisabled();
  await editor.getByRole("button", { name: "docs/notes" }).click();
  await editor
    .getByRole("textbox", { name: "docs/notes" })
    .fill("combined notes\n");
  await resolve.click();
  await editor.getByRole("button", { name: /README.md Resolved/ }).click();
  await expect(readme).toHaveValue("combined README\n");
  await publish.click();
  await expect(editor).toBeHidden();
  expect(writes).toEqual([
    update,
    {
      ...preview,
      resolutions: [
        { path_hex: files[0].path_hex, text: "combined README\n" },
        { path_hex: files[1].path_hex, text: "combined notes\n" },
      ],
    },
  ]);
});
