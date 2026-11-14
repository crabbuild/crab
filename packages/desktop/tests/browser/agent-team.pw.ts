import { test, expect } from "@playwright/test";
import { readFile } from "node:fs/promises";
import path from "node:path";

test.beforeEach(async ({ page }) => { await page.goto("/tests/fixtures/agent-team.html"); });

test("opens complete child output, keeps the parent separate, and exposes team messages", async ({ page }) => {
  await expect(page.getByRole("region", { name: "Agent team" })).toContainText("4 agents · 3 working");
  await expect(page.getByRole("region", { name: "Agent team" }).getByRole("button", { name: /Lock contention/ })).toContainText("Failure paths / Regression check");
  await expect(page.getByText("FULL_TRANSCRIPT_END", { exact: false })).toHaveCount(0);
  await page.getByRole("button", { name: /Ownership review Codex/ }).click();
  const dialog = page.getByRole("dialog");
  await expect(dialog.getByRole("button", { name: /Lock contention/ })).toContainText("Failure paths / Regression check");
  await dialog.getByRole("button", { name: /Worked for/ }).click();
  await expect(dialog).toContainText("FULL_TRANSCRIPT_END");
  await expect(dialog.getByText("Instructions", { exact: true })).toBeVisible();
  await dialog.getByRole("tab", { name: /Messages/ }).click();
  await expect(dialog).toContainText("Please verify that cancellation waits");
  await expect(dialog).toContainText("Delivered");
  await dialog.getByRole("tab", { name: "Transcript", exact: true }).click();
  await dialog.getByRole("button", { name: "Failure paths Running", exact: true }).click();
  const input = dialog.getByRole("textbox", { name: "Message this agent" });
  await input.fill("Check the error path too");
  await page.evaluate(() => { window.teamFixture.fail = true; });
  await dialog.getByRole("button", { name: "Send message to agent" }).click();
  await expect(dialog.getByRole("alert")).toContainText("Agent connection lost");
  await expect(input).toHaveValue("Check the error path too");
  await page.evaluate(() => { window.teamFixture.fail = false; });
  await dialog.getByRole("button", { name: "Send message to agent" }).click();
  await expect(input).toHaveValue("");
  expect(await page.evaluate(() => window.teamFixture.calls)).toEqual([{ id: "investigator", text: "Check the error path too" }, { id: "investigator", text: "Check the error path too" }]);
});

test("shows long output on narrow screens and exports every byte", async ({ page }) => {
  await page.setViewportSize({ width: 390, height: 844 });
  await page.getByRole("button", { name: /Ownership review Codex/ }).click();
  const download = page.waitForEvent("download");
  await page.getByRole("button", { name: "Download full transcript" }).click();
  const file = await download;
  const saved = await file.path();
  expect(saved && await readFile(saved, "utf8")).toContain("FULL_TRANSCRIPT_END");
  expect(saved && await readFile(saved, "utf8")).toContain("COMMAND_SESSION_BYTES_END");
  await page.getByRole("tab", { name: "Full text", exact: true }).click();
  await expect(page.getByRole("tabpanel", { name: "Full text", exact: true })).toContainText("COMMAND_SESSION_BYTES_END");
  const dialog = page.getByRole("dialog");
  expect(await dialog.evaluate(element => element.scrollWidth <= element.clientWidth)).toBe(true);
});

test("renders saved real-provider output", async ({ page }) => {
  const root = process.env.CRAB_AGENT_LIVE_ROOT;
  test.skip(!root, "Requires the dedicated real-provider team qualification");
  for (const provider of ["codex", "claude-code"]) {
    const messages = JSON.parse(await readFile(path.join(root!, `${provider}-team-transcript.json`), "utf8"));
    await page.evaluate(messages => window.teamFixture.replace(messages), messages);
    await page.getByRole("region", { name: "Agent team" }).getByRole("button", { name: /Verifier/ }).click();
    await expect(page.getByRole("dialog")).toContainText("CHILD_RESULT_READY");
    await page.getByRole("button", { name: "Close", exact: true }).click();
  }
});

test("keeps the viewer open when the provider assigns a stable agent identity", async ({ page }) => {
  await page.evaluate(() => window.teamFixture.replace([{ id: "agent-row", role: "agent", content: "", timestamp: 1, kind: "subagent",
    subagent: { id: "spawn-call", name: "Starting reviewer", provider: "claude-code", status: "starting" }, messages: [] }]));
  await page.getByRole("region", { name: "Agent team" }).getByRole("button", { name: /Starting reviewer/ }).click();
  await page.evaluate(() => window.teamFixture.replace([{ id: "agent-row", role: "agent", content: "", timestamp: 2, kind: "subagent",
    subagent: { id: "stable-agent", toolCallId: "spawn-call", name: "Starting reviewer", provider: "claude-code", status: "running" },
    messages: [{ id: "reply", role: "agent", content: "Stable agent output", kind: "text", timestamp: 2 }] }]));
  await expect(page.getByRole("dialog")).toContainText("Stable agent output");
});
