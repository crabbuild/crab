import { expect, test, type Page } from "@playwright/test";

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
        repositories: [{ owner: "team", name: "project", description: "" }],
      };
    } else if (path === "/api/repos/team/project/issues/1") {
      if (request.method() === "PATCH") {
        const update = request.postDataJSON() as {
          title?: string;
          body?: string;
          state?: string;
        };
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
    } else if (
      path === "/api/repos/team/project/issues/1/comments/1" &&
      request.method() === "PATCH"
    ) {
      const input = request.postDataJSON() as { body: string };
      comments[0] = {
        ...comments[0],
        body: input.body,
        version: comments[0].version + 1,
      };
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
    page.getByRole("button", { name: "Edit comment", exact: true }),
  ).toBeVisible();
}

test("issue editing and state changes retain a keyboard continuation point", async ({
  page,
}) => {
  await openDiscussion(page);
  const edit = page.getByRole("button", { name: "Edit issue", exact: true });
  await edit.focus();
  await page.keyboard.press("Enter");
  await expect(page.getByLabel("Title", { exact: true })).toBeFocused();
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
    await header.getByLabel("Appearance").selectOption(theme);
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
      await header.locator("summary").click();
      const popover = header.locator(".git-popover");
      await expect(popover.getByRole("heading")).toBeVisible();
      const bounds = await popover.boundingBox();
      expect(bounds).not.toBeNull();
      expect(bounds!.x).toBeGreaterThanOrEqual(0);
      expect(bounds!.x + bounds!.width).toBeLessThanOrEqual(width);
      await header.locator("summary").click();
    }
  }
});
