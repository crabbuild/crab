import { expect, test, type Locator } from "@playwright/test";
import { mockPullRequest } from "./pulls/fixture";

async function supplyFile(
  editor: Locator,
  method: "paste" | "drop",
  name: string,
) {
  await editor.evaluate(
    (input, { method, name }) => {
      const files = new DataTransfer();
      files.items.add(new File(["notes"], name, { type: "text/plain" }));
      const event =
        method === "paste"
          ? new ClipboardEvent("paste", {
              bubbles: true,
              cancelable: true,
              clipboardData: files,
            })
          : new DragEvent("drop", {
              bubbles: true,
              cancelable: true,
              dataTransfer: files,
            });
      input.dispatchEvent(event);
    },
    { method, name },
  );
}

test("attached files can be selected, pasted, dropped, retried, and posted", async ({
  page,
}) => {
  const fixture = await mockPullRequest(page);
  const files = new Map<string, string>();
  let saved: { title: string; body: string } | undefined;
  await page.route("**/api/repos/team/project/**", async (route) => {
    const request = route.request();
    const url = new URL(request.url());
    if (url.pathname.endsWith("/discussion-attachments")) {
      const repo = request.headers()["x-crab-repository-id"];
      const id = url.searchParams.get("id");
      const name = url.searchParams.get("name")!;
      if (name === "clipboard.txt" && !files.has(name)) {
        files.set(name, id!);
        return route.fulfill({
          status: 503,
          json: { error: { message: "Try again" } },
        });
      }
      if (files.has(name)) expect(id).toBe(files.get(name));
      files.set(name, id!);
      return route.fulfill({
        json: {
          url: `/api/discussion-attachments/${repo}/${id}`,
          name,
          inline: name.endsWith(".png"),
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
    if (url.pathname.endsWith("/issues/2") && saved) {
      return route.fulfill({
        json: { ...fixture.pull(), ...saved, number: 2 },
      });
    }
    return route.fallback();
  });
  await page.route("**/api/discussion-attachments/**", (route) =>
    route.fulfill({
      status: 200,
      contentType: "image/png",
      body: Buffer.from(
        "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAusB9Wl2n0AAAAAASUVORK5CYII=",
        "base64",
      ),
    }),
  );

  await page.goto("/team/project/issues/new");
  await page
    .getByRole("textbox", { name: "Title", exact: true })
    .fill("Attachment evidence");
  const editor = page.getByRole("textbox", {
    name: "Description",
    exact: true,
  });
  const picker = page.waitForEvent("filechooser");
  await page.getByRole("button", { name: "Attach files" }).click();
  await (
    await picker
  ).setFiles({
    name: "preview.png",
    mimeType: "image/png",
    buffer: Buffer.from("example"),
  });
  await expect(editor).toHaveValue(/!\[preview\.png\]/);
  await supplyFile(editor, "paste", "clipboard.txt");
  await expect(page.getByRole("alert")).toContainText(
    "clipboard.txt: Try again",
  );
  await page.getByRole("button", { name: "Retry", exact: true }).click();
  await supplyFile(editor, "drop", "dropped.txt");
  await expect(editor).toHaveValue(/\[clipboard\.txt\].*\n\n\[dropped\.txt\]/s);
  await page.getByRole("tab", { name: "Preview" }).click();
  const preview = page.getByRole("tabpanel", { name: "Preview" });
  await expect(preview.getByRole("img", { name: "preview.png" })).toBeVisible();
  await expect(
    preview.getByRole("link", { name: "dropped.txt" }),
  ).toHaveAttribute("href", new RegExp(files.get("dropped.txt")!));
  await page.getByRole("button", { name: "Create issue" }).click();
  await expect(page).toHaveURL(/\/issues\/2$/);
  await page.reload();
  await expect(page.getByRole("link", { name: "clipboard.txt" })).toBeVisible();
  expect(saved?.body).not.toContain("Uploading");
});
