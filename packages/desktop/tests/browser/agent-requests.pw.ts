import { test, expect } from "@playwright/test";

// Dedicated component qualification against an owned Vite server. Chrome uses
// an isolated browser profile; no desktop sidecar or user workspace is opened.
test.beforeEach(async ({ page }) => {
  page.on("pageerror", error => console.error("Renderer error:", error.message));
  await page.goto("/tests/fixtures/agent-requests.html");
});

test("a rejected turn Undo shows failure and stops before editing another file", async ({ page }) => {
  await page.getByRole("region", { name: "Undo failure fixture" }).getByRole("button", { name: "Undo", exact: true }).click();
  await expect(page.getByText("Undo failed", { exact: true })).toBeVisible();
  await expect(page.getByText("File has changed since the edit", { exact: true })).toBeVisible();
  await expect(page.getByText("File edits undone", { exact: true })).toHaveCount(0);
  expect(await page.evaluate(() => window.requestCalls)).toEqual([{ kind: "revert", taskId: "fixture-task", requestId: "second.ts" }]);
});

test("creates a first goal, preserves a rejected draft, and projects native accounting and controls", async ({ page }) => {
  const goal = page.getByRole("region", { name: "Codex goal" });
  await goal.getByRole("button", { name: "New goal" }).click();
  await goal.getByRole("textbox", { name: "Goal objective" }).fill("Write the selected result");
  await goal.getByRole("spinbutton", { name: "Goal token budget" }).fill("1200");
  await page.evaluate(() => { window.goalFixture.fail = true; });
  await goal.getByRole("button", { name: "Start goal", exact: true }).click();
  await expect(goal.getByRole("alert")).toContainText("Native goal unavailable");
  await expect(goal.getByRole("textbox", { name: "Goal objective" })).toHaveValue("Write the selected result");
  await goal.getByRole("button", { name: "Start goal", exact: true }).click();
  await expect(goal.getByRole("status", { name: "Goal status" })).toHaveText("Active");
  await expect(goal.getByRole("textbox", { name: "Goal objective" })).toHaveCount(0);
  await goal.getByRole("button", { name: "Pause goal" }).click();
  await expect(goal.getByRole("status", { name: "Goal status" })).toHaveText("Paused");
  await goal.getByRole("button", { name: "Resume goal" }).click();
  await page.evaluate(() => {
    const current = window.goalFixture.goal;
    if (!current) throw new Error("Goal was not created");
    window.goalFixture.notify({ ...current, status: "budgetLimited", tokensUsed: 1201 });
  });
  await expect(goal.getByRole("status", { name: "Goal status" })).toHaveText("Budget limited");
  await expect(goal).toContainText("1,201 tokens / 1,200");
  await page.setViewportSize({ width: 390, height: 800 });
  await goal.screenshot({ path: "/tmp/crab-agent-contract/goal-controls-mobile.png" });
  await goal.getByRole("button", { name: "Clear goal" }).click();
  await expect(goal.getByRole("button", { name: "New goal" })).toBeVisible();
  expect(await page.evaluate(() => window.goalFixture.calls)).toEqual([
    { objective: "Write the selected result", status: "active", tokenBudget: 1200 },
    { objective: "Write the selected result", status: "active", tokenBudget: 1200 }, { status: "paused" }, { status: "active" }, null,
  ]);
});

test("restores an archived workspace without retrying inference and refreshes its path", async ({ page }) => {
  await page.goto("/tests/fixtures/agent-workspace.html");
  await page.getByRole("button", { name: "Restore", exact: true }).click();
  await expect.poll(() => page.evaluate(() => window.workspaceFixture.calls)).toEqual(["list", "restore", "list"]);
  await expect.poll(() => page.evaluate(() => window.workspaceFixture.modelPaths)).toContain("/fixture/restored");
  await expect(page.getByRole("button", { name: "Restore", exact: true })).toHaveCount(0);
});

