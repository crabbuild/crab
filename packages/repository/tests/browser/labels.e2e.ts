import { expect, test } from "@playwright/test";

test("repository labels can be managed and assigned to an issue", async ({
  page,
}) => {
  let labels: Array<Record<string, unknown>> = [];
  let selected: number[] = [];
  let issueVersion = 1;
  await page.route("**/api/**", async (route) => {
    const request = route.request();
    const path = new URL(request.url()).pathname;
    if (path === "/api/session")
      return route.fulfill({
        json: {
          authenticated: true,
          mode: "local",
          user: null,
          csrf: null,
        },
      });
    if (path === "/api/repos")
      return route.fulfill({
        json: {
          repositories: [
            {
              owner: "team",
              name: "project",
              description: "",
              access: "write",
              protected_branches: [],
            },
          ],
        },
      });
    if (path === "/api/repos/team/project/labels") {
      if (request.method() === "POST") {
        const input = request.postDataJSON() as Record<string, unknown>;
        labels = [
          {
            id: 1,
            name: input.name,
            color: input.color,
            description: input.description,
            version: 1,
            created_at: 1_700_000_000_000,
            updated_at: 1_700_000_000_000,
          },
        ];
        return route.fulfill({ status: 201, json: labels[0] });
      }
      return route.fulfill({ json: { items: labels, can_manage: true } });
    }
    if (path === "/api/repos/team/project/labels/1") {
      if (request.method() === "DELETE") {
        labels = [];
        return route.fulfill({ status: 204 });
      }
      const input = request.postDataJSON() as Record<string, unknown>;
      labels = [
        {
          ...labels[0],
          ...input,
          version: 2,
          updated_at: 1_700_000_010_000,
        },
      ];
      return route.fulfill({ json: labels[0] });
    }
    if (path === "/api/repos/team/project/issues/1") {
      if (request.method() === "PATCH") {
        const input = request.postDataJSON() as {
          version: number;
          label_ids: number[];
        };
        selected = input.label_ids;
        issueVersion += 1;
      }
      return route.fulfill({
        json: {
          number: 1,
          author: "Alice",
          title: "Needs triage",
          body: "Please investigate.",
          state: "open",
          version: issueVersion,
          created_at: 1_700_000_000_000,
          updated_at: 1_700_000_000_000,
          can_edit: true,
          can_label: true,
          labels: labels.filter((label) =>
            selected.includes(label.id as number),
          ),
        },
      });
    }
    if (path === "/api/repos/team/project/issues/1/comments")
      return route.fulfill({ json: { items: [], next: null } });
    throw new Error(`Unexpected label request: ${request.method()} ${path}`);
  });

  await page.goto("/team/project?view=labels");
  await page.getByLabel("Name", { exact: true }).fill("bug");
  await page
    .getByLabel("Description", { exact: true })
    .fill("Confirmed defect");
  await page.getByRole("button", { name: "Create label" }).click();
  await expect(page.getByText("bug", { exact: true })).toBeVisible();

  await page.getByRole("button", { name: "Edit", exact: true }).click();
  const edit = page.locator(".label-row-edit");
  await edit.getByLabel("Name", { exact: true }).fill("kind/bug");
  await edit.getByRole("button", { name: "Save changes" }).click();
  await expect(page.getByText("kind/bug", { exact: true })).toBeVisible();

  await page.goto("/team/project?view=issues&issue=1");
  await page.locator(".label-picker summary").click();
  await page.getByRole("checkbox", { name: "kind/bug" }).check();
  await page.getByRole("button", { name: "Apply labels" }).click();
  await expect(page.locator(".discussion-label-controls")).toContainText(
    "kind/bug",
  );

  await page.goto("/team/project?view=labels");
  page.once("dialog", (dialog) => dialog.accept());
  await page.getByRole("button", { name: "Delete", exact: true }).click();
  await expect(page.getByText("No labels yet")).toBeVisible();
});
