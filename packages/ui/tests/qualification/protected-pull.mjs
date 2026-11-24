import assert from "node:assert/strict";
import { chromium, expect } from "@playwright/test";

const [, , originValue, action, head] = process.argv;
const origin = new URL(originValue);
assert.equal(origin.protocol, "http:");
assert.equal(origin.hostname, "127.0.0.1");
assert.match(head, /^[0-9a-f]{40}$/);

const pull = "/team/private/pull/1";
const api = "/api/repos/team/private/pulls";
const browser = await chromium.launch();

try {
  const page = await browser.newPage({ baseURL: origin.origin });
  page.setDefaultTimeout(15_000);
  const button = (name) => page.getByRole("button", { name, exact: true });
  const merge = page.getByRole("region", { name: "Merge status", exact: true });
  const navigation =
    action === "create"
      ? "/team/private/compare/main...feature"
      : action === "review"
        ? `${pull}/files`
        : pull;
  const post = (request, path) =>
    request.method() === "POST" && new URL(request.url()).pathname === path;

  await page.goto(navigation);
  await page
    .getByRole("link", { name: "Continue to sign in", exact: true })
    .click();
  switch (action) {
    case "create": {
      await page
        .getByLabel("Title", { exact: true })
        .fill("Protect the deploy branch");
      const submitted = page.waitForResponse((response) =>
        post(response.request(), api),
      );
      await button("Create pull request").click();
      const response = await submitted;
      assert.equal(response.status(), 201);
      const created = await response.json();
      assert.deepEqual(response.request().postDataJSON().expected, {
        base_oid: created.base_oid,
        head_oid: head,
      });
      await expect(page).toHaveURL(new URL(pull, origin).href);
      break;
    }
    case "review": {
      await button("Review changes").click();
      await page
        .getByRole("textbox", { name: "Review summary", exact: true })
        .fill("Reviewed this commit");
      await page.getByLabel("Approve", { exact: true }).check();
      const submitted = page.waitForResponse((response) =>
        post(response.request(), `${api}/1/reviews`),
      );
      await button("Submit review").click();
      const response = await submitted;
      assert.ok(response.ok(), `Review returned HTTP ${response.status()}`);
      assert.equal(response.request().postDataJSON().commit_oid, head);
      await expect(
        page.getByText("Bob approved these changes", { exact: false }).last(),
      ).toBeVisible();
      break;
    }
    case "waiting":
    case "revoked":
    case "publish": {
      const checks = merge.locator(".required-checks");
      await expect(
        checks.getByText("Required provider · GitHub Actions", {
          exact: false,
        }),
      ).toBeVisible();
      await expect(checks.locator("code")).toHaveAttribute("title", head);
      await expect(checks).toContainText(
        action === "waiting"
          ? "Required checks are waiting"
          : "All required checks have passed",
      );
      if (action === "revoked") {
        await expect(
          page.getByText("Write access required", { exact: true }),
        ).toBeVisible();
      }
      if (action !== "publish") {
        await expect(merge).toContainText("Merging is blocked");
        await expect(button("Merge pull request")).toHaveCount(0);
        break;
      }

      const mergePath = `${api}/1/merge`;
      const devtools = await page.context().newCDPSession(page);
      let status;
      // Let Chromium send the real merge. Interrupt only the response,
      // after the server has reported the committed publication.
      await devtools.send("Fetch.enable", {
        patterns: [
          {
            urlPattern: `${origin.origin}${mergePath}*`,
            requestStage: "Response",
          },
        ],
      });
      devtools.on(
        "Fetch.requestPaused",
        ({ requestId, responseStatusCode }) => {
          status = responseStatusCode;
          void devtools.send("Fetch.failRequest", {
            requestId,
            errorReason: "Failed",
          });
        },
      );
      await button("Select merge method").click();
      await page
        .getByRole("menuitemradio", { name: /^Fast-forward only/ })
        .click();
      await button("Merge pull request").click();
      const lost = page.waitForEvent("requestfailed", {
        predicate: (request) => post(request, mergePath),
        timeout: 45_000,
      });
      await button("Confirm merge").click();
      const submitted = (await lost).postDataJSON();
      assert.equal(status, 200);
      assert.equal(submitted.head_oid, head);
      assert.equal(submitted.method, "fast_forward");
      console.log(JSON.stringify(submitted));
      break;
    }
    case "merged":
      await expect(merge).toContainText("Pull request merged");
      await expect(
        merge.getByText(head.slice(0, 7), { exact: true }),
      ).toBeVisible();
      break;
    default:
      assert.fail(`Unknown qualification action ${action}`);
  }
} finally {
  await browser.close();
}
