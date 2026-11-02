#!/usr/bin/env node
// The Rust HTTP harness supplies two real OIDC sessions and native Cell data.
import assert from "node:assert/strict";
import { mkdir, readFile, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { chromium, expect } from "@playwright/test";

const fixture = JSON.parse(await readFile(process.argv[2], "utf8"));
const origin = new URL(fixture.origin);
assert(["127.0.0.1", "localhost"].includes(origin.hostname));
assert.notEqual(fixture.alice.actor, fixture.bob.actor);
await mkdir(fixture.artifacts, { recursive: true, mode: 0o700 });
const browser = await chromium.launch();
const contexts = await Promise.all([
  browser.newContext({ baseURL: origin.origin }),
  browser.newContext({ baseURL: origin.origin }),
]);
async function session(context, cookie) {
  const separator = cookie.indexOf("=");
  assert(separator > 0);
  await context.addCookies([
    {
      name: cookie.slice(0, separator),
      value: cookie.slice(separator + 1),
      url: origin.origin,
      httpOnly: true,
      sameSite: "Lax",
    },
  ]);
}
await session(contexts[0], fixture.alice.cookie);
await session(contexts[1], fixture.bob.cookie);
const [alice, bob] = await Promise.all(
  contexts.map((context) => context.newPage()),
);
for (const page of [alice, bob]) {
  page.setDefaultTimeout(15_000);
  page.on("dialog", (dialog) =>
    dialog.type() === "beforeunload" ? dialog.accept() : dialog.dismiss(),
  );
}
const root = `/api/organizations/${fixture.organization}/projects`;
const home = `/-/organizations/${fixture.organization}/projects/${fixture.project}`;
async function rows(page) {
  const response = await page.request.get(`${root}/${fixture.project}/items`);
  assert.equal(response.status(), 200);
  return (await response.json()).page.items;
}
async function edit(page, title) {
  await page
    .getByRole("button", { name: `Edit ${title}`, exact: true })
    .click();
  return page.getByRole("dialog", { name: "Project item", exact: true });
}
try {
  await Promise.all([alice.goto(home), bob.goto(home)]);
  assert.equal(
    (await (await alice.request.get("/api/session")).json()).user.subject,
    "alice-id",
  );
  assert.equal(
    (await (await bob.request.get("/api/session")).json()).user.subject,
    "bob-id",
  );
  const first = await edit(alice, "First deliverable");
  const second = await edit(bob, "Second deliverable");
  await first
    .getByLabel("Description", { exact: true })
    .fill("Alice's independent edit");
  await second
    .getByLabel("Description", { exact: true })
    .fill("Bob's independent edit");
  await Promise.all([
    first.getByRole("button", { name: "Save item", exact: true }).click(),
    second.getByRole("button", { name: "Save item", exact: true }).click(),
  ]);
  await expect(first).toHaveCount(0, { timeout: 15_000 });
  await expect(second).toHaveCount(0, { timeout: 15_000 });
  let saved = await rows(alice);
  assert.deepEqual(
    saved.map((row) => [row.item.title, row.item.body, row.version]).sort(),
    [
      ["First deliverable", "Alice's independent edit", 2],
      ["Second deliverable", "Bob's independent edit", 2],
    ],
  );
  await Promise.all([alice.reload(), bob.reload()]);
  const winner = await edit(alice, "First deliverable");
  const draft = await edit(bob, "First deliverable");
  await expect(winner.getByLabel("Description", { exact: true })).toHaveValue(
    "Alice's independent edit",
  );
  await winner
    .getByLabel("Description", { exact: true })
    .fill("Alice's saved revision");
  await draft
    .getByLabel("Description", { exact: true })
    .fill("Bob's retained draft");
  await winner.getByRole("button", { name: "Save item", exact: true }).click();
  await expect(winner).toHaveCount(0, { timeout: 15_000 });
  await draft.getByRole("button", { name: "Save item", exact: true }).click();
  await expect(draft.getByRole("alert")).toContainText(
    "Your draft is preserved",
  );
  await expect(draft.getByLabel("Description", { exact: true })).toHaveValue(
    "Bob's retained draft",
  );

  // The tab keeps its storage while its authenticated account changes.
  await session(contexts[1], fixture.alice.cookie);
  await bob.reload();
  await expect(
    bob.getByRole("button", { name: "Resume item draft", exact: true }),
  ).toHaveCount(0);
  await session(contexts[1], fixture.bob.cookie);
  await bob.reload();
  await bob
    .getByRole("button", { name: "Resume item draft", exact: true })
    .click();
  const restored = bob.getByRole("dialog", {
    name: "Project item",
    exact: true,
  });
  await expect(restored.getByLabel("Description", { exact: true })).toHaveValue(
    "Bob's retained draft",
  );
  await restored
    .getByRole("button", { name: "Save item", exact: true })
    .click();
  await expect(restored.getByRole("alert")).toContainText(
    "Your draft is preserved",
  );
  await restored
    .getByRole("button", { name: "Review saved item", exact: true })
    .click();
  await expect(
    restored.getByRole("cell", { name: "Alice's saved revision", exact: true }),
  ).toBeVisible();
  await expect(
    restored.getByRole("cell", { name: "Bob's retained draft", exact: true }),
  ).toBeVisible();
  await bob.screenshot({
    path: join(fixture.artifacts, "conflict-comparison.png"),
    fullPage: true,
  });
  await restored
    .getByRole("button", {
      name: "Keep my draft against this revision",
      exact: true,
    })
    .click();

  const submissions = [];
  let captured;
  bob.on("request", (request) => {
    if (
      new URL(request.url()).pathname === `${root}/commands` &&
      request.method() === "POST"
    )
      submissions.push(request.postDataJSON());
  });
  await bob.route(
    (url) => url.pathname === `${root}/commands`,
    async (route) => {
      if (captured) return route.continue();
      const input = route.request().postDataJSON();
      const response = await route.fetch();
      assert.equal(response.status(), 200);
      const outcome = await response.json();
      assert.equal(outcome.outcome, "applied");
      captured = { input, outcome };
      await route.abort("failed");
    },
  );
  await restored
    .getByRole("button", { name: "Save item", exact: true })
    .click();
  await expect(
    restored.getByRole("button", { name: "Retry submission", exact: true }),
  ).toBeVisible();
  await bob.reload();
  await bob
    .getByRole("button", { name: "Retry submission", exact: true })
    .click();
  await expect(
    bob.getByRole("button", { name: "Retry submission", exact: true }),
  ).toHaveCount(0, { timeout: 15_000 });
  await expect(
    bob.getByRole("button", { name: "Resume item draft", exact: true }),
  ).toHaveCount(0);
  assert(captured);
  assert.equal(submissions.length, 2);
  assert.deepEqual(submissions[0], submissions[1]);
  saved = await rows(bob);
  assert.deepEqual(
    saved.map((row) => [row.item.title, row.item.body, row.version]).sort(),
    [
      ["First deliverable", "Bob's retained draft", 4],
      ["Second deliverable", "Bob's independent edit", 2],
    ],
  );
  const history = [];
  let after = 0;
  for (;;) {
    const response = await bob.request.get(
      `${root}/${fixture.project}/history?after=${after}`,
    );
    assert.equal(response.status(), 200);
    const page = await response.json();
    history.push(...page.items);
    if (page.next === null) break;
    assert(page.next > after);
    after = page.next;
  }
  const bodyChanges = history.flatMap((event) =>
    event.changes
      .filter((change) => change.field === "/body" && change.before !== null)
      .map((change) => ({
        actor: event.actor.id,
        before: change.before,
        after: change.after,
        item: event.target.id,
      })),
  );
  const firstId = saved.find((row) => row.item.title === "First deliverable")
    .item.id;
  assert.deepEqual(
    bodyChanges
      .filter((change) => change.item === firstId)
      .map(({ actor, before, after }) => ({ actor, before, after })),
    [
      {
        actor: fixture.alice.actor,
        before: "",
        after: "Alice's independent edit",
      },
      {
        actor: fixture.alice.actor,
        before: "Alice's independent edit",
        after: "Alice's saved revision",
      },
      {
        actor: fixture.bob.actor,
        before: "Alice's saved revision",
        after: "Bob's retained draft",
      },
    ],
  );
  assert.equal(
    bodyChanges.filter((change) => change.after === "Bob's retained draft")
      .length,
    1,
  );
  assert(
    history.every(
      (event) =>
        event.actor.id &&
        Number.isSafeInteger(event.at_ms) &&
        Number.isSafeInteger(event.version),
    ),
  );
  await bob.getByRole("tab", { name: "History", exact: true }).click();
  const region = bob.getByRole("region", {
    name: "Project history",
    exact: true,
  });
  await region
    .getByRole("button", { name: /^View \d+ field changes$/ })
    .first()
    .click();
  await expect(
    region.getByRole("cell", { name: "Bob's retained draft", exact: true }),
  ).toBeVisible();
  await expect(
    region.getByText(`Member ${fixture.bob.actor}`, { exact: true }),
  ).toHaveCount(
    history.filter((event) => event.actor.id === fixture.bob.actor).length,
  );
  await expect(
    region.getByText(`Member ${fixture.bob.actor}`, { exact: true }).first(),
  ).toBeVisible();
  await bob.screenshot({
    path: join(fixture.artifacts, "attributed-history.png"),
    fullPage: true,
  });
  await writeFile(
    join(fixture.artifacts, "verified.json"),
    JSON.stringify(
      {
        project: fixture.project,
        actors: [fixture.alice.actor, fixture.bob.actor],
        captured,
        submissions,
        history,
        verified: true,
      },
      null,
      2,
    ),
    { mode: 0o600 },
  );
  console.log(
    JSON.stringify({
      verified: true,
      project: fixture.project,
      history_events: history.length,
    }),
  );
} catch (error) {
  await bob
    .screenshot({
      path: join(fixture.artifacts, "failure.png"),
      fullPage: true,
    })
    .catch(() => {});
  throw error;
} finally {
  await browser.close();
}
