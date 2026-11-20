import { chooseSelect } from "../helpers/select";
import {
  test,
  expect,
  _electron as electron,
  type ElectronApplication,
  type Page,
} from "@playwright/test";
import { execFileSync } from "node:child_process";
import {
  appendFileSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  writeFileSync,
  rmSync,
  unlinkSync,
} from "node:fs";
import path from "node:path";
import { randomUUID } from "node:crypto";
import { pathToFileURL } from "node:url";
import { PROTOCOL_VERSION } from "../../electron/daemon/protocol";
import { call, closeFixtureDaemon, packagedExecutable } from "./packaged-app";
import {
  checkResultsFailures,
  checkStorageFailures,
} from "./results-failure-checks";
import { resultsStorageProxy } from "./results-storage-proxy";
import { checkResultsLifecycle } from "./results-lifecycle";

function git(root: string, args: string[]) {
  return execFileSync(
    "git",
    [
      "-c",
      "user.name=Results fixture",
      "-c",
      "user.email=results@example.invalid",
      "-c",
      "commit.gpgsign=false",
      ...args,
    ],
    { cwd: root, stdio: "pipe" },
  );
}
async function prompt(page: Page, name: RegExp, value: string) {
  const dialog = page.getByRole("dialog", { name });
  await dialog.getByRole("textbox").fill(value);
  await dialog.getByRole("button", { name: "OK", exact: true }).click();
}

