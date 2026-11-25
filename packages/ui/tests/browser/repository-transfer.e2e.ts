import { expect, test } from "@playwright/test";
import { expectNoAccessibilityViolations } from "./accessibility";
import { mockPullRequest } from "./pulls/fixture";
import { chooseSelectOption } from "./select";

test("an administrator creates an offer that the recipient can find and confirm", async ({
  page,
}) => {
  const repository = "01900000-0000-7000-8000-000000000021";
  const id = "01900000-0000-7000-8000-000000000022";
  const link = `/-/repository-transfers/${id}`;
  const from = { owner: "team", name: "before" };
  const to = { owner: "account-bob", name: "moved" };
  let recipient = false;
  let accepted = false;
  let canTransfer = false;
  let proposal: unknown;
  const offer = () => ({
    id,
    repository,
    from,
    to,
    recipient: "@bob",
    state: accepted ? "accepted" : "pending",
    expires_at_ms: Date.now() + 86_400_000,
    can_accept: recipient && !accepted,
    can_cancel: !recipient && !accepted,
  });
  await page.route("**/api/**", async (route) => {
    const request = route.request();
    const { pathname } = new URL(request.url());
    if (pathname === "/api/session")
      return route.fulfill({
        json: {
          authenticated: true,
          mode: "oidc",
          csrf: "fixture-csrf",
          user: {
            issuer: "https://example.test",
            subject: recipient ? "bob" : "alice",
            name: recipient ? "Bob" : "Alice",
          },
        },
      });
    if (pathname === "/api/notifications/unread")
      return route.fulfill({
        json: {
          unread: false,
          pending_transfers: recipient && !accepted,
        },
      });
    if (pathname === "/api/notifications")
      return route.fulfill({ json: { items: [], next: null } });
    if (pathname === "/api/repos")
      return route.fulfill({
        json: {
          repositories: [
            {
              id: repository,
              ...(accepted ? to : from),
              description: "",
              can_admin: true,
              archived: false,
              archive_version: 0,
              protection_version: 0,
              protected_branches: [],
            },
          ],
        },
      });
    if (pathname === `/api/repositories/${repository}/transfer`) {
      if (request.method() === "GET")
        return route.fulfill({
          json: { version: 0, can_transfer: canTransfer, offer: null },
        });
      expect(canTransfer).toBe(true);
      expect(request.headers()["x-csrf-token"]).toBe("fixture-csrf");
      if (!proposal) {
        proposal = request.postDataJSON();
        expect(proposal).toMatchObject({
          recipient: { kind: "account", handle: "bob" },
          name: "moved",
          version: 0,
          request_id: expect.any(String),
        });
        return route.abort("failed");
      }
      expect(request.postDataJSON()).toEqual(proposal);
      return route.fulfill({ json: offer() });
    }
    if (pathname === "/api/repository-transfers/incoming")
      return route.fulfill({
        json: { items: recipient && !accepted ? [offer()] : [], next: null },
      });
    if (pathname === `/api/repository-transfers/${id}`) {
      if (request.method() === "PUT") {
        expect(recipient).toBe(true);
        expect(request.headers()["x-csrf-token"]).toBe("fixture-csrf");
        expect(request.postDataJSON()).toEqual({ decision: "accept" });
        if (!accepted) {
          accepted = true;
          return route.abort("failed");
        }
      }
      return route.fulfill({ json: offer() });
    }
    if (pathname.endsWith("/refs"))
      return route.fulfill({
        json: { head: null, default_branch: "main", refs: [] },
      });
    if (pathname.endsWith("/settings/merge-methods"))
      return route.fulfill({ json: { version: 0, enabled: ["merge_commit"] } });
    return route.fulfill({ status: 404 });
  });

  await page.goto("/team/before/settings");
  await page.getByRole("button", { name: "Transfer repository" }).click();
  await expect(page.getByRole("dialog")).toContainText(
    "Your organization’s transfer policy does not let your account transfer this repository.",
  );
  await expect(
    page.getByRole("button", { name: "Create transfer offer" }),
  ).toHaveCount(0);
  await page.keyboard.press("Escape");
  canTransfer = true;
  await page.getByRole("button", { name: "Transfer repository" }).click();
  await page.getByLabel("Recipient profile handle").fill("bob");
  await page.getByLabel("Repository name at destination").fill("moved");
  const create = page.getByRole("button", { name: "Create transfer offer" });
  await expect(create).toBeDisabled();
  await page.getByLabel("Type before to confirm transfer.").fill("before");
  await create.click();
  await expect(page.getByRole("dialog")).toContainText(
    "The response was lost.",
  );
  await create.click();
  await expect(
    page.getByRole("link", { name: "View transfer" }),
  ).toHaveAttribute("href", link);

  recipient = true;
  await page.goto("/-/notifications");
  await page
    .getByRole("link", { name: "Notifications, pending repository transfers" })
    .click();
  await expect(
    page.getByRole("heading", { name: "Incoming repository transfers" }),
  ).toBeVisible();
  await expectNoAccessibilityViolations(page);
  await page
    .getByRole("link", { name: "Review transfer of team/before" })
    .click();
  const accept = page.getByRole("button", { name: "Accept transfer" });
  await expect(accept).toBeDisabled();
  await expectNoAccessibilityViolations(page);
  await page.getByLabel("Type moved to accept ownership.").fill("moved");
  await accept.click();
  await expect(page.getByRole("alert")).toContainText("The response was lost.");
  await accept.click();
  await expect(page.getByText("Transfer accepted")).toBeVisible();
  await expect(
    page.getByRole("link", { name: "Open repository" }),
  ).toHaveAttribute("href", `/account-bob/moved?repository_id=${repository}`);
  await page.reload();
  await expect(page.getByText("Transfer accepted")).toBeVisible();
  await page.goto("/-/notifications?tab=transfers");
  await expect(
    page.getByText("No pending repository transfers."),
  ).toBeVisible();
});

