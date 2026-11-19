import {
  test,
  expect,
  _electron as electron,
  type ElectronApplication,
} from "@playwright/test";
import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
} from "node:fs";
import path from "node:path";
import { call, closeFixtureDaemon, packagedExecutable } from "./packaged-app";
import { PROTOCOL_VERSION } from "../../electron/daemon/protocol";

test("native Results preserves real observations when authored plots fail", async () => {
  const evidence = process.env.CRAB_RESULTS_EVIDENCE;
  const repo = evidence && path.join(evidence, "authored-fixture");
  test.skip(
    !repo || !existsSync(repo),
    "Requires the real CLI authored-plot capture",
  );
  test.setTimeout(90_000);
  const root = mkdtempSync(
    path.join(process.env.CARGO_TARGET_DIR!, "results-authored-native-"),
  );
  const home = path.join(root, "home");
  mkdirSync(home);
  const payload: { id: string } = JSON.parse(
    readFileSync(path.join(evidence!, "authored-failures.html"), "utf8").match(
      /<script id="results-data" type="application\/json">([\s\S]*?)<\/script>/,
    )![1],
  );
  let app: ElectronApplication | undefined;
  const errors: string[] = [];
  try {
    app = await electron.launch({
      executablePath: packagedExecutable(),
      args: [`--user-data-dir=${path.join(root, "profile")}`],
      env: {
        ...process.env,
        HOME: home,
        USERPROFILE: home,
        CRAB_CACHE_DIR: path.join(root, "cache"),
      },
    });
    const page = await app.firstWindow();
    page.on("pageerror", (error) => errors.push(error.message));
    await page.getByRole("button", { name: "Skip", exact: true }).click();
    await page.getByRole("button", { name: "No thanks", exact: true }).click();
    await expect
      .poll(async () => {
        try {
          return (await call(page, "ping", {})).pong;
        } catch {
          return false;
        }
      })
      .toBeTruthy();
    const handle = await call(page, "open_local", {
      repo_id: repo!,
      path: repo!,
    });
    await page.evaluate(
      ({ handle, repo }) =>
        window.dispatchEvent(
          new CustomEvent("crab:repo-opened", {
            detail: { handle, path: repo },
          }),
        ),
      { handle, repo },
    );
    const primary = page.getByRole("navigation", {
      name: "Primary",
      exact: true,
    });
    await expect(
      primary.getByRole("button", { name: "Results", exact: true }),
    ).toHaveCount(0);
    await primary
      .getByRole("button", { name: "Data Workflows", exact: true })
      .click();
    await page.getByRole("tab", { name: "Results", exact: true }).click();
    await page.getByLabel("Open immutable report").selectOption(payload.id);
    const workspace = page.getByLabel("Results workspace", { exact: true });
    const healthy = workspace.locator('section[aria-label="healthy"]');
    const event = workspace.locator('section[aria-label="event"]');
    await expect(workspace.getByRole("alert")).toHaveCount(2);
    await expect(event.locator("g.mark-line path")).toHaveCount(1);
    await event.getByRole("checkbox").click();
    await expect(event.getByRole("alert")).toContainText(
      "Could not render this plot. Inspect its template and source data.",
    );
    await expect(workspace.getByRole("alert")).toHaveCount(3);
    await expect(event.locator("svg")).toHaveCount(0);
    await expect(healthy.locator("g.mark-line path")).toHaveCount(1);
    await expect(workspace).toContainText("0.25");
    await expect(page.getByLabel("Open immutable report")).toHaveValue(
      payload.id,
    );
    for (const width of [1280, 900]) {
      await page.setViewportSize({ width, height: 1000 });
      await event.scrollIntoViewIfNeeded();
      await page.screenshot({
        path: path.join(evidence!, `native-authored-failure-${width}.png`),
        fullPage: true,
      });
    }
    await page.getByLabel("Open immutable report").selectOption("");
    await page.getByLabel("Open immutable report").selectOption(payload.id);
    await expect(workspace.getByRole("alert")).toHaveCount(2);
    await expect(event.locator("g.mark-line path")).toHaveCount(1);
    await expect(healthy.locator("g.mark-line path")).toHaveCount(1);
    expect(errors).toEqual([]);

    const partialRepo = path.join(evidence!, "partial-template-fixture");
    const partial: { id: string } = JSON.parse(
      readFileSync(path.join(evidence!, "partial-template.html"), "utf8").match(
        /<script id="results-data" type="application\/json">([\s\S]*?)<\/script>/,
      )![1],
    );
    const partialHandle = await call(page, "open_local", {
      repo_id: partialRepo,
      path: partialRepo,
    });
    await page.evaluate(
      ({ handle, repo }) =>
        window.dispatchEvent(
          new CustomEvent("crab:repo-opened", {
            detail: { handle, path: repo },
          }),
        ),
      { handle: partialHandle, repo: partialRepo },
    );
    await primary
      .getByRole("button", { name: "Data Workflows", exact: true })
      .click();
    await page.getByRole("tab", { name: "Results", exact: true }).click();
    await page.getByLabel("Open immutable report").selectOption(partial.id);
    const curve = workspace.locator('section[aria-label="Saved loss"]');
    await expect(curve).toContainText(
      "Plot evidence was not captured for this run.",
    );
    await expect(curve.locator("g.mark-line path")).toHaveCount(1);
    await expect(workspace).toContainText("0.1");
    await expect(workspace).toContainText("0.2");
    await expect(page.getByLabel("Open immutable report")).toHaveValue(
      partial.id,
    );
    await expect(workspace.getByRole("alert")).toHaveCount(0);
    for (const width of [1280, 900]) {
      await page.setViewportSize({ width, height: 1000 });
      await curve.scrollIntoViewIfNeeded();
      await page.screenshot({
        path: path.join(evidence!, `native-partial-template-${width}.png`),
        fullPage: true,
      });
    }
    await page.getByLabel("Open immutable report").selectOption("");
    await page.getByLabel("Open immutable report").selectOption(partial.id);
    await expect(curve.locator("g.mark-line path")).toHaveCount(1);
    await expect(curve).toContainText(
      "Plot evidence was not captured for this run.",
    );
    expect(errors).toEqual([]);
  } finally {
    await app?.close();
    if (
      existsSync(
        path.join(home, `.crab/desktop/daemon/daemon-v${PROTOCOL_VERSION}.pid`),
      )
    )
      await closeFixtureDaemon(home);
    rmSync(root, { recursive: true, force: true });
  }
});