test("consumes the preload's single payload for status, task switching, and budget notices", async ({ page }) => {
  await page.goto("/tests/fixtures/agent-workspace.html");
  await expect(page.getByRole("button", { name: "Restore", exact: true })).toBeVisible();
  await page.evaluate(() => window.workspaceFixture.status("interrupted"));
  await page.getByRole("button", { name: "Retry", exact: true }).click();
  await expect.poll(() => page.evaluate(() => window.workspaceFixture.calls)).toContain("retry");
  await expect(page.getByRole("button", { name: "Retry", exact: true })).toHaveCount(0);
  await page.evaluate(() => window.workspaceFixture.switched());
  await expect(page.getByText("Stopped run in Workspace fixture to start here.", { exact: true })).toBeVisible();
  await page.evaluate(() => window.workspaceFixture.budget());
  await expect(page.getByRole("alertdialog")).toContainText("Workspace fixture");
});

test("answers all questions on the pending request with choices, multiple selections, and masked text", async ({ page }) => {
  const send = page.getByRole("button", { name: "Send answers" });
  await expect(send).toBeDisabled();
  await page.getByRole("radio", { name: "Thorough" }).check();
  await page.getByRole("checkbox", { name: "Build", exact: true }).check();
  await page.getByRole("checkbox", { name: "Test", exact: true }).check();
  const secret = page.getByRole("textbox", { name: "Answer: Fixture value?" });
  await expect(secret).toHaveAttribute("type", "password");
  await secret.fill(" fixture-only ");
  await send.click();
  await expect(page.getByText("Question resolved", { exact: true })).toBeVisible();
  expect(await page.evaluate(() => window.requestCalls)).toEqual([{
    kind: "input", taskId: "fixture-task", requestId: "question-1",
    response: { kind: "questions", answers: { mode: ["Thorough"], checks: ["Build", "Test"], secret: [" fixture-only "] } },
  }]);
});

test("reports an archive failure without showing the task as archived", async ({ page }) => {
  await page.goto("/tests/fixtures/agent-workspace.html");
  await expect(page.getByRole("button", { name: "Restore", exact: true })).toBeVisible();
  await page.evaluate(() => { window.workspaceFixture.failArchive = true; window.workspaceFixture.status("applied"); });
  await page.getByRole("button", { name: "Archive", exact: true }).click();
  await expect(page.getByText("Could not archive task", { exact: true })).toBeVisible();
  await expect(page.getByText("Snapshot unavailable", { exact: true })).toBeVisible();
  await expect(page.getByRole("button", { name: "Restore", exact: true })).toHaveCount(0);
  expect(await page.evaluate(() => window.workspaceFixture.calls)).toEqual(["list", "archive"]);
});

test("reopens applied work through restoration without offering a Git undo", async ({ page }) => {
  await page.goto("/tests/fixtures/agent-workspace.html");
  await expect(page.getByRole("button", { name: "Restore", exact: true })).toBeVisible();
  await page.evaluate(() => window.workspaceFixture.status("applied"));
  await page.getByRole("button", { name: "Reopen task", exact: true }).click();
  await expect.poll(() => page.evaluate(() => window.workspaceFixture.calls)).toEqual(["list", "restore", "list"]);
  await expect(page.getByRole("button", { name: "Reopen task", exact: true })).toHaveCount(0);
});

test("retains a failed discard and shows discarded history as read-only after success", async ({ page }) => {
  await page.goto("/tests/fixtures/agent-workspace.html");
  await expect(page.getByRole("button", { name: "Restore", exact: true })).toBeVisible();
  await page.evaluate(() => { window.workspaceFixture.failDiscard = true; window.workspaceFixture.status("stopped"); });
  await page.getByRole("button", { name: "Discard", exact: true }).click();
  await expect(page.getByText("Could not discard task", { exact: true })).toBeVisible();
  await expect(page.getByText("Workspace is locked", { exact: true })).toBeVisible();
  await expect(page.getByRole("button", { name: "Discard", exact: true })).toBeVisible();
  await page.evaluate(() => { window.workspaceFixture.failDiscard = false; });
  await page.getByRole("button", { name: "Discard", exact: true }).click();
  await expect(page.getByText("This conversation was discarded. Start a new chat to continue.", { exact: true })).toBeVisible();
  await expect(page.getByRole("textbox", { name: "Chat message input", exact: true })).toBeDisabled();
  expect(await page.evaluate(() => window.workspaceFixture.calls)).toEqual(["list", "discard", "discard", "list"]);
});

