import { expect, test } from "@playwright/test";
import {
  expectNoAccessibilityViolations,
  selectDarkTheme,
} from "./accessibility";
import { mockPullRequest } from "./pulls/fixture";

test.use({ hasTouch: true });

test("Markdown commands, mentions and references survive submitting a description", async ({
  page,
}) => {
  await page.setViewportSize({ width: 390, height: 844 });
  const fixture = await mockPullRequest(page);
  const profileId = "bc62103e-1205-4b30-8f11-ec45007697e8";
  let saved: { title: string; body: string } | undefined;
  await page.route("**/api/repos/team/project/**", async (route) => {
    const request = route.request();
    const url = new URL(request.url());
    if (url.pathname.endsWith("/discussion-search/mentions")) {
      expect(url.searchParams.get("repository_id")).toBeTruthy();
      return route.fulfill({
        json: {
          items:
            url.searchParams.get("search") === "ali"
              ? [{ id: profileId, handle: "alice", name: "Alice" }]
              : [],
        },
      });
    }
    if (url.pathname.endsWith("/discussion-search/references")) {
      expect(url.searchParams.get("search")).toBe("readme");
      return route.fulfill({
        json: {
          items: [
            {
              subject: { kind: "issue", number: 1 },
              title: "Update the README",
              state: "open",
            },
            {
              subject: { kind: "pull", number: 1 },
              title: "Improve the README",
              state: "merged",
            },
          ],
        },
      });
    }
    if (url.pathname.endsWith("/issues") && request.method() === "POST") {
      saved = request.postDataJSON();
      return route.fulfill({
        status: 201,
        json: { ...fixture.pull(), ...saved, number: 2 },
      });
    }
    if (url.pathname.endsWith("/issues/2") && saved)
      return route.fulfill({
        json: { ...fixture.pull(), ...saved, number: 2 },
      });
    return route.fallback();
  });

  await page.goto("/team/project/issues/new");
  await selectDarkTheme(page);
  await page
    .getByRole("textbox", { name: "Title", exact: true })
    .fill("Improve the editor");
  const editor = page.getByRole("textbox", {
    name: "Description",
    exact: true,
  });
  await editor.fill("/alert");
  await expect(page.getByRole("option", { name: /Alert/ })).toBeVisible();
  await expectNoAccessibilityViolations(page);
  await editor.press("Enter");
  await editor.press("ArrowDown");
  await editor.press("Tab");
  await expect(editor).toHaveValue("> [!TIP]\n> Add context here.\n\n");
  await page.keyboard.insertText("Look at the docs");
  await editor.evaluate((input: HTMLTextAreaElement) =>
    input.setSelectionRange(input.value.length, input.value.length),
  );
  await page.keyboard.insertText("Ask @ali");
  const mentions = page.getByRole("listbox", {
    name: "Mention a collaborator",
  });
  await expect(mentions.getByRole("option", { name: /@alice/ })).toBeVisible();
  await editor.press("Tab");
  await page.keyboard.insertText("about #readme");
  const references = page.getByRole("listbox", {
    name: "Issues and pull requests",
  });
  await expect(references.getByRole("option")).toHaveCount(2);
  await references.getByRole("option", { name: /Improve the README/ }).tap();
  await expect(editor).toBeFocused();
  await expect(editor).toHaveValue(
    `> [!TIP]\n> Look at the docs\n\nAsk [@alice](/-/users/${profileId}) about [#1](/team/project/pull/1) `,
  );
  await page.getByRole("tab", { name: "Preview" }).click();
  const preview = page.getByRole("tabpanel", { name: "Preview" });
  await expect(preview.getByText("Tip", { exact: true })).toBeVisible();
  await expect(preview.getByRole("link", { name: "@alice" })).toHaveAttribute(
    "href",
    `/-/users/${profileId}`,
  );
  await expect(preview.getByRole("link", { name: "#1" })).toHaveAttribute(
    "href",
    "/team/project/pull/1",
  );
  await page.getByRole("button", { name: "Create issue" }).click();
  await expect(page).toHaveURL(/\/team\/project\/issues\/2$/);
  await page.reload();
  await expect(
    page.getByRole("link", { name: "#1", exact: true }),
  ).toHaveAttribute("href", "/team/project/pull/1");
  expect(saved?.body).toContain(`[@alice](/-/users/${profileId})`);
});

test("the toolbar inserts a fenced code block and pasting a URL links selected text", async ({
  page,
}) => {
  await mockPullRequest(page);
  await page.goto("/team/project/issues/new");
  const editor = page.getByRole("textbox", {
    name: "Description",
    exact: true,
  });
  await editor.fill("Read the guide");
  await editor.evaluate((input: HTMLTextAreaElement) => {
    input.setSelectionRange(9, 14);
    const clipboardData = new DataTransfer();
    clipboardData.setData("text/plain", "https://example.com/docs");
    input.dispatchEvent(
      new ClipboardEvent("paste", {
        clipboardData,
        bubbles: true,
        cancelable: true,
      }),
    );
  });
  await expect(editor).toHaveValue(
    "Read the [guide](https://example.com/docs)",
  );
  await page.getByRole("button", { name: "Insert a Markdown command" }).click();
  await page.getByRole("option", { name: /Code block/ }).click();
  await page.getByRole("option", { name: "Rust" }).click();
  await page.keyboard.insertText("fn main() {}");
  await expect(editor).toHaveValue(
    "Read the [guide](https://example.com/docs)\n\n```rust\nfn main() {}\n```\n\n",
  );
  await page.getByRole("tab", { name: "Preview" }).click();
  const preview = page.getByRole("tabpanel", { name: "Preview" });
  await expect(preview.getByRole("link", { name: "guide" })).toHaveAttribute(
    "href",
    "https://example.com/docs",
  );
  await expect(preview.locator(".markdown-code-block")).toBeVisible();
});
