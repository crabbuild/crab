import { chooseSelect } from "../helpers/select";
import { expect, type Page } from "@playwright/test";
import { readFileSync, unlinkSync, writeFileSync } from "node:fs";
import path from "node:path";
import type { ResultsResponse } from "../../src/lib/results-contracts";
import { call } from "./packaged-app";
import type { resultsStorageProxy } from "./results-storage-proxy";

export async function checkStorageFailures({
  page,
  handle,
  reportId,
  storage,
  output,
  recoverLocal,
}: {
  page: Page;
  handle: Record<string, unknown>;
  reportId: string;
  storage: Awaited<ReturnType<typeof resultsStorageProxy>>;
  output: string;
  recoverLocal: () => Promise<void>;
}) {
  const workspace = page.getByLabel("Results workspace", { exact: true });
  const response = (await call(page, "workflow_results_read", {
    repo_id: handle.repo_id,
    native_session_id: handle.native_session_id,
    request: { kind: "report", args: { id: reportId } },
  })) as unknown as ResultsResponse;
  if (response.kind !== "report")
    throw new Error("Expected actual remote report");
  const attachment =
    response.data.report.comparison.rows[0].run.snapshot.attachments.find(
      (attachment) =>
        attachment.kind === "plot" && attachment.path === "curve.csv",
    );
  if (!attachment?.hash) throw new Error("Actual report has no retained curve");
  const preview = workspace.getByRole("button", {
    name: "plot · curve.csv · present",
    exact: true,
  });
  const values = workspace.getByLabel("Metric comparison", { exact: true });
  storage.hold(attachment.hash);
  await preview.click();
  await expect.poll(() => storage.held).toBe(1);
  expect(storage.observed).toEqual({ method: "GET", status: 200 });
  expect(storage.readers.active).toBeGreaterThan(0);
  await expect(workspace.getByRole("alert")).toHaveCount(0);
  await expect(values).toContainText("0.1");
  storage.disconnect();
  await expect(workspace.getByRole("alert")).toContainText(
    "workflow Results operation failed",
    { timeout: 180_000 },
  );
  await expect(page.getByLabel("Report ID")).toHaveText(reportId);
  await expect(values).toContainText("0.2");
  await expect(workspace.locator("g.mark-line path")).toHaveCount(4);
  const viewport = await page.evaluate(() => ({
    width: innerWidth,
    height: innerHeight,
  }));
  await page.setViewportSize({ width: 1280, height: 1000 });
  await workspace.locator(":scope > .overflow-auto").evaluate((element) => {
    element.scrollTop = 0;
  });
  await page.screenshot({
    path: path.join(output, "native-results-storage-disconnected.png"),
    fullPage: true,
  });
  await page.setViewportSize({ width: 900, height: 1000 });
  await expect(
    workspace
      .getByRole("alert")
      .getByRole("button", { name: "Refresh view", exact: true }),
  ).toBeInViewport({ ratio: 1 });
  await page.screenshot({
    path: path.join(output, "native-results-storage-disconnected-narrow.png"),
    fullPage: true,
  });
  await page.setViewportSize(viewport);
  storage.resume();
  await preview.click();
  await expect(workspace.getByRole("alert")).toHaveCount(0);
  await expect(workspace.locator("pre")).toContainText("step,loss");

  storage.hold(attachment.hash);
  await preview.click();
  await expect.poll(() => storage.held).toBe(1);
  expect(storage.observed).toEqual({ method: "GET", status: 200 });
  expect(storage.readers.active).toBeGreaterThan(0);
  await call(page, "close", { repo_id: handle.repo_id });
  await expect.poll(() => storage.held).toBe(0);
  storage.resume();
  await recoverLocal();
  await expect(workspace.getByRole("alert")).toHaveCount(0);
  await expect(values).toContainText("0.1");
  await expect(values).toContainText("0.2");
  await expect(workspace.locator("g.mark-line path")).toHaveCount(4);
  await expect(workspace.locator("pre")).toHaveCount(0);
  await page.setViewportSize({ width: 1280, height: 1000 });
  await workspace.locator(":scope > .overflow-auto").evaluate((element) => {
    element.scrollTop = 0;
  });
  await page.screenshot({
    path: path.join(output, "native-results-inflight-session-recovery.png"),
    fullPage: true,
  });
  await page.setViewportSize(viewport);
  await expect.poll(() => storage.readers.active).toBe(0);
  expect(storage.readers.started).toBe(storage.readers.finished);
  const resourceFile = path.join(output, "native-resource-envelope.json");
  const resources = JSON.parse(readFileSync(resourceFile, "utf8"));
  resources.after_cancellation = storage.readers;
  writeFileSync(resourceFile, JSON.stringify(resources, null, 2));
}