test("keeps a draft while archived and enables it after restoration", async ({ page }) => {
  await page.goto("/tests/fixtures/agent-workspace.html");
  const input = page.getByRole("textbox", { name: "Chat message input", exact: true });
  await expect(input).toBeDisabled();
  await page.getByRole("button", { name: "Restore", exact: true }).click();
  await expect(input).toBeEnabled();
  await input.fill("Keep this draft");
  await page.evaluate(() => window.workspaceFixture.status("archived"));
  await expect(input).toBeDisabled();
  await page.getByRole("button", { name: "Restore", exact: true }).click();
  await expect(input).toBeEnabled();
  await expect(input).toHaveValue("Keep this draft");
});

test("keeps a background question visible after turn completion and disables it when its connection closes", async ({ page }) => {
  const transcript = page.getByRole("region", { name: "Durable transcript" });
  await transcript.getByRole("button", { name: "Show background request" }).click();
  await expect(transcript.getByRole("radio", { name: "Continue", exact: true })).toBeVisible();
  await expect(transcript.getByRole("button", { name: /Worked for/ })).toHaveAttribute("aria-expanded", "true");
  await expect(transcript.getByRole("button", { name: /Worked for/ })).toBeDisabled();
  await expect(transcript.getByRole("button", { name: "Send answers" })).toBeVisible();
  await transcript.getByRole("button", { name: "Close background connection" }).click();
  await expect(transcript.getByText("Connection closed", { exact: true })).toBeVisible();
  await expect(transcript.getByRole("button", { name: "Send answers" })).toHaveCount(0);
});

test("shows the terminal failure without requiring expansion of a completed tool turn", async ({ page }) => {
  const transcript = page.getByRole("region", { name: "Durable transcript" });
  await transcript.getByRole("button", { name: "Show failed turn" }).click();
  await expect(transcript.getByText("Provider failed fixture", { exact: true })).toBeVisible();
});

test("renders distinct provider blocks separately even when their text is identical", async ({ page }) => {
  const transcript = page.getByRole("region", { name: "Durable transcript" });
  await transcript.getByRole("button", { name: "Show separate blocks" }).click();
  await expect(transcript.locator(".prose")).toHaveText(["Repeated", "Repeated"]);
});

test("keeps a newer chat patch when its history snapshot arrives later", async ({ page }) => {
  await page.goto("/tests/fixtures/agent-requests.html?slowHistory");
  await page.waitForFunction(() => Boolean(window.resolveChat && window.updateChat));
  await page.evaluate(() => window.updateChat?.({ taskId: "projection-task", revision: 2, upsert: [
    { id: "reply", role: "agent", kind: "text", content: "Final provider reply", timestamp: 2, sequence: 2 },
  ] }));
  const transcript = page.getByRole("region", { name: "Durable transcript" });
  await expect(transcript.getByText("Final provider reply", { exact: true })).toBeVisible();
  await page.evaluate(() => window.resolveChat?.({ success: true, revision: 1, messages: [
    { id: "initial", role: "user", content: "Restored user prompt", timestamp: 1, sequence: 1 },
    { id: "reply", role: "agent", kind: "text", content: "Partial provider reply", timestamp: 2, sequence: 2 },
  ] }));
  await expect(transcript.getByText("Restored user prompt", { exact: true })).toBeVisible();
  await expect(transcript.locator(".prose")).toHaveText(["Final provider reply"]);
});

test("shows chat failures from a reopened snapshot and from the active provider", async ({ page }) => {
  await page.goto("/tests/fixtures/agent-requests.html?slowHistory");
  await page.waitForFunction(() => Boolean(window.resolveChat && window.failChat));
  await page.evaluate(() => window.resolveChat?.({ success: true, messages: [], revision: 0, persistenceError: "Could not save chat: fixture disk full" }));
  await expect(page.getByRole("alert")).toContainText("Chat history incomplete");
  await expect(page.getByRole("alert")).toContainText("fixture disk full");
  await page.getByRole("button", { name: "Dismiss notification" }).click();
  await page.evaluate(() => window.failChat?.({ taskId: "projection-task", message: "Fixture provider disconnected" }));
  await expect(page.getByRole("alert")).toContainText("Fixture provider disconnected");
});

