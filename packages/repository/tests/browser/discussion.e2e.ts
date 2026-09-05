import { expect, test, type Page } from "@playwright/test";
import {
  expectNoAccessibilityViolations,
  selectDarkTheme,
} from "./accessibility";

async function openDiscussion(page: Page) {
  const record = {
    number: 1,
    author: "Alice",
    body: "Original description",
    version: 1,
    created_at: 1_700_000_000_000,
    updated_at: 1_700_000_000_000,
    can_edit: true,
  };
  let issue = { ...record, title: "Keyboard discussion", state: "open" };
  const comments = [{ ...record, body: "Original comment" }];
  await page.route("**/api/**", async (route) => {
    const request = route.request();
    const path = new URL(request.url()).pathname;
    let json: unknown;
    if (path === "/api/session") {
      json = {
        authenticated: true,
        mode: "oidc",
        csrf: "test-csrf",
        user: { issuer: "test", subject: "alice", name: "Alice" },
      };
    } else if (path === "/api/repos") {
      json = {
        repositories: [
          {
            owner: "team",
            name: "project",
            description: "",
            access: "write",
            archive_version: 0,
            archived: false,
            protection_version: 0,
            protected_branches: [],
          },
        ],
      };
    } else if (path === "/api/repos/team/project/issues/1") {
      if (request.method() === "PATCH") {
        const update = request.postDataJSON() as {
          version: number;
          title?: string;
          body?: string;
          state?: string;
        };
        if (update.version !== issue.version) {
          await route.fulfill({
            status: 409,
            json: { error: { code: "conflict", message: "Content changed" } },
          });
          return;
        }
        issue = { ...issue, ...update, version: issue.version + 1 };
      }
      json = issue;
    } else if (path === "/api/repos/team/project/issues/1/comments") {
      if (request.method() === "POST") {
        const input = request.postDataJSON() as { body: string };
        const comment = {
          ...record,
          number: comments.length + 1,
          body: input.body,
        };
        comments.push(comment);
        json = comment;
      } else json = { items: comments, next: null };
    } else if (path === "/api/repos/team/project/issues/1/comments/1") {
      if (request.method() === "PATCH") {
        const input = request.postDataJSON() as {
          body: string;
          version: number;
        };
        if (input.version !== comments[0].version) {
          await route.fulfill({
            status: 409,
            json: { error: { code: "conflict", message: "Content changed" } },
          });
          return;
        }
        comments[0] = {
          ...comments[0],
          body: input.body,
          version: comments[0].version + 1,
        };
      }
      json = comments[0];
    } else
      throw new Error(
        `Unexpected discussion request: ${request.method()} ${path}`,
      );
    await route.fulfill({ json });
  });
  await page.goto("/team/project?view=issues&issue=1");
  await expect(
    page.getByRole("heading", { name: "Keyboard discussion #1" }),
  ).toBeVisible();
  await expect(
    page.getByRole("complementary", { name: "Issue navigation" }),
  ).toHaveCount(0);
  await expect(
    page.getByRole("button", { name: "Edit comment", exact: true }),
  ).toBeVisible();
  return {
    changeIssue: (body: string) => {
      issue = {
        ...issue,
        title: "Updated elsewhere",
        body,
        version: issue.version + 1,
      };
    },
    changeComment: (body: string) => {
      comments[0] = { ...comments[0], body, version: comments[0].version + 1 };
    },
  };
}

test("issue editing and state changes retain a keyboard continuation point", async ({
  page,
}) => {
  await openDiscussion(page);
  await expectNoAccessibilityViolations(page);
  await selectDarkTheme(page);
  const edit = page.getByRole("button", { name: "Edit issue", exact: true });
  await edit.focus();
  await page.keyboard.press("Enter");
  await expect(page.getByLabel("Title", { exact: true })).toBeFocused();
  await expectNoAccessibilityViolations(page);
  await page.getByRole("button", { name: "Cancel edit", exact: true }).click();
  await expect(edit).toBeFocused();
  await page.keyboard.press("Enter");
  await page.getByLabel("Title", { exact: true }).fill("Updated discussion");
  await page.getByRole("button", { name: "Save changes", exact: true }).click();
  await expect(edit).toBeFocused();
  await expect(
    page.getByRole("heading", { name: "Updated discussion #1" }),
  ).toBeVisible();
  await page.getByRole("button", { name: "Close issue", exact: true }).click();
  await expect(
    page.getByRole("button", { name: "Reopen issue", exact: true }),
  ).toBeFocused();
  await page.keyboard.press("Enter");
  await expect(
    page.getByRole("button", { name: "Close issue", exact: true }),
  ).toBeFocused();
});