test("an organization owner sets destination properties when accepting", async ({
  page,
}) => {
  const id = "01900000-0000-7000-8000-000000000050";
  const repository = "01900000-0000-7000-8000-000000000051";
  const property = "01900000-0000-7000-8000-000000000052";
  const offer = {
    id,
    repository,
    from: { owner: "team", name: "project" },
    to: { owner: "org-receiver", name: "project" },
    recipient: "Crab Data",
    expires_at_ms: Date.now() + 86_400_000,
    state: "pending",
    can_accept: true,
    can_cancel: false,
  };
  await page.route("**/api/session", (route) =>
    route.fulfill({
      json: {
        authenticated: true,
        csrf: "fixture-csrf",
        mode: "oidc",
        user: { issuer: "fixture", subject: "alex", name: "Alex" },
      },
    }),
  );
  await page.route("**/api/notifications/unread", (route) =>
    route.fulfill({ json: { unread: false, pending_transfers: false } }),
  );
  await page.route(`**/api/repository-transfers/${id}`, (route) => {
    if (route.request().method() === "PUT") {
      expect(route.request().postDataJSON()).toEqual({
        decision: "accept",
        properties: { [property]: "Platform" },
      });
      return route.fulfill({
        json: { ...offer, state: "accepted", can_accept: false },
      });
    }
    return route.fulfill({
      json: {
        ...offer,
        properties: [
          {
            id: property,
            name: "service_area",
            description: "Responsible group",
            kind: "text",
            options: [],
            default: "Shared",
            required: true,
            require_explicit_values: true,
            values_editable_by: "organization",
          },
        ],
      },
    });
  });

  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto(`/-/repository-transfers/${id}`);
  const field = page.getByLabel("service_area");
  await expect(field).toHaveAttribute("placeholder", "Choose a value");
  await expectNoAccessibilityViolations(page);
  await page.getByLabel("Type project to accept ownership.").fill("project");
  await expect(
    page.getByRole("button", { name: "Accept transfer" }),
  ).toBeDisabled();
  await field.fill("Platform");
  await page.getByRole("button", { name: "Accept transfer" }).click();
  await expect(page.getByText("Transfer accepted")).toBeVisible();
});

