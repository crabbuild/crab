#!/usr/bin/env node
// Run against an isolated local server; optionally pause after losing a real
// commit reply so a controller can restart, hand off, or restore its storage.
import assert from "node:assert/strict";
import { mkdir, readFile, rename, writeFile } from "node:fs/promises";
import { resolve, join } from "node:path";
import { parseArgs } from "node:util";
import { randomUUID } from "node:crypto";
import { chromium, expect } from "@playwright/test";

const { values } = parseArgs({
  options: {
    origin: { type: "string" },
    organization: { type: "string" },
    project: { type: "string" },
    artifacts: { type: "string" },
    resume: { type: "string" },
    "resume-timeout-seconds": { type: "string", default: "600" },
    loss: { type: "string", default: "reply" },
  },
});
assert(
  values.origin && values.organization && values.artifacts,
  "Required: --origin --organization --artifacts; optional --project --resume FILE",
);
const origin = new URL(values.origin);
assert(["request", "reply"].includes(values.loss));
assert(
  ["127.0.0.1", "localhost", "[::1]"].includes(origin.hostname),
  "Qualification requires an isolated loopback server",
);
const artifacts = resolve(values.artifacts);
await mkdir(artifacts, { recursive: true, mode: 0o700 });
const run = randomUUID();
const name = `Recovery qualification ${run.slice(0, 8)}`;
const title = `Retained submission ${run}`;
const root = `/api/organizations/${values.organization}/projects`;
const home = `/-/organizations/${values.organization}/projects`;
const browser = await chromium.launch();
const context = await browser.newContext({
  baseURL: origin.origin,
  viewport: { width: 1280, height: 900 },
});
const page = await context.newPage();
page.on("dialog", (dialog) =>
  dialog.type() === "beforeunload" ? dialog.accept() : dialog.dismiss(),
);
const submissions = [];
const responses = [];
page.on("request", (request) => {
  if (
    new URL(request.url()).pathname === `${root}/commands` &&
    request.method() === "POST"
  )
    submissions.push(request.postDataJSON());
});
page.on("response", async (response) => {
  if (
    new URL(response.url()).pathname === `${root}/commands` &&
    response.request().method() === "POST"
  ) {
    try {
      responses.push({
        request: response.request().postDataJSON(),
        outcome: await response.json(),
      });
    } catch {
      /* The deliberately lost response has no browser body. */
    }
  }
});
try {
  let project = values.project;
  if (!project) {
    await page.goto(home);
    await page
      .getByRole("button", { name: "New project", exact: true })
      .click();
    await page.getByLabel("Project name", { exact: true }).fill(name);
    await page
      .getByRole("button", { name: "Create project", exact: true })
      .click();
    await expect(page.getByRole("heading", { name, exact: true })).toBeVisible({
      timeout: 45_000,
    });
    project = new URL(page.url()).pathname.split("/").at(-1);
  } else await page.goto(`${home}/${project}`);
  assert.match(project, /^[0-9a-f-]{36}$/i);

  let captured;
  let lost = false;
  await page.route(
    (url) => url.pathname === `${root}/commands`,
    async (route) => {
      const input = route.request().postDataJSON();
      if (lost || input.command.kind !== "item") return route.continue();
      let outcome = null;
      if (values.loss === "reply") {
        const response = await route.fetch({ timeout: 45_000 });
        outcome = await response.json();
        assert.equal(response.status(), 200, JSON.stringify(outcome));
        assert.equal(outcome.outcome, "applied", JSON.stringify(outcome));
      }
      lost = true;
      captured = { input, outcome };
      await route.abort("failed");
    },
  );
  await page
    .getByRole("button", { name: "Add work", exact: true })
    .first()
    .click();
  const editor = page.getByRole("dialog", {
    name: "Add work to project",
    exact: true,
  });
  const draftTab = editor.getByRole("tab", { name: "Draft note", exact: true });
  if (await draftTab.count()) await draftTab.click();
  await editor.getByLabel("Title", { exact: true }).fill(title);
  await editor
    .getByLabel("Description", { exact: true })
    .fill("This exact draft and its original request must survive recovery.");
  await editor
    .getByRole("button", { name: "Add to project", exact: true })
    .click();
  await expect(
    editor.getByRole("button", { name: "Retry submission", exact: true }),
  ).toBeVisible({ timeout: 45_000 });
  assert(captured && lost);
  assert.equal(captured.input.command.project, project);
  const checkpoint = {
    run,
    organization: values.organization,
    project,
    title,
    loss: values.loss,
    ...captured,
  };
  const pendingPath = join(artifacts, "pending.json");
  await writeFile(`${pendingPath}.tmp`, JSON.stringify(checkpoint, null, 2), {
    mode: 0o600,
  });
  await rename(`${pendingPath}.tmp`, pendingPath);
  console.log(
    JSON.stringify({
      phase: "pending",
      project,
      request_id: captured.input.request_id,
    }),
  );
  await page.screenshot({
    path: join(artifacts, "pending.png"),
    fullPage: true,
  });

  if (values.resume) {
    const timeout = Number(values["resume-timeout-seconds"]);
    assert(Number.isSafeInteger(timeout) && timeout > 0 && timeout <= 7200);
    const deadline = Date.now() + timeout * 1000;
    let resumed = false;
    while (Date.now() < deadline) {
      try {
        const signal = JSON.parse(
          await readFile(resolve(values.resume), "utf8"),
        );
        if (signal.request_id === captured.input.request_id) {
          resumed = true;
          break;
        }
      } catch (error) {
        if (error.code !== "ENOENT" && !(error instanceof SyntaxError))
          throw error;
      }
      await new Promise((resolve) => setTimeout(resolve, 500));
    }
    assert(
      resumed,
      "Controller did not confirm recovery of the original request",
    );
  }

  await page.reload();
  await page
    .getByRole("button", { name: "Retry submission", exact: true })
    .click({ timeout: 45_000 });
  await expect(page.getByText(title, { exact: true })).toBeVisible({
    timeout: 45_000,
  });
  const attempts = submissions.filter(
    (input) => input.request_id === captured.input.request_id,
  );
  assert.equal(attempts.length, 2);
  assert.deepEqual(attempts[1], attempts[0]);
  const acknowledgement = () =>
    responses.find(
      (value) =>
        value.request.request_id === captured.input.request_id &&
        value.outcome.outcome === "applied",
    )?.outcome;
  await expect.poll(acknowledgement, { timeout: 10_000 }).toMatchObject({
    outcome: "applied",
    project,
  });
  const applied = acknowledgement();
  if (captured.outcome) assert.deepEqual(applied, captured.outcome);
  await expect(
    page.getByRole("button", { name: "Retry submission", exact: true }),
  ).toHaveCount(0);
  await expect(
    page.getByRole("button", { name: "Resume item draft", exact: true }),
  ).toHaveCount(0);

  const items = await (
    await page.request.get(`${root}/${project}/items`)
  ).json();
  assert.equal(
    items.page.items.filter(
      (row) => row.item.id === captured.input.command.item.id,
    ).length,
    1,
  );
  const events = [];
  let after = 0;
  for (;;) {
    const reply = await page.request.get(
      `${root}/${project}/history?after=${after}`,
    );
    assert.equal(reply.status(), 200);
    const history = await reply.json();
    events.push(...history.items);
    if (history.next === null) break;
    assert(history.next > after);
    after = history.next;
  }
  const affected = events.filter(
    (event) =>
      event.target.kind === "item" &&
      event.target.id === captured.input.command.item.id,
  );
  assert.equal(affected.length, 1);
  assert.equal(affected[0].version, applied.version);
  assert(affected[0].actor.id);
  assert(Number.isSafeInteger(affected[0].at_ms));
  assert.deepEqual(
    affected[0].changes.find((change) => change.field === "/title"),
    { field: "/title", before: null, after: title },
  );
  await page.getByRole("tab", { name: "History", exact: true }).click();
  const history = page.getByRole("region", {
    name: "Project history",
    exact: true,
  });
  await history
    .getByRole("button", { name: /^View \d+ field changes$/ })
    .first()
    .click();
  await expect(
    history.getByRole("cell", { name: title, exact: true }),
  ).toBeVisible();
  await page.screenshot({
    path: join(artifacts, "recovered-history.png"),
    fullPage: true,
  });
  await writeFile(
    join(artifacts, "verified.json"),
    JSON.stringify(
      {
        ...checkpoint,
        applied_outcome: applied,
        attempts,
        history: affected[0],
        verified: true,
      },
      null,
      2,
    ),
    { mode: 0o600 },
  );
  console.log(
    JSON.stringify({
      phase: "verified",
      project,
      request_id: captured.input.request_id,
      revision: applied.version,
    }),
  );
} catch (error) {
  await page
    .screenshot({ path: join(artifacts, "failure.png"), fullPage: true })
    .catch(() => {});
  throw error;
} finally {
  await browser.close();
}