test("comment edits restore focus and Markdown preview posts reset to Write", async ({
  page,
}) => {
  await openDiscussion(page);
  const card = page.locator("#comment-1");
  const edit = card.getByRole("button", { name: "Edit comment", exact: true });
  await edit.click();
  await expect(
    page.getByRole("textbox", { name: "Edit comment", exact: true }),
  ).toBeFocused();
  await card.getByRole("button", { name: "Cancel edit", exact: true }).click();
  await expect(edit).toBeFocused();
  await edit.click();
  await page
    .getByRole("textbox", { name: "Edit comment", exact: true })
    .fill("Edited comment");
  await card.getByRole("button", { name: "Save comment", exact: true }).click();
  await expect(edit).toBeFocused();
  await expect(card.getByText("Edited comment", { exact: true })).toBeVisible();

  const editor = page.locator(".new-comment");
  await editor
    .getByRole("textbox", { name: "Comment", exact: true })
    .fill("**New comment**");
  await editor.getByRole("tab", { name: "Write", exact: true }).focus();
  await page.keyboard.press("ArrowRight");
  await expect(
    editor.getByRole("tab", { name: "Preview", exact: true }),
  ).toBeFocused();
  await expect(
    editor.getByRole("tabpanel", { name: "Preview", exact: true }),
  ).toContainText("New comment");
  const controlledPanels = await editor
    .getByRole("tab")
    .evaluateAll((tabs) =>
      tabs.every(
        (tab) =>
          document
            .getElementById(tab.getAttribute("aria-controls") ?? "")
            ?.getAttribute("role") === "tabpanel",
      ),
    );
  expect(controlledPanels).toBe(true);
  await page.keyboard.press("Tab");
  await expect(
    editor.getByRole("tabpanel", { name: "Preview", exact: true }),
  ).toBeFocused();
  await editor.getByRole("button", { name: "Comment", exact: true }).click();
  await expect(editor.getByRole("status")).toHaveText("Comment posted.");
  await expect(
    editor.getByRole("textbox", { name: "Comment", exact: true }),
  ).toBeFocused();
  await expect(
    editor.getByRole("textbox", { name: "Comment", exact: true }),
  ).toHaveValue("");
  await expect(page.locator("#comment-2")).toContainText("New comment");
});

test("failed posts preserve drafts and slow replies do not steal focus", async ({
  page,
}) => {
  await openDiscussion(page);
  const editor = page.locator(".new-comment");
  const textbox = editor.getByRole("textbox", { name: "Comment", exact: true });
  await textbox.fill("Keep my draft");
  const url = /\/issues\/1\/comments(?:\?.*)?$/;
  await page.route(url, (route) =>
    route.fulfill({
      status: 502,
      json: { error: { message: "Storage unavailable" } },
    }),
  );
  await editor.getByRole("button", { name: "Comment", exact: true }).click();
  await expect(editor.getByRole("alert")).toContainText(
    "Your draft is still in this form.",
  );
  await expect(textbox).toBeFocused();
  await expect(textbox).toHaveValue("Keep my draft");
  await page.unroute(url);

  let release!: () => void;
  const gate = new Promise<void>((resolve) => {
    release = resolve;
  });
  await page.route(url, async (route) => {
    await gate;
    await route.fallback();
  });
  try {
    await editor.getByRole("button", { name: "Comment", exact: true }).click();
    await expect(
      editor.getByRole("button", { name: "Posting…", exact: true }),
    ).toBeDisabled();
    const elsewhere = page.getByRole("button", {
      name: "Refresh discussion",
      exact: true,
    });
    await elsewhere.focus();
    release();
    await expect(editor.getByRole("status")).toHaveText("Comment posted.");
    await expect(elsewhere).toBeFocused();
  } finally {
    release();
  }
});

test("header controls remain reachable on narrow screens in both themes", async ({
  page,
}) => {
  await openDiscussion(page);
  const header = page.locator(".global-header");
  for (const theme of ["light", "dark"]) {
    await header.getByRole("button", { name: "Appearance" }).click();
    await page
      .getByRole("menuitemradio", {
        name: theme === "light" ? "Light" : "Dark",
        exact: true,
      })
      .click();
    for (const width of [320, 390, 640, 900, 1280]) {
      await page.setViewportSize({ width, height: 900 });
      const obscured = await header
        .locator("a:visible, summary:visible, button:visible, select:visible")
        .evaluateAll((controls) =>
          controls
            .filter((control) => {
              const rect = control.getBoundingClientRect();
              const hit = document.elementFromPoint(
                rect.x + rect.width / 2,
                rect.y + rect.height / 2,
              );
              return (
                rect.left < 0 ||
                rect.right > innerWidth ||
                !hit ||
                !control.contains(hit)
              );
            })
            .map((control) => control.textContent?.trim()),
        );
      expect(obscured, `${theme} at ${width}px`).toEqual([]);
      await header.getByText("Git access", { exact: true }).click();
      const popover = header.locator(".git-popover");
      await expect(popover.getByRole("heading")).toBeVisible();
      const bounds = await popover.boundingBox();
      expect(bounds).not.toBeNull();
      expect(bounds!.x).toBeGreaterThanOrEqual(0);
      expect(bounds!.x + bounds!.width).toBeLessThanOrEqual(width);
      await header.getByText("Git access", { exact: true }).click();
    }
  }
});