test("removes a rejected pending send even when its older snapshot arrives later", async ({ page }) => {
  await page.goto("/tests/fixtures/agent-requests.html?slowHistory");
  await page.waitForFunction(() => Boolean(window.resolveChat && window.updateChat));
  await page.evaluate(() => {
    window.updateChat?.({ taskId: "projection-task", revision: 3, upsert: [], deleted: ["rejected"] });
    window.resolveChat?.({ success: true, revision: 2, messages: [
      { id: "initial", role: "user", content: "Restored prompt", timestamp: 1, sequence: 1 },
      { id: "rejected", role: "user", content: "Rejected intent", pendingDelivery: true, timestamp: 2, sequence: 2 },
    ] });
  });
  const transcript = page.getByRole("region", { name: "Durable transcript" });
  await expect(transcript.getByText("Restored prompt", { exact: true })).toBeVisible();
  await expect(transcript.getByText("Rejected intent", { exact: true })).toHaveCount(0);
});

test("keeps an unconfirmed send visible when the provider has already completed", async ({ page }) => {
  await page.goto("/tests/fixtures/agent-requests.html?slowHistory");
  await page.waitForFunction(() => Boolean(window.resolveChat));
  await page.evaluate(() => window.resolveChat?.({ success: true, revision: 1, messages: [
    { id: "initial", role: "user", content: "Initial", timestamp: 1 },
    { id: "uncertain", role: "user", content: "Possibly accepted", deliveryError: "Message delivery could not be confirmed", timestamp: 2 },
    { id: "tool", role: "agent", kind: "tool", events: [{ type: "file_read", tool: "Read", summary: "Read fixture.txt" }], content: "", timestamp: 3 },
    { id: "reply", role: "agent", kind: "text", content: "Provider acted", timestamp: 4 },
    { id: "done", role: "agent", kind: "done", outcome: "completed", content: "", timestamp: 5 },
  ] }));
  const transcript = page.getByRole("region", { name: "Durable transcript" });
  await expect(transcript.getByText("Possibly accepted", { exact: true })).toBeVisible();
  await expect(transcript.getByText("Message delivery could not be confirmed. Check the conversation before sending it again.", { exact: true })).toBeVisible();
  await expect(transcript.getByRole("button", { name: /Worked for/ })).toHaveAttribute("aria-expanded", "true");
});

test("shows a failed submission and sends the provider's offered decision when retried", async ({ page }) => {
  await page.getByRole("button", { name: "Fail next response" }).click();
  await page.getByRole("button", { name: "Allow once", exact: true }).click();
  await expect(page.getByRole("alert")).toHaveText("Connection unavailable");
  await page.getByRole("button", { name: "Block network access to example.invalid" }).click();
  await expect(page.getByRole("button", { name: "Allow once", exact: true })).toHaveCount(0);
  expect((await page.evaluate(() => window.requestCalls)).map(call => call.decision)).toEqual(["allow_once", "network:1"]);
});

test("disables requests resolved by the provider without submitting another turn", async ({ page }) => {
  await page.getByRole("button", { name: "Resolve from provider" }).click();
  await expect(page.getByRole("button", { name: "Send answers" })).toHaveCount(0);
  await expect(page.getByRole("button", { name: "Allow once", exact: true })).toHaveCount(0);
  expect(await page.evaluate(() => window.requestCalls)).toEqual([]);
});

test("retains repeated text and distinct items while replacing the completed block", async ({ page }) => {
  const replies = page.getByRole("region", { name: "Streamed replies" });
  await replies.getByRole("button", { name: "Stream text" }).click();
  await expect(replies.locator("p")).toHaveText(["haha", "haha"]);
  await replies.getByRole("button", { name: "Complete text" }).click();
  await expect(replies.locator("p")).toHaveText(["haha!", "haha"]);
});