for (const method of ["directory", "link"] as const) {
  test(`an organization transfer uses the stable destination ID from the ${method}`, async ({
    page,
  }) => {
    const { lifecycle } = await mockPullRequest(page);
    const source = "01900000-0000-7000-8000-000000000010";
    const destination = "01900000-0000-7000-8000-000000000020";
    const inaccessible = "01900000-0000-7000-8000-000000000030";
    await page.route("**/api/session", (route) =>
      route.fulfill({
        json: {
          authenticated: true,
          mode: "oidc",
          csrf: "fixture-csrf",
          user: { issuer: "fixture", subject: "alice", name: "Alice" },
        },
      }),
    );
    await page.route("**/api/repos?*", (route) =>
      route.fulfill({
        json: {
          repositories: [
            {
              id: lifecycle.repositoryId,
              organization_id: source,
              owner: "team",
              name: "project",
              can_admin: true,
              archived: false,
              protected_branches: [],
            },
          ],
        },
      }),
    );
    await page.route("**/api/organizations*", (route) => {
      const after = new URL(route.request().url()).searchParams.get("after");
      return route.fulfill({
        json: after
          ? {
              items: [
                {
                  kind: "member",
                  role: "member",
                  can_create: true,
                  organization: { id: destination, name: "Crab Data" },
                },
              ],
              next: null,
            }
          : {
              items: [
                {
                  kind: "member",
                  role: "owner",
                  can_create: true,
                  organization: { id: source, name: "Source" },
                },
                {
                  kind: "member",
                  role: "member",
                  can_create: false,
                  organization: { id: inaccessible, name: "Read only" },
                },
              ],
              next: "next-page",
            },
      });
    });
    await page.route(
      `**/api/repositories/${lifecycle.repositoryId}/transfer`,
      (route) => {
        if (route.request().method() === "GET")
          return route.fulfill({
            json: { version: 0, can_transfer: true, offer: null },
          });
        expect(route.request().postDataJSON()).toMatchObject({
          recipient: { kind: "organization", id: destination },
          name: "project",
          version: 0,
        });
        return route.fulfill({
          json: {
            id: "01900000-0000-7000-8000-000000000040",
            recipient: "Crab Data",
            state: "pending",
          },
        });
      },
    );

    await page.setViewportSize({ width: 320, height: 640 });
    await page.goto("/team/project/settings");
    await page.getByRole("button", { name: "Transfer repository" }).click();
    const dialog = page.getByRole("dialog", {
      name: "Transfer repository",
      exact: true,
    });
    const type = dialog.getByRole("combobox", { name: "Recipient type" });
    const create = dialog.getByRole("button", {
      name: "Create transfer offer",
    });
    const confirm = dialog.getByLabel("Type project to confirm transfer.");
    if (method === "directory") {
      await chooseSelectOption(type, "directory");
      await dialog.getByRole("button", { name: "Choose organization" }).click();
      const picker = page.getByRole("dialog", { name: "Choose organization" });
      await expect(
        picker.getByText("No eligible organizations on this page."),
      ).toBeVisible();
      await picker.getByRole("button", { name: "Next page" }).click();
      await picker.getByRole("button", { name: "Crab Data" }).click();
      await expect(
        dialog.getByRole("button", { name: "Crab Data" }),
      ).toBeFocused();
      await expect(
        dialog.getByLabel("Repository name at destination"),
      ).toHaveJSProperty("readOnly", true);
      await expect(
        dialog.getByLabel("Repository name at destination"),
      ).toHaveValue("project");
    } else {
      await chooseSelectOption(type, "organization");
      const link = dialog.getByRole("textbox", {
        name: "Organization link or ID",
      });
      await confirm.fill("project");
      await link.fill(
        `https://another.example/-/organizations/crab-data--${destination}`,
      );
      await expect(create).toBeDisabled();
      await link.fill(`/-/organizations/crab-data--${source}`);
      await expect(create).toBeDisabled();
      await confirm.clear();
      await link.fill(
        new URL(
          `/-/organizations/crab-data--${destination}?tab=repositories`,
          page.url(),
        ).href,
      );
    }
    await expect(create).toBeDisabled();
    await expectNoAccessibilityViolations(page);
    await confirm.fill("project");
    await create.click();
    await expect(
      dialog.getByText("Awaiting acceptance from Crab Data."),
    ).toBeVisible();
  });
}