for (const kind of ["issue", "comment"] as const) {
  test(`${kind} conflicts require explicit review and retain drafts through repeated races`, async ({
    page,
  }) => {
    const fixture = await openDiscussion(page);
    const change =
      kind === "issue" ? fixture.changeIssue : fixture.changeComment;
    const edit = page.getByRole("button", {
      name: kind === "issue" ? "Edit issue" : "Edit comment",
      exact: true,
    });
    await edit.click();
    const form =
      kind === "issue"
        ? page.locator(".discussion-compose.panel")
        : page.locator(".comment-edit");
    const draft = form.getByRole("textbox", {
      name: kind === "issue" ? "Description" : "Edit comment",
      exact: true,
    });
    const save = form.getByRole("button", {
      name: kind === "issue" ? "Save changes" : "Save comment",
      exact: true,
    });
    let writes = 0;
    page.on("request", (request) => {
      if (request.method() === "PATCH") writes++;
    });
    await draft.fill("My unsaved changes");
    if (kind === "issue")
      await form.getByLabel("Title", { exact: true }).fill("My title");
    change("Saved **elsewhere** <script>untrusted</script>");
    await save.click();
    const review = form.getByRole("region", { name: "Review newer content" });
    await expect(review).toContainText(
      "Saved **elsewhere** <script>untrusted</script>",
    );
    await expect(review).toBeFocused();
    await expect(save).toBeDisabled();
    await expect(draft).toHaveValue("My unsaved changes");
    await review
      .getByRole("button", { name: "Continue with my draft" })
      .click();
    await expect(save).toBeFocused();
    await expect(draft).toHaveValue("My unsaved changes");
    expect(writes).toBe(1);
    change("Another concurrent edit");
    await save.click();
    await expect(review).toContainText("version 3");
    await expect(review).toContainText("Another concurrent edit");
    await review.getByRole("button", { name: "Use saved content" }).click();
    await expect(draft).toHaveValue("Another concurrent edit");
    if (kind === "issue")
      await expect(form.getByLabel("Title", { exact: true })).toHaveValue(
        "Updated elsewhere",
      );
    expect(writes).toBe(2);
    await draft.fill("Another concurrent edit\n\nMy additions");
    await save.click();
    await expect(edit).toBeFocused();
    const rendered =
      kind === "issue"
        ? page.locator(".issue-detail .discussion-main > .discussion-card")
        : page.locator("#comment-1");
    await expect(rendered).toContainText("My additions");
    await page.reload();
    await expect(rendered).toContainText("Another concurrent edit");
    await expect(rendered).toContainText("My additions");
    expect(writes).toBe(3);
  });
}

test("a failed conflict read retains the draft and can be retried without publishing", async ({
  page,
}) => {
  const fixture = await openDiscussion(page);
  await page.getByRole("button", { name: "Edit issue", exact: true }).click();
  const draft = page.getByRole("textbox", { name: "Description", exact: true });
  await draft.fill("Keep this draft through read failures");
  fixture.changeIssue("Saved while editing");
  const url = /\/issues\/1\?/;
  await page.route(url, (route) =>
    route.request().method() === "GET"
      ? route.fulfill({
          status: 502,
          json: { error: { message: "Storage unavailable" } },
        })
      : route.fallback(),
  );
  const save = page.getByRole("button", { name: "Save changes", exact: true });
  await save.click();
  const review = page.getByRole("region", { name: "Review newer content" });
  await expect(
    review.getByRole("alert").filter({ hasText: "Unable to load" }),
  ).toBeVisible();
  await expect(save).toBeDisabled();
  await expect(draft).toHaveValue("Keep this draft through read failures");
  await page.unroute(url);
  await review.getByRole("button", { name: "Try again", exact: true }).click();
  await expect(review).toContainText("Saved while editing");
  await review.getByRole("button", { name: "Continue with my draft" }).click();
  await expect(draft).toHaveValue("Keep this draft through read failures");
  await expect(save).toBeEnabled();
});
