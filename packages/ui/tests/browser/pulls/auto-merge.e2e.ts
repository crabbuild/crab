import { chooseSelectOption } from "../select";
import { expect, test } from "@playwright/test";
import { mockPullRequest } from "./fixture";

test("a waiting merge can be confirmed, recovered, and canceled", async ({
  page,
}) => {
  const fixture = await mockPullRequest(page);
  const pull = fixture.pull();
  let scheduled: {
    request_id: string;
    method: string;
    head_oid: string;
    scheduled_by: string;
    stopped: string | null;
  } | null = null;
  const writes: unknown[] = [];
  await page.route(
    (url) => url.pathname === "/api/repos/team/project/pulls/1",
    (route) =>
      route.fulfill({
        json: {
          ...pull,
          auto_merge_configured: true,
          can_merge: false,
          merge_methods: ["merge_commit", "squash"],
        },
      }),
  );
  await page.route(
    (url) => url.pathname === "/api/repos/team/project/pulls/1/auto-merge",
    async (route) => {
      const request = route.request();
      if (request.method() === "PUT") {
        const input = request.postDataJSON();
        writes.push(input);
        scheduled = {
          request_id: input.request_id,
          method: input.method,
          head_oid: input.head_oid,
          scheduled_by: "Alice",
          stopped: null,
        };
        if (writes.length === 1) {
          await route.fulfill({
            status: 200,
            contentType: "application/json",
            body: "",
          });
          return;
        }
      } else if (request.method() === "DELETE") {
        expect(request.postDataJSON()).toEqual({
          request_id: scheduled?.request_id,
        });
        if (scheduled) scheduled = { ...scheduled, stopped: "canceled" };
      }
      const active = scheduled?.stopped === null;
      await route.fulfill({
        json: { can_enable: !active, can_disable: active, request: scheduled },
      });
    },
  );

  await page.goto("/team/project/pull/1");
  const panel = page.getByRole("region", { name: "Auto-merge", exact: true });
  await panel.getByRole("button", { name: "Enable auto-merge" }).click();
  const confirm = page.getByRole("dialog", { name: "Enable auto-merge" });
  await chooseSelectOption(
    confirm.getByRole("combobox", { name: "Merge method" }),
    "squash",
  );
  await confirm
    .getByRole("textbox", { name: "Commit message" })
    .fill("Reviewed changes");
  const submit = confirm.getByRole("button", { name: "Confirm auto-merge" });
  await submit.click();
  await expect(confirm.getByRole("alert")).toContainText("The reply was lost");
  await expect(
    confirm.getByRole("textbox", { name: "Commit message" }),
  ).toBeDisabled();
  await submit.click();
  await expect(panel).toContainText("Auto-merge enabled");
  expect(writes[1]).toEqual(writes[0]);
  expect(writes[0]).toMatchObject({
    method: "squash",
    message: "Reviewed changes",
    version: pull.version,
    base_oid: pull.base_oid,
    head_oid: pull.head_oid,
  });
  await panel.getByRole("button", { name: "Disable auto-merge" }).click();
  await expect(panel.getByRole("status")).toContainText(
    "Auto-merge was disabled for this pull request",
  );
});