test("shows tokens and an unavailable dollar estimate when the provider reports no cost", async ({ page }) => {
  const costs = page.getByLabel("Cost detail panel");
  await expect(costs.getByText("Unavailable", { exact: true })).toHaveCount(2);
  await expect(costs.getByRole("table").getByRole("row").nth(1)).toHaveText(/15.*5.*Unavailable/);
  await expect(costs.getByText("Cumulative Cost", { exact: true })).toHaveCount(0);
});

test("uses the provider's discovered models and native effort value in a new chat", async ({ page }) => {
  const models = page.getByRole("region", { name: "Provider models" });
  await models.getByRole("button", { name: "Select model and intelligence" }).click();
  await models.getByRole("button", { name: "Runtime Model", exact: true }).click();
  await models.getByRole("button", { name: "Select model and intelligence" }).click();
  await models.getByRole("button", { name: "Provider-level", exact: true }).click();
  await models.getByPlaceholder("Ask anything. / for commands, @ for context, # for files…").fill("Fixture prompt");
  await models.getByRole("button", { name: "Send message" }).click();
  expect(await page.evaluate(() => window.modelSubmission)).toEqual({ model: "runtime-model", intelligence: "provider-level" });
});

test("shows discovery failure without presenting a static model list", async ({ page }) => {
  await page.goto("/tests/fixtures/agent-requests.html?failModels");
  const models = page.getByRole("region", { name: "Provider models" });
  await expect(models.getByRole("status")).toHaveText("Could not load models: Provider catalog unavailable");
  await expect(models.getByRole("button", { name: "Select model and intelligence" })).toHaveCount(0);
});

test("opens the child's own approval, reflects turn completion, and opens workspace changes", async ({ page }) => {
  const children = page.getByRole("region", { name: "Child conversations" });
  await expect(children.getByRole("status")).toHaveText(["reviewing", "running"]);
  await children.getByRole("button", { name: "Open reviewer chat" }).click();
  await children.getByRole("button", { name: "Allow child once" }).click();
  expect(await page.evaluate(() => window.requestCalls)).toEqual([
    { kind: "approval", taskId: "child", requestId: "child-approval", decision: "allow_once", summary: "Child permission" },
  ]);
  await children.getByRole("button", { name: "Complete child turn" }).click();
  await expect(children.getByRole("status")).toHaveText(["reviewing", "reviewing"]);
  await children.getByRole("button", { name: "Review changes" }).click();
  await expect(children.getByText("Workspace changes", { exact: true })).toBeVisible();
  await children.getByRole("button", { name: "Fail child turn" }).click();
  await expect(children.getByRole("status")).toHaveText(["reviewing", "failed"]);
  await page.setViewportSize({ width: 390, height: 800 });
  await expect(children.getByRole("button", { name: "Open reviewer chat" })).toBeVisible();
  await children.screenshot({ path: "/tmp/crab-agent-contract/child-conversations-mobile.png" });
});

test("cancels model discovery, ignores its late result, and retries with a new request", async ({ page }) => {
  await page.goto("/tests/fixtures/agent-requests.html?slowModels");
  const models = page.getByRole("region", { name: "Provider models" });
  await models.getByRole("button", { name: "Cancel model discovery" }).click();
  await models.getByRole("button", { name: "Complete model discovery" }).click();
  await expect(models.getByRole("status")).toHaveText("Model discovery cancelled");
  await models.getByRole("button", { name: "Retry model discovery" }).click();
  await expect(models.getByRole("status")).toHaveText("Loading models…");
  await models.getByRole("button", { name: "Complete model discovery" }).click();
  await expect(models.getByRole("status")).toHaveCount(0);
  const queries = await page.evaluate(() => window.modelQueries);
  expect({ cancelled: queries.map(query => query.cancelled), unique: new Set(queries.map(query => query.id)).size }).toEqual({ cancelled: [true, false], unique: 2 });
});

test("cancels pending discovery when the model picker unmounts", async ({ page }) => {
  await page.goto("/tests/fixtures/agent-requests.html?slowModels");
  const models = page.getByRole("region", { name: "Provider models" });
  await expect(models.getByRole("status")).toHaveText("Loading models…");
  await models.getByRole("button", { name: "Hide model picker" }).click();
  expect(await page.evaluate(() => window.modelQueries.map(query => query.cancelled))).toEqual([true]);
});

