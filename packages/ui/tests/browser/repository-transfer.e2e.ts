import { expect, test } from "@playwright/test";
import { expectNoAccessibilityViolations } from "./accessibility";

test("an administrator can share a durable offer and the recipient can confirm it", async ({
  page,
}) => {
  const repository = "01900000-0000-7000-8000-000000000021";
  const id = "01900000-0000-7000-8000-000000000022";
  const link = `/-/repository-transfers/${id}`;
  const from = { owner: "team", name: "before" };
  const to = { owner: "account-bob", name: "moved" };
  let recipient = false;
  let accepted = false;
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
      return route.fulfill({ json: { unread: false } });
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
        return route.fulfill({ json: { version: 0, offer: null } });
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
  await page.getByLabel("Recipient profile handle").fill("bob");
  await page.getByLabel("Repository name at destination").fill("moved");
  const create = page.getByRole("button", { name: "Create transfer offer" });
  await create.click();
  await expect(page.getByRole("dialog")).toContainText(
    "The response was lost.",
  );
  await create.click();
  await expect(
    page.getByRole("link", { name: "View transfer" }),
  ).toHaveAttribute("href", link);

  recipient = true;
  await page.goto(link);
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
});