test("queue submission replays a lost reply and allocates a new request on re-entry", async ({
  page,
}) => {
  const fixture = await mockPullRequest(page);
  const pull = fixture.pull();
  const root = "/api/repos/team/project/pulls/1";
  const writes: Array<{ request_id: string }> = [];
  let active = false;
  let grouped = false;
  await page.route(
    (url) => url.pathname === root,
    (route) =>
      route.fulfill({
        json: { ...pull, auto_merge_configured: true, merge_queued: active },
      }),
  );
  await page.route(
    (url) => url.pathname === `${root}/auto-merge`,
    (route) => {
      const request = route.request();
      if (request.method() === "PUT") {
        writes.push(request.postDataJSON());
        active = true;
        if (writes.length === 1) {
          return route.fulfill({
            status: 200,
            contentType: "application/json",
            body: "",
          });
        }
      } else if (request.method() === "DELETE") {
        active = false;
      }
      return route.fulfill({
        json: {
          can_enable: false,
          can_enqueue: !active,
          queue_method: "rebase",
          can_disable: active,
          request: writes.length
            ? {
                request_id: writes.at(-1)?.request_id,
                method: "rebase",
                head_oid: pull.head_oid,
                scheduled_by: "Alice",
                stopped: active ? null : "canceled",
                queue: true,
                first: false,
                testing: grouped,
              }
            : null,
        },
      });
    },
  );

  await page.goto("/team/project/pull/1");
  const enqueue = page.getByRole("button", { name: "Add to merge queue" });
  await enqueue.click();
  const dialog = page.getByRole("dialog", { name: "Add to merge queue" });
  await expect(dialog).toContainText("using a rebase merge");
  const confirm = dialog.getByRole("button", {
    name: "Confirm merge queue entry",
  });
  await confirm.click();
  await expect(dialog.getByRole("alert")).toContainText("The reply was lost");
  await confirm.click();
  const queue = page.getByRole("region", { name: "Merge queue" });
  await expect(queue).toContainText("Waiting in the merge queue");
  expect(writes[1]).toEqual(writes[0]);
  expect(writes[0]).toMatchObject({ queue: true, method: "rebase" });
  grouped = true;
  await expect(queue).toContainText("Testing with earlier queue entries");
  await expect(
    queue.getByRole("link", { name: "Candidate checks" }),
  ).toBeVisible();
  grouped = false;
  await queue.getByRole("button", { name: "Remove from merge queue" }).click();
  await expect(queue.getByRole("status")).toContainText(
    "Removed from the merge queue",
  );
  await enqueue.click();
  await confirm.click();
  await expect(queue).toContainText("Waiting in the merge queue");
  expect(writes[2]).toEqual({
    ...writes[0],
    request_id: expect.any(String),
  });
  expect(writes[2].request_id).not.toBe(writes[0].request_id);
});

test("merge when ready waits for requirements before showing a queue position", async ({
  page,
}) => {
  const fixture = await mockPullRequest(page);
  const pull = fixture.pull();
  let active = false;
  let queued = false;
  let canceled = false;
  let requestId: string | undefined;
  await page.route(
    (url) => url.pathname === "/api/repos/team/project/pulls/1",
    (route) =>
      route.fulfill({
        json: {
          ...pull,
          merge_queue_required: true,
          merge_queue_requested: active,
          merge_queued: queued,
          can_merge: false,
        },
      }),
  );
  await page.route(
    (url) => url.pathname === "/api/repos/team/project/pulls/1/auto-merge",
    (route) => {
      const request = route.request();
      if (request.method() === "PUT") {
        const input = request.postDataJSON();
        expect(input).toEqual({
          request_id: expect.any(String),
          queue: true,
          when_ready: true,
          method: "squash",
          version: pull.version,
          base_oid: pull.base_oid,
          head_oid: pull.head_oid,
        });
        if (canceled) expect(input.request_id).not.toBe(requestId);
        requestId = input.request_id;
        canceled = false;
        active = true;
      } else if (request.method() === "DELETE") {
        expect(request.postDataJSON()).toEqual({ request_id: requestId });
        canceled = true;
        active = false;
      }
      return route.fulfill({
        json: {
          can_enable: false,
          can_enqueue: false,
          can_wait_for_queue: !active,
          queue_method: "squash",
          can_disable: active,
          request: requestId
            ? {
                request_id: requestId,
                method: "squash",
                head_oid: pull.head_oid,
                scheduled_by: "Alice",
                stopped: canceled ? "canceled" : null,
                waiting_for_queue: !queued,
                queue: queued,
              }
            : null,
        },
      });
    },
  );

  await page.goto("/team/project/pull/1");
  const start = page.getByRole("button", {
    name: "Merge when ready",
    exact: true,
  });
  const dialog = page.getByRole("dialog", { name: "Merge when ready" });
  const panel = page.getByRole("region", { name: "Merge queue", exact: true });
  const confirm = dialog.getByRole("button", {
    name: "Confirm merge when ready",
  });
  await start.click();
  await expect(dialog).toContainText(pull.head_oid.slice(0, 7));
  await confirm.click();
  await expect(panel).toContainText("Waiting for merge requirements");
  await expect(
    panel.getByRole("link", { name: "View merge queue" }),
  ).toHaveCount(0);
  await panel.getByRole("button", { name: "Cancel merge when ready" }).click();
  await expect(panel.getByRole("status")).toContainText(
    "Merge when ready canceled",
  );
  await start.click();
  await confirm.click();
  await expect(panel).toContainText("Waiting for merge requirements");
  queued = true;
  await expect(panel).toContainText("Waiting in the merge queue");
  await expect(
    panel.getByRole("link", { name: "View merge queue" }),
  ).toBeVisible();
});