async function queueRuns(page: Page, values: string) {
  await page.getByRole("tab", { name: "Queue", exact: true }).click();
  await page.getByRole("button", { name: "Queue", exact: true }).click();
  await page.getByPlaceholder("train.lr", { exact: true }).fill("factor");
  await page
    .getByPlaceholder("0.001, 0.01, 0.1 or range(0.001, 0.01, 0.003)", {
      exact: true,
    })
    .fill(values);
  await page.getByRole("button", { name: "Queue", exact: true }).last().click();
  await page.getByRole("button", { name: /Start \(jobs:/ }).click();
}

async function bindDataset(page: Page, id: string) {
  const workspace = page.getByLabel("Results workspace", { exact: true });
  await workspace
    .getByRole("button", { name: id.slice(-8), exact: true })
    .click();
  const hash = workspace.getByLabel("Dataset hash");
  if (!(await hash.isVisible()))
    await workspace
      .locator("summary")
      .filter({ hasText: "Dataset and split identity" })
      .click();
  const witness = await workspace
    .locator("datalist option")
    .filter({ hasText: "dataset.csv" })
    .first()
    .getAttribute("value");
  expect(witness).toMatch(/^[0-9a-f]{64}$/);
  await hash.fill(witness!);
  await workspace.getByLabel("Dataset split").fill("validation");
  await workspace.getByLabel("Evaluator identity").fill("native-evaluator-v1");
  await expect(hash).toHaveValue(witness!);
  await workspace
    .getByRole("button", { name: "Set identity", exact: true })
    .click();
}

test("Results retains an actual comparison after restart and cold remote report retrieval", async () => {
  test.skip(
    !process.env.CRAB_SDK_TEST_S3_BUCKET,
    "Requires an owned writable S3-compatible fixture",
  );
  test.setTimeout(360_000);
  const root = mkdtempSync(
    path.join(process.env.CARGO_TARGET_DIR!, "desktop-results-"),
  );
  const repo = path.join(root, "source");
  const cold = path.join(root, "cold");
  const home = path.join(root, "home");
  mkdirSync(repo);
  mkdirSync(home);
  git(repo, ["init", "--initial-branch=main"]);
  const remote = `crab://${process.env.CRAB_SDK_TEST_S3_BUCKET}/results-desktop/${randomUUID()}`;
  writeFileSync(path.join(repo, "params.json"), JSON.stringify({ factor: 1 }));
  writeFileSync(path.join(repo, "dataset.csv"), "x,y\n1,2\n");
  const externalDataset = path.join(root, "external-dataset.txt");
  writeFileSync(externalDataset, "retained identity without a public locator");
  const train =
    "const fs=require('fs');const f=JSON.parse(fs.readFileSync('params.json')).factor;fs.writeFileSync('metrics.json',JSON.stringify({score:f/10,loss:1/f}));fs.writeFileSync('curve.csv','step,loss\\n0,'+(4/f)+'\\n1,'+(2/f)+'\\n');fs.writeFileSync('steps.csv','step,series\\n0,'+f+'\\n1,'+f+'\\n');fs.writeFileSync('curve2.csv','step,loss\\n0,'+(3/f)+'\\n1,'+(1/f)+'\\n');fs.writeFileSync('classes.csv','actual,predicted\\n'+(f===1?'0,0\\n0,0\\n1,1\\n1,1\\n':'0,1\\n0,1\\n1,0\\n1,0\\n'));fs.writeFileSync('prediction.svg','<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"200\" height=\"100\"><rect width=\"'+(f*50)+'\" height=\"100\" fill=\"#3b82f6\"/></svg>');fs.writeFileSync('model.json',JSON.stringify({factor:f}));console.log('trained factor '+f);if(f===3)process.exit(7)";
  writeFileSync(
    path.join(repo, "crab.yaml"),
    JSON.stringify({
      params: ["params.json"],
      defaults: { env: "empty" },
      plots: [
        {
          "curve.csv": {
            x: { "steps.csv": "step" },
            y: { "curve.csv": "loss" },
          },
        },
        { "curve2.csv": { x: "step", y: "loss", title: "Validation loss" } },
        "prediction.svg",
        {
          "classes.csv": { x: "actual", y: "predicted", template: "confusion" },
        },
      ],
      stages: {
        train: {
          cmd: { argv: [process.execPath, "-e", train] },
          deps: [
            "dataset.csv",
            { url: { url: pathToFileURL(externalDataset).href } },
          ],
          params: [{ "params.json": ["factor"] }],
          metrics: ["metrics.json"],
          plots: ["curve.csv", "curve2.csv", "prediction.svg", "classes.csv"],
          outs: ["model.json", "steps.csv"],
        },
      },
    }),
  );
  const env = {
    ...process.env,
    HOME: home,
    USERPROFILE: home,
    CRAB_CACHE_DIR: path.join(root, "cache"),
    GIT_CONFIG_GLOBAL: path.join(root, "gitconfig"),
    GIT_CONFIG_NOSYSTEM: "1",
  };
  writeFileSync(env.GIT_CONFIG_GLOBAL, "");
  const executablePath = packagedExecutable();
  const resources =
    process.platform === "darwin"
      ? path.resolve(path.dirname(executablePath), "../Resources")
      : path.join(path.dirname(executablePath), "resources");
  const crab = path.join(
    resources,
    "agent",
    process.platform === "win32" ? "crab.exe" : "crab",
  );
  execFileSync(crab, ["init", remote, "--json"], {
    cwd: repo,
    env,
    stdio: "pipe",
  });
  git(repo, ["add", "."]);
  git(repo, ["commit", "-m", "Results baseline"]);
  const storage = await resultsStorageProxy(process.env.AWS_ENDPOINT_URL_S3!);
  let app: ElectronApplication | undefined;
  let activeHandle: Record<string, unknown> | undefined;
  const lifecycleFile = path.join(
    process.env.CRAB_RESULTS_EVIDENCE ?? root,
    "native-process-lifecycle.log",
  );
  const recordLifecycle = (phase: string, pid: number | undefined) =>
    appendFileSync(
      lifecycleFile,
      JSON.stringify({ phase, pid, wall_ms: Date.now() }) + "\n",
    );
  const launch = async () => {
    recordLifecycle("launch.start", undefined);
    app = await electron.launch({
      executablePath: packagedExecutable(),
      args: [`--user-data-dir=${path.join(root, "profile")}`],
      env: {
        ...env,
        AWS_ENDPOINT_URL: storage.endpoint,
        AWS_ENDPOINT_URL_S3: storage.endpoint,
      },
    });
    const child = app.process();
    recordLifecycle("launch.connected", child.pid);
    child.once("exit", () => recordLifecycle("child.exit", child.pid));
    child.once("close", () => recordLifecycle("child.stdio.close", child.pid));
    await app.evaluate(({ app }, file) => {
      const fs = process.getBuiltinModule(
        "node:fs",
      ) as typeof import("node:fs");
      const record = (phase: string) =>
        fs.appendFileSync(
          file,
          JSON.stringify({ phase, pid: process.pid, wall_ms: Date.now() }) +
            "\n",
        );
      for (const event of [
        "before-quit",
        "will-quit",
        "window-all-closed",
      ] as const)
        app.prependListener(event, () => record(event));
      process.once("exit", () => record("main.node.exit"));
    }, lifecycleFile);
    recordLifecycle("firstWindow.start", child.pid);
    const page = await app.firstWindow();
    recordLifecycle("firstWindow.end", child.pid);
    await expect
      .poll(async () => {
        try {
          return (await call(page, "ping", {})).pong;
        } catch {
          return false;
        }
      })
      .toBeTruthy();
    return page;
  };
  const close = async (phase: string) => {
    await test.step(`Close desktop: ${phase}`, async () => {
      const pid = app!.process().pid;
      recordLifecycle(`${phase}.close.start`, pid);
      await app!.close();
      recordLifecycle(`${phase}.close.end`, pid);
    });
    app = undefined;
  };
  const open = async (page: Page, directory: string) => {
    const handle = await call(page, "open_local", {
      repo_id: directory,
      path: directory,
    });
    activeHandle = handle;
    await page.evaluate(
      ({ handle, directory }) =>
        window.dispatchEvent(
          new CustomEvent("crab:repo-opened", {
            detail: { handle, path: directory },
          }),
        ),
      { handle, directory },
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
    return handle;
  };
  const checkReport = async (page: Page, id: string) => {
    await expect(page.getByLabel("Report ID")).toHaveText(id);
    const workspace = page.getByLabel("Results workspace", { exact: true });
    await expect(workspace).toContainText("0.1");
    await expect(workspace).toContainText("0.2");
    await expect(
      workspace
        .locator("section")
        .filter({ has: page.locator("g.mark-line path") }),
    ).toHaveCount(2);
    await expect(workspace.locator("g.mark-line path")).toHaveCount(4);
    await expect(workspace.locator("figure img")).toHaveCount(2);
    await expect(workspace.locator("g.mark-rect.role-mark path")).toHaveCount(
      8,
    );
    await expect(
      workspace.getByText(
        "Counted all 8 retained observations into 4 observed class pairs. Missing class combinations are shown as zero.",
        { exact: true },
      ),
    ).toBeVisible();
    const split = workspace.getByLabel("Dataset split");
    if (!(await split.isVisible()))
      await workspace
        .locator("summary")
        .filter({ hasText: "Dataset and split identity" })
        .click();
    await expect(split).toHaveValue("validation");
    await expect(split).toHaveJSProperty("readOnly", true);
    await expect(workspace.getByLabel("Evaluator identity")).toHaveValue(
      "native-evaluator-v1",
    );
    const externalWitness = workspace.getByText(/^Input \(external source\)/);
    if (!(await externalWitness.isVisible()))
      await workspace
        .getByText("Dataset/output witnesses", { exact: true })
        .click();
    await expect(externalWitness).toBeVisible();
    const curves = workspace
      .locator("section")
      .filter({ has: page.locator("g.mark-line path") });
    await expect
      .poll(async () =>
        curves.evaluateAll((charts) =>
          charts.every(
            (chart) =>
              chart.querySelector("svg")!.getBoundingClientRect().width <=
              chart.clientWidth + 1,
          ),
        ),
      )
      .toBeTruthy();
    await workspace
      .locator('g[class*="_voronoi"] path')
      .first()
      .hover({ force: true });
    await expect(workspace.getByText(/^Cursor step:/)).toHaveCount(2);
    const cursors = await workspace
      .getByText(/^Cursor step:/)
      .allTextContents();
    expect(cursors[0]).toEqual(cursors[1]);

    await expect(workspace.getByLabel(/^X minimum /)).toHaveCount(2);
    for (const input of await workspace.getByLabel(/^X minimum /).all())
      await expect(input).toHaveValue("0");
    for (const input of await workspace.getByLabel(/^X maximum /).all())
      await expect(input).toHaveValue("0.5");
    await workspace
      .getByRole("button", { name: "Zoom in", exact: true })
      .first()
      .click();
    for (const input of await workspace.getByLabel(/^X minimum /).all())
      await expect(input).toHaveValue("0.125");
    await workspace
      .getByRole("button", { name: "Reset range", exact: true })
      .first()
      .click();
    for (const input of await workspace.getByLabel(/^X maximum /).all())
      await expect(input).toHaveValue("0.5");
    await expect(page.getByLabel("Report ID")).toHaveText(id);
    await expect(workspace.getByRole("alert")).toHaveCount(0);
  };
  try {
    let page = await launch();
    await page.getByRole("button", { name: "Skip", exact: true }).click();
    await page.getByRole("button", { name: "No thanks", exact: true }).click();
    await open(page, repo);
    const primary = page.getByRole("navigation", {
      name: "Primary",
      exact: true,
    });
    await primary
      .getByRole("button", { name: "Data Workflows", exact: true })
      .click();
    await queueRuns(page, "1,2");
    await expect(page.getByText("Done", { exact: true })).toHaveCount(2, {
      timeout: 60_000,
    });
    await page.getByRole("tab", { name: "Results", exact: true }).click();
    const selections = page.getByRole("checkbox", { name: /^Compare / });
    await expect(selections).toHaveCount(2);
    await selections.nth(0).check();
    await selections.nth(1).check();
    await expect(page.getByLabel("Results workspace")).toContainText("Δ");
    const runId = async (score: string) => {
      const row = page.getByRole("row").filter({
        has: page.getByRole("cell", { name: score, exact: true }),
      });
      const label = await row.getByRole("checkbox").getAttribute("aria-label");
      expect(label).toMatch(/^Compare /);
      return label!.replace(/^Compare /, "");
    };
    const newer = await runId("0.2");
    const older = await runId("0.1");
    await chooseSelect(page.getByLabel("Baseline run"), older);
    await chooseSelect(
      page.getByLabel("Objective metrics.json/score"),
      "maximize",
    );
    await bindDataset(page, newer);
    await bindDataset(page, older);
    const candidate = page
      .getByRole("region", { name: "Metric comparison" })
      .getByRole("row")
      .filter({
        has: page.getByRole("button", { name: newer.slice(-8), exact: true }),
      });
    await expect(candidate.getByRole("cell").last()).toContainText(
      "Δ 0.1000 (100.00%)",
    );
    await expect(
      candidate.getByRole("cell").last().locator(".text-success"),
    ).toHaveCount(1);
    await page.getByText("Columns", { exact: true }).click();
    const scoreColumn = page.getByRole("checkbox", {
      name: "metric · metrics.json/score",
      exact: true,
    });
    await scoreColumn.uncheck();
    await expect(page.getByLabel("Objective metrics.json/score")).toHaveCount(
      0,
    );
    await scoreColumn.check();
    await expect(page.getByLabel("Objective metrics.json/score")).toHaveText(
      "Maximize",
    );
    await page.getByText("Columns", { exact: true }).click();
    await page.getByRole("button", { name: "Apply", exact: true }).click();
    const preview = page.getByRole("dialog", {
      name: "Apply captured workspace",
    });
    await expect(preview).toContainText("params.json");
    await expect(preview).toContainText("model.json");
    await preview.getByRole("button", { name: "Cancel", exact: true }).click();
    const range = page.getByRole("form", { name: /^X range / }).first();
    await expect(range).toContainText(
      "Ranges are saved with your working comparison",
    );
    const zoom = range.getByRole("button", { name: "Zoom in", exact: true });
    await zoom.focus();
    await page.evaluate(
      async ({ handle, directory }) => {
        window.dispatchEvent(
          new CustomEvent("crab:repo-opened", {
            detail: { handle, path: directory },
          }),
        );
        await new Promise<void>((resolve) =>
          requestAnimationFrame(() => requestAnimationFrame(() => resolve())),
        );
      },
      { handle: activeHandle, directory: repo },
    );
    await expect(zoom).toBeFocused();
    await page.keyboard.press("Enter");
    await expect(zoom).toBeFocused();
    await page.screenshot({
      path: path.join(
        process.env.CRAB_RESULTS_EVIDENCE ?? root,
        "native-metadata-focus.png",
      ),
    });
    for (const input of await page.getByLabel(/^X minimum /).all())
      await expect(input).toHaveValue("0.25");
    await range.getByLabel(/^X minimum /).fill("0");
    await range.getByLabel(/^X maximum /).fill("0.5");
    const applyRange = range.getByRole("button", {
      name: "Apply range",
      exact: true,
    });
    await applyRange.focus();
    await page.keyboard.press("Enter");
    await expect(applyRange).toBeFocused();
    await close("working comparison");
    page = await launch();
    await open(page, repo);
    await expect(
      page.getByRole("checkbox", { name: /^Compare /, checked: true }),
    ).toHaveCount(2);
    await expect(page.getByLabel(/^X maximum /)).toHaveCount(2);
    for (const input of await page.getByLabel(/^X maximum /).all())
      await expect(input).toHaveValue("0.5");
    await page
      .getByRole("button", { name: "Save report", exact: true })
      .click();
    await prompt(page, /Save immutable report/, "Real validation comparison");
    await expect(page.getByLabel("Report ID")).toHaveText(/^[0-9a-f]{64}$/);
    const id = await page.getByLabel("Report ID").innerText();
    expect(id).toMatch(/^[0-9a-f]{64}$/);
    await checkReport(page, id);
    await page
      .getByRole("button", { name: "Publish report", exact: true })
      .click();
    await expect(
      page.getByRole("status").filter({ hasText: "Report published" }),
    ).toBeVisible({ timeout: 90_000 });
    await close("published report");
    page = await launch();
    await open(page, repo);
    await checkReport(page, id);
    git(root, ["clone", repo, cold]);
    git(cold, ["remote", "set-url", "origin", remote]);
    unlinkSync(path.join(cold, "crab.yaml"));
    await open(page, cold);
    await page
      .getByRole("button", { name: "Retrieve report", exact: true })
      .click();
    await prompt(page, /Open immutable report/, id);
    await checkReport(page, id);
    await page
      .getByLabel("Results workspace", { exact: true })
      .locator(":scope > .overflow-auto")
      .evaluate((element) => {
        element.scrollTop = 0;
      });
    await page.screenshot({
      path: path.join(
        process.env.CRAB_RESULTS_EVIDENCE ?? root,
        "native-results-top.png",
      ),
      fullPage: true,
    });
    await page
      .getByLabel("Results workspace", { exact: true })
      .locator("g.mark-line path")
      .first()
      .scrollIntoViewIfNeeded();
    await page.screenshot({
      path: path.join(
        process.env.CRAB_RESULTS_EVIDENCE ?? root,
        "native-results.png",
      ),
      fullPage: true,
    });
    await close("retrieved report");
    page = await launch();
    await open(page, cold);
    await checkReport(page, id);
    await app!.evaluate(({ ipcMain }, repo) => {
      // Electron freezes the renderer bridge. Reorder real replies at its
      // main registration, retaining both calls to the actual sidecar.
      const transport = ipcMain as unknown as {
        _invokeHandlers: Map<
          string,
          (
            event: unknown,
            method: string,
            params: Record<string, unknown>,
          ) => Promise<unknown>
        >;
      };
      const original = transport._invokeHandlers.get("crab:rpc");
      if (!original) throw new Error("Missing Electron RPC registration");
      const state = globalThis as unknown as { queueReplyOrder: number[] };
      state.queueReplyOrder = [];
      let held = false;
      let release: (() => void) | undefined;
      transport._invokeHandlers.set(
        "crab:rpc",
        async (event, method, params) => {
          const reply = await original(event, method, params);
          if (method !== "workflow_exp_list" || params.repo_id !== repo)
            return reply;
          const rows = (reply as { value?: { experiments?: unknown[] } }).value
            ?.experiments;
          if (rows?.length === 2 && !held) {
            held = true;
            await new Promise<void>((resolve) => {
              release = resolve;
            });
          } else if (rows?.length === 3 && release) {
            const complete = release;
            release = undefined;
            setTimeout(complete, 0);
          }
          state.queueReplyOrder.push(rows?.length ?? -1);
          return reply;
        },
      );
    }, repo);
    await open(page, repo);
    await queueRuns(page, "3");
    await expect
      .poll(() =>
        app!.evaluate(() =>
          (
            globalThis as unknown as { queueReplyOrder: number[] }
          ).queueReplyOrder.slice(0, 2),
        ),
      )
      .toEqual([3, 2]);
    await expect(page.getByText("Failed", { exact: true })).toHaveCount(1, {
      timeout: 60_000,
    });
    await page.getByRole("tab", { name: "Results", exact: true }).click();
    await chooseSelect(page.getByLabel("Open immutable report"), "");
    const failed = page
      .getByLabel("Results workspace", { exact: true })
      .getByRole("row")
      .filter({ hasText: "failed" });
    await expect(failed).toHaveCount(1);
    await expect(
      failed.getByRole("cell", { name: "0.3", exact: true }),
    ).toHaveCount(1);
    await expect(page.getByRole("checkbox", { name: /^Compare / })).toHaveCount(
      3,
    );
    await failed.getByRole("checkbox").check();
    await page
      .getByLabel("Metric comparison", { exact: true })
      .getByRole("row")
      .filter({ hasText: "0.3" })
      .getByRole("button")
      .click();
    await expect(
      page.getByText("Duration unavailable", { exact: false }),
    ).toBeVisible();
    await checkResultsFailures({
      page,
      source: repo,
      cold,
      reportId: id,
      output: process.env.CRAB_RESULTS_EVIDENCE ?? root,
      open,
      checkReport,
    });
    const bareRepoId = `bare-${randomUUID()}`;
    const openBare = async () => {
      const handle = await call(page, "open_url", {
        repo_id: bareRepoId,
        url: remote,
      });
      activeHandle = handle;
      await page.evaluate(
        ({ handle, url }) =>
          window.dispatchEvent(
            new CustomEvent("crab:repo-opened", {
              detail: { handle, path: url },
            }),
          ),
        { handle, url: remote },
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
      await chooseSelect(page.getByLabel("Open immutable report"), id);
      await checkReport(page, id);
      return handle;
    };
    let bareHandle = await openBare();
    await expect(
      page.getByRole("button", { name: "Publish report", exact: true }),
    ).toBeDisabled();
    await page.screenshot({
      path: path.join(
        process.env.CRAB_RESULTS_EVIDENCE!,
        "native-bare-results.png",
      ),
    });
    await checkResultsLifecycle({
      app: app!,
      page,
      reportId: id,
      storage,
      output: process.env.CRAB_RESULTS_EVIDENCE ?? root,
      checkReport,
    });
    // Reload restores a new admitted session. The following RPC fixture must
    // capture that owner again rather than bypassing the stale-session guard.
    bareHandle = await openBare();
    await checkStorageFailures({
      page,
      handle: bareHandle,
      reportId: id,
      storage,
      output: process.env.CRAB_RESULTS_EVIDENCE ?? root,
      recoverLocal: async () => {
        await open(page, cold);
        await checkReport(page, id);
      },
    });
  } catch (error) {
    // Failure capture must not wait for a future window during a failed close.
    const page = app?.windows().find((window) => !window.isClosed());
    await page
      ?.screenshot({
        path: path.join(
          process.env.CRAB_RESULTS_EVIDENCE ?? root,
          "native-results-failure.png",
        ),
        fullPage: true,
      })
      .catch(() => undefined);
    if (page) {
      const output = process.env.CRAB_RESULTS_EVIDENCE ?? root;
      writeFileSync(
        path.join(output, "native-results-dom.html"),
        await page.content(),
      );
      writeFileSync(
        path.join(output, "native-results-text.txt"),
        await page.locator("body").innerText(),
      );
      const reportId = await page
        .getByLabel("Report ID")
        .innerText()
        .catch(() => "");
      if (reportId) {
        const response = await call(page, "workflow_results_read", {
          repo_id: activeHandle?.repo_id ?? repo,
          native_session_id: activeHandle?.native_session_id,
          request: { kind: "report", args: { id: reportId } },
        }).catch((error) => ({ error: String(error) }));
        writeFileSync(
          path.join(output, "native-results-report.json"),
          JSON.stringify(response, null, 2),
        );
      }
    }
    throw error;
  } finally {
    await storage.close();
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