export async function checkResultsFailures({
  page,
  source,
  cold,
  reportId,
  output,
  open,
  checkReport,
}: {
  page: Page;
  source: string;
  cold: string;
  reportId: string;
  output: string;
  open: (page: Page, directory: string) => Promise<Record<string, unknown>>;
  checkReport: (page: Page, id: string) => Promise<void>;
}) {
  const handle = await open(page, cold);
  await checkReport(page, reportId);
  const workspace = page.getByLabel("Results workspace", { exact: true });
  const response = (await call(page, "workflow_results_read", {
    repo_id: handle.repo_id,
    native_session_id: handle.native_session_id,
    request: { kind: "report", args: { id: reportId } },
  })) as unknown as ResultsResponse;
  expect(response.kind).toBe("report");
  if (response.kind !== "report") throw new Error("Expected retained report");
  const plot =
    response.data.report.comparison.rows[0].run.snapshot.attachments.find(
      (attachment) =>
        attachment.path === "curve.csv" && attachment.kind === "plot",
    );
  if (!plot?.hash) throw new Error("Actual run has no retained curve");
  const object = path.join(cold, ".crab/workflow/results/objects", plot.hash);
  const bytes = readFileSync(object);
  const previewCurve = workspace.getByRole("button", {
    name: "plot · curve.csv · present",
    exact: true,
  });
  writeFileSync(object, "corrupted retained curve");
  await previewCurve.click();
  await expect(workspace.getByRole("alert")).toContainText(
    "attachment digest mismatch",
  );
  await expect(
    workspace.getByLabel("Metric comparison", { exact: true }),
  ).toContainText("0.1");
  await expect(workspace.locator("g.mark-line path")).toHaveCount(4);
  await page.screenshot({
    path: path.join(output, "native-results-corrupt-evidence.png"),
    fullPage: true,
  });
  const viewport = await page.evaluate(() => ({
    width: innerWidth,
    height: innerHeight,
  }));
  await page.setViewportSize({ width: 900, height: viewport.height });
  await page.screenshot({
    path: path.join(output, "native-results-corrupt-evidence-narrow.png"),
    fullPage: true,
  });
  await expect(
    workspace
      .getByRole("alert")
      .getByRole("button", { name: "Refresh view", exact: true }),
  ).toBeInViewport({ ratio: 1 });
  await page.setViewportSize(viewport);
  writeFileSync(object, bytes);
  await previewCurve.click();
  await expect(workspace.getByRole("alert")).toHaveCount(0);
  await expect(workspace.locator("pre")).toContainText("step,loss");
  unlinkSync(object);
  await previewCurve.click();
  await expect(workspace.getByRole("alert")).toContainText(
    /No such file|cannot find|not found/i,
  );
  await expect(
    workspace.getByLabel("Metric comparison", { exact: true }),
  ).toContainText("0.2");
  await expect(page.getByLabel("Report ID")).toHaveText(reportId);
  await page.screenshot({
    path: path.join(output, "native-results-missing-evidence.png"),
    fullPage: true,
  });
  writeFileSync(object, bytes);
  await previewCurve.click();
  await expect(workspace.getByRole("alert")).toHaveCount(0);
  await expect(workspace.locator("pre")).toContainText("step,loss");

  await open(page, source);
  await chooseSelect(page.getByLabel("Open immutable report"), "");
  const failed = workspace
    .getByRole("row")
    .filter({ hasText: "failed" })
    .getByRole("checkbox");
  await expect(failed).toBeChecked();
  await failed.uncheck();
  await expect(workspace.locator("g.mark-line path")).toHaveCount(4);
  const coordinates =
    response.data.report.comparison.rows[0].run.snapshot.attachments.find(
      (attachment) => attachment.path === "steps.csv",
    );
  if (!coordinates?.hash)
    throw new Error("Actual run has no retained coordinates");
  const refresh = workspace.getByRole("button", {
    name: "Refresh Results",
    exact: true,
  });
  for (const retained of [plot, coordinates]) {
    const coordinate = retained.path === "steps.csv";
    const sourceObject = path.join(
      source,
      ".crab/workflow/results/objects",
      retained.hash!,
    );
    const original = readFileSync(sourceObject);
    for (const missing of [false, true]) {
      if (missing) unlinkSync(sourceObject);
      else writeFileSync(sourceObject, "corrupted retained curve");
      await refresh.click();
      await expect(workspace).toContainText(
        missing
          ? "Retained plot bytes are missing"
          : "Retained plot failed integrity verification",
      );
      await expect(workspace.getByRole("alert")).toHaveCount(0);
      await expect(
        workspace.getByLabel("Metric comparison", { exact: true }),
      ).toContainText("0.1");
      await expect(
        workspace.getByLabel("Metric comparison", { exact: true }),
      ).toContainText("0.2");
      await expect(workspace.locator("g.mark-line path")).toHaveCount(3);
      const partial = workspace.getByRole("region", {
        name: "curve.csv",
        exact: true,
      });
      await partial.scrollIntoViewIfNeeded();
      await expect(partial.locator("g.mark-line path")).toBeInViewport();
      await page.screenshot({
        path: path.join(
          output,
          `native-results-partial-${coordinate ? "coordinates-" : ""}${missing ? "missing" : "corrupt"}.png`,
        ),
        fullPage: true,
      });
      if (!missing) {
        await page.setViewportSize({ width: 900, height: viewport.height });
        await partial.scrollIntoViewIfNeeded();
        await expect(partial.locator("g.mark-line path")).toBeInViewport();
        await page.screenshot({
          path: path.join(
            output,
            `native-results-partial-${coordinate ? "coordinates-" : ""}corrupt-narrow.png`,
          ),
          fullPage: true,
        });
        await page.setViewportSize(viewport);
      }
      writeFileSync(sourceObject, original);
      await refresh.click();
      await expect(workspace).not.toContainText(
        "Retained plot bytes are missing",
      );
      await expect(workspace).not.toContainText(
        "Retained plot failed integrity verification",
      );
      await expect(workspace.locator("g.mark-line path")).toHaveCount(4);
    }
  }

  const coordinateObjects = response.data.report.comparison.rows.map((row) => {
    const attachment = row.run.snapshot.attachments.find(
      (a) => a.path === "steps.csv",
    );
    if (!attachment?.hash) throw new Error("Run has no retained coordinates");
    const file = path.join(
      source,
      ".crab/workflow/results/objects",
      attachment.hash,
    );
    return { file, bytes: readFileSync(file) };
  });
  for (const object of coordinateObjects)
    writeFileSync(object.file, "corrupt coordinates");
  await refresh.click();
  await expect(workspace).toContainText(
    "Retained plot failed integrity verification",
  );
  const unavailable = workspace.getByRole("region", {
    name: "curve.csv",
    exact: true,
  });
  await expect(unavailable.locator("g.mark-line path")).toHaveCount(0);
  await expect(workspace.locator("g.mark-line path")).toHaveCount(2);
  await unavailable.scrollIntoViewIfNeeded();
  await page.screenshot({
    path: path.join(output, "native-results-all-coordinates-unavailable.png"),
    fullPage: true,
  });
  for (const object of coordinateObjects)
    writeFileSync(object.file, object.bytes);
  await refresh.click();
  await expect(workspace.locator("g.mark-line path")).toHaveCount(4);

  const closing = await open(page, cold);
  await checkReport(page, reportId);
  await call(page, "close", { repo_id: closing.repo_id });
  await refresh.click();
  await expect(workspace.getByRole("alert")).toContainText(
    "native session ended",
  );
  // A closed reader clears report bodies. The chooser must still pin the
  // complete selected ID so reopening can recover the same report.
  await page.getByLabel("Open immutable report").click();
  await expect(
    page.locator(`[role="option"][data-value=":${reportId}"]`),
  ).toHaveAttribute("data-state", "checked");
  await page.keyboard.press("Escape");
  await page.screenshot({
    path: path.join(output, "native-results-closed-session.png"),
    fullPage: true,
  });
  const reopened = await open(page, cold);
  expect(reopened.native_session_id).not.toBe(closing.native_session_id);
  await checkReport(page, reportId);
}