test("submits native mode and effort from an active chat without rewriting the prompt", async ({ page }) => {
  const chat = page.getByRole("region", { name: "Active chat controls" });
  await chat.getByRole("button", { name: "Select model and intelligence" }).click();
  await chat.getByRole("button", { name: "Provider-level", exact: true }).click();
  await chat.getByRole("button", { name: "Instant", exact: true }).click();
  await chat.getByPlaceholder("Send feedback, / for commands, @ for context…").fill("Preserve this prompt");
  await chat.getByRole("button", { name: "Send message" }).click();
  expect(await page.evaluate(() => window.turnSubmission)).toMatchObject({ text: "Preserve this prompt", model: "runtime-model", mode: "plan", intelligence: "provider-level" });
});

test("clears an active model override when the user selects the agent default", async ({ page }) => {
  const chat = page.getByRole("region", { name: "Active chat controls" });
  await chat.getByRole("button", { name: "Select model and intelligence" }).click();
  await chat.getByRole("button", { name: "Agent default", exact: true }).click();
  await chat.getByPlaceholder("Send feedback, / for commands, @ for context…").fill("Use the default");
  await chat.getByRole("button", { name: "Send message" }).click();
  expect(await page.evaluate(() => window.turnSubmission)).toMatchObject({ text: "Use the default", model: null, mode: "instant", intelligence: null });
});

test("sends typed MCP form values and preserves input after a failed submission", async ({ page }) => {
  const form = page.getByRole("region", { name: "MCP form" });
  await expect(form.getByLabel("Count *", { exact: true })).toHaveValue("0");
  await expect(form.getByLabel("Include reports (optional)", { exact: true })).toHaveValue("false");
  await form.getByLabel("Review mode *", { exact: true }).selectOption({ label: "Thorough review" });
  await form.getByRole("checkbox", { name: "Build project" }).check();
  await page.getByRole("button", { name: "Fail next response" }).click();
  await form.getByRole("button", { name: "Send form" }).click();
  await expect(form.getByRole("alert")).toHaveText("Could not send the response");
  await form.getByRole("button", { name: "Send form" }).click();
  await expect(form.getByText("Fixture MCP: request resolved", { exact: true })).toBeVisible();
  expect((await page.evaluate(() => window.requestCalls)).map(call => call.response)).toEqual([
    { kind: "elicitation", action: "accept", content: { count: 0, enabled: false, mode: "thorough", checks: ["build"], note: "" } },
    { kind: "elicitation", action: "accept", content: { count: 0, enabled: false, mode: "thorough", checks: ["build"], note: "" } },
  ]);
});

for (const action of ["Decline", "Cancel"] as const) {
  test(`returns the distinct MCP ${action.toLowerCase()} action without opening the URL`, async ({ page }) => {
    const link = page.getByRole("region", { name: "MCP URL" });
    await link.getByRole("button", { name: action, exact: true }).click();
    expect({ calls: await page.evaluate(() => window.requestCalls), opened: await page.evaluate(() => window.openedUrls) }).toEqual({
      calls: [{ kind: "input", taskId: "fixture-task", requestId: "mcp-url-1", response: { kind: "elicitation", action: action.toLowerCase() } }], opened: [],
    });
  });
}

test("displays the full MCP URL before opening and confirms consent only after the user's click", async ({ page }) => {
  const link = page.getByRole("region", { name: "MCP URL" });
  await expect(link.getByText("https://auth.example.invalid/connect?flow=fixture", { exact: true })).toBeVisible();
  expect(await page.evaluate(() => window.openedUrls)).toEqual([]);
  await link.getByRole("button", { name: "Open auth.example.invalid", exact: true }).click();
  await expect(link.getByText("Fixture MCP: request resolved", { exact: true })).toBeVisible();
  expect({ calls: await page.evaluate(() => window.requestCalls), opened: await page.evaluate(() => window.openedUrls) }).toEqual({
    calls: [{ kind: "input", taskId: "fixture-task", requestId: "mcp-url-1", response: { kind: "elicitation", action: "accept" } }], opened: ["https://auth.example.invalid/connect?flow=fixture"],
  });
});
