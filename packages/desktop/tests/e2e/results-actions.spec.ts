import { chooseSelect } from "../helpers/select";
import {
  test,
  expect,
  _electron as electron,
  type ElectronApplication,
} from "@playwright/test";
import { execFileSync } from "node:child_process";
import {
  existsSync,
  closeSync,
  ftruncateSync,
  mkdirSync,
  mkdtempSync,
  openSync,
  readFileSync,
  renameSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import path from "node:path";
import { randomUUID } from "node:crypto";
import { call, closeFixtureDaemon, packagedExecutable } from "./packaged-app";
import { PROTOCOL_VERSION } from "../../electron/daemon/protocol";
import type {
  RunPage,
  SealedResult,
  ActionPreview,
} from "../../src/lib/results-contracts";

test("Results actions restore, reproduce, resume and promote actual captured outputs", async () => {
  test.skip(
    !process.env.CRAB_SDK_TEST_S3_BUCKET,
    "Requires an owned writable S3-compatible fixture",
  );
  test.setTimeout(240_000);
  const root = mkdtempSync(
    path.join(process.env.CARGO_TARGET_DIR!, "results-actions-"),
  );
  const repo = path.join(root, "repo");
  const home = path.join(root, "home");
  mkdirSync(repo);
  mkdirSync(home);
  const env = {
    ...process.env,
    HOME: home,
    USERPROFILE: home,
    CRAB_CACHE_DIR: path.join(root, "cache"),
    GIT_CONFIG_GLOBAL: path.join(root, "gitconfig"),
    GIT_CONFIG_NOSYSTEM: "1",
  };
  writeFileSync(env.GIT_CONFIG_GLOBAL, "");
  const git = (args: string[]) =>
    execFileSync(
      "git",
      [
        "-c",
        "user.name=Results actions",
        "-c",
        "user.email=results@example.invalid",
        "-c",
        "commit.gpgsign=false",
        ...args,
      ],
      { cwd: repo, env, stdio: "pipe" },
    )
      .toString()
      .trim();
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
  const cli = <T>(args: string[]): T =>
    (
      JSON.parse(
        execFileSync(crab, [...args, "--json"], {
          cwd: repo,
          env,
          stdio: "pipe",
        }).toString(),
      ) as { data: T }
    ).data;
  const list = () => cli<RunPage>(["results", "list"]);
  const show = (id: string) => cli<SealedResult>(["results", "show", id]);
  const model = (id: string) => {
    const file = path.join(root, `model-${id}.json`);
    cli(["results", "attachment", id, "model.json", "--output", file]);
    return JSON.parse(readFileSync(file, "utf8")) as Record<string, unknown>;
  };
  const recorded = `const fs = require('node:fs');
const { execFileSync } = require('node:child_process');
const factor = JSON.parse(fs.readFileSync('params.json')).factor;
const iteration = fs.existsSync('model.json') ? JSON.parse(fs.readFileSync('model.json')).iteration + 1 : 1;
fs.writeFileSync('model.json', JSON.stringify({factor, iteration, data: fs.readFileSync('data.txt','utf8'), revision: 'recorded'}));
fs.writeFileSync('metrics.json', JSON.stringify({score: factor / 10, nested: {zero: 0, missing: null}}));
fs.rmSync('obsolete.txt', {force:true});
execFileSync(process.env.CRAB_WORKFLOW_EXECUTABLE, ['workflow','checkpoint'], {stdio:'pipe'});
console.log('acknowledged iteration', iteration);
`;
  git(["init", "--initial-branch=main"]);
  writeFileSync(path.join(repo, "params.json"), JSON.stringify({ factor: 1 }));
  writeFileSync(path.join(repo, "data.txt"), "captured-data");
  writeFileSync(path.join(repo, "obsolete.txt"), "delete in experiment");
  writeFileSync(path.join(repo, "train.cjs"), recorded);
  writeFileSync(
    path.join(repo, "crab.yaml"),
    JSON.stringify({
      params: ["params.json"],
      defaults: { env: "empty" },
      artifacts: { model: { path: "model.json", kind: "model" } },
      stages: {
        train: {
          cmd: { argv: [process.execPath, "train.cjs"] },
          deps: ["train.cjs", "data.txt"],
          params: [{ "params.json": ["factor"] }],
          metrics: ["metrics.json"],
          outs: [{ path: "model.json", checkpoint: true }],
        },
      },
    }),
  );
  cli([
    "init",
    `crab://${process.env.CRAB_SDK_TEST_S3_BUCKET}/results-actions/${randomUUID()}`,
  ]);
  git(["add", "."]);
  git(["commit", "-m", "Recorded action workflow"]);
  const baseline = git(["rev-parse", "HEAD"]);
  let app: ElectronApplication | undefined;
  try {
    app = await electron.launch({
      executablePath,
      args: [`--user-data-dir=${path.join(root, "profile")}`],
      env,
    });
    const page = await app.firstWindow();
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
      repo_id: repo,
      path: repo,
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
    await page
      .getByRole("navigation", { name: "Primary", exact: true })
      .getByRole("button", { name: "Data Workflows", exact: true })
      .click();
    await page.getByRole("tab", { name: "Queue", exact: true }).click();
    await page.getByRole("button", { name: "Queue", exact: true }).click();
    await page.getByPlaceholder("train.lr", { exact: true }).fill("factor");
    await page
      .getByPlaceholder("0.001, 0.01, 0.1 or range(0.001, 0.01, 0.003)", {
        exact: true,
      })
      .fill("1,2");
    await page
      .getByRole("button", { name: "Queue", exact: true })
      .last()
      .click();
    await page.getByRole("button", { name: /Start \(jobs:/ }).click();
    await expect(page.getByText("Done", { exact: true })).toHaveCount(2, {
      timeout: 60_000,
    });
    await page.getByRole("tab", { name: "Results", exact: true }).click();
    const runs = list().rows;
    expect(runs).toHaveLength(2);
    const source = runs.find((run) =>
      run.metrics.some(
        (metric) => metric.pointer === "/score" && metric.value === 0.1,
      ),
    )!;
    const candidate = runs.find((run) => run.id !== source.id)!;
    const sourceRow = page.getByRole("row").filter({
      has: page.getByRole("cell", { name: "0.1", exact: true }),
    });
    await expect(
      sourceRow.getByRole("cell", { name: "0", exact: true }),
    ).toHaveCount(1);
    await expect(
      sourceRow.getByRole("cell", { name: "null", exact: true }),
    ).toHaveCount(1);
    const select = async (id: string) => {
      await page
        .getByRole("checkbox", { name: `Compare ${id}`, exact: true })
        .check();
      await page
        .getByRole("region", { name: "Metric comparison", exact: true })
        .getByRole("button", { name: id.slice(-8), exact: true })
        .click();
    };
    const status = (text: string) =>
      page.getByRole("status").filter({ hasText: text });
    const prompt = async (name: RegExp, value: string) => {
      const dialog = page.getByRole("dialog", { name });
      await dialog.getByRole("textbox").fill(value);
      await dialog.getByRole("button", { name: "OK", exact: true }).click();
    };
    const apply = async (id: string) => {
      await select(id);
      await page.getByRole("button", { name: "Apply", exact: true }).click();
      const dialog = page.getByRole("dialog", {
        name: "Apply captured workspace",
        exact: true,
      });
      await expect(dialog).toContainText("model.json");
      await dialog
        .getByRole("button", { name: "Apply workspace", exact: true })
        .click();
      await expect(status("apply completed")).toBeVisible({ timeout: 30_000 });
      await expect(dialog).toHaveCount(0);
    };
    const promote = async (version: string, expected: string) => {
      await page
        .getByRole("button", { name: "Promote artifact", exact: true })
        .click();
      await prompt(/Artifact name/, "model");
      await prompt(/Immutable artifact version/, version);
      await prompt(/Promote artifact to stage/, "staging");
      await prompt(/Expected current version/, expected);
    };
    await select(source.id);
    const initial = model(source.id);
    expect(initial).toEqual({
      factor: 1,
      iteration: 1,
      data: "captured-data",
      revision: "recorded",
    });
    writeFileSync(path.join(repo, "model.json"), "dirty model");
    writeFileSync(
      path.join(repo, "params.json"),
      JSON.stringify({ factor: 777 }),
    );
    await page.getByRole("button", { name: "Apply", exact: true }).click();
    const preview = page.getByRole("dialog", {
      name: "Apply captured workspace",
      exact: true,
    });
    await expect(preview).toContainText("obsolete.txt");
    await expect(preview).toContainText(
      "Local edits at these paths will be overwritten",
    );
    await preview.getByRole("button", { name: "Cancel", exact: true }).click();
    expect(readFileSync(path.join(repo, "model.json"), "utf8")).toBe(
      "dirty model",
    );
    await apply(source.id);
    expect(
      JSON.parse(readFileSync(path.join(repo, "model.json"), "utf8")),
    ).toEqual(initial);
    expect(
      JSON.parse(readFileSync(path.join(repo, "params.json"), "utf8")),
    ).toEqual({ factor: 1 });
    expect(existsSync(path.join(repo, "obsolete.txt"))).toBe(false);
    git(["add", "-A"]);
    git(["commit", "-m", "Apply captured model one"]);
    const first = cli<{ manifest: { version_id: string } }>([
      "artifacts",
      "version",
      "create",
      "model",
    ]).manifest.version_id;
    await promote(first, "");
    await expect(status("promote completed")).toBeVisible({ timeout: 30_000 });
    await apply(candidate.id);
    git(["add", "-A"]);
    git(["commit", "-m", "Apply captured model two"]);
    const second = cli<{ manifest: { version_id: string } }>([
      "artifacts",
      "version",
      "create",
      "model",
    ]).manifest.version_id;
    expect(second).not.toBe(first);
    await promote(second, `b3:${"f".repeat(64)}`);
    await expect(page.getByRole("alert")).toContainText(
      /(conflict|expected|compare.and.swap)/i,
      { timeout: 30_000 },
    );
    cli([
      "artifacts",
      "get",
      "model",
      "--stage",
      "staging",
      "--output",
      "after-conflict.json",
    ]);
    expect(
      JSON.parse(readFileSync(path.join(repo, "after-conflict.json"), "utf8")),
    ).toEqual(initial);
    await promote(second, first);
    await expect(page.getByRole("alert")).toHaveCount(0);
    await expect(status("promote completed")).toBeVisible({ timeout: 30_000 });
    cli([
      "artifacts",
      "get",
      "model",
      "--stage",
      "staging",
      "--output",
      "promoted.json",
    ]);
    expect(
      JSON.parse(readFileSync(path.join(repo, "promoted.json"), "utf8")),
    ).toEqual(model(candidate.id));

    writeFileSync(
      path.join(repo, "params.json"),
      JSON.stringify({ factor: 99 }),
    );
    writeFileSync(path.join(repo, "data.txt"), "today");
    writeFileSync(
      path.join(repo, "train.cjs"),
      "throw new Error('today must not execute during reproduction');\n",
    );
    git(["add", "params.json", "data.txt", "train.cjs"]);
    git(["commit", "-m", "Move HEAD and replace training inputs"]);
    const moved = git(["rev-parse", "HEAD"]);
    expect(moved).not.toBe(baseline);
    await select(source.id);
    await expect(
      page.getByRole("button", { name: "Reproduce", exact: true }),
    ).toBeEnabled();
    await page.getByRole("button", { name: "Reproduce", exact: true }).click();
    await expect(status("reproduce completed")).toBeVisible({
      timeout: 30_000,
    });
    const replay = list().rows.find(
      (run) => !runs.some((original) => original.id === run.id),
    )!;
    expect(replay).toBeTruthy();
    expect(show(replay.id).snapshot.base_commit).toBe(baseline);
    expect(show(replay.id).snapshot.reproduction?.source_result).toEqual({
      id: source.id,
      digest: source.digest,
    });
    expect(model(replay.id)).toEqual(initial);
    expect(
      JSON.parse(readFileSync(path.join(repo, "params.json"), "utf8")),
    ).toEqual({ factor: 99 });
    expect(readFileSync(path.join(repo, "train.cjs"), "utf8")).toContain(
      "today must not execute",
    );
    writeFileSync(
      path.join(repo, "train.cjs"),
      recorded.replace("revision: 'recorded'", "revision: 'current'"),
    );
    git(["add", "train.cjs"]);
    git(["commit", "-m", "Current resume implementation"]);
    const current = git(["rev-parse", "HEAD"]);
    const resume = cli<ActionPreview>([
      "results",
      "preview",
      source.id,
      "--digest",
      source.digest,
      "--resume",
    ]);
    await page.getByRole("button", { name: "Resume", exact: true }).click();
    const confirmation = page.getByRole("dialog", {
      name: "Resume checkpoint",
      exact: true,
    });
    await expect(confirmation).toContainText(current);
    await expect(confirmation).toContainText(resume.checkpoint!);
    await confirmation
      .getByRole("button", { name: "Resume", exact: true })
      .click();
    await expect(status("resume completed")).toBeVisible({ timeout: 30_000 });
    const resumed = list().rows.find(
      (run) =>
        run.id !== replay.id &&
        !runs.some((original) => original.id === run.id),
    )!;
    expect(show(resumed.id).snapshot.base_commit).toBe(current);
    expect(model(resumed.id)).toEqual({
      factor: 99,
      iteration: 2,
      data: "today",
      revision: "current",
    });
    const lineage = JSON.parse(
      readFileSync(
        path.join(repo, ".crab/workflow/checkpoints", resumed.id, "train.json"),
        "utf8",
      ),
    ) as {
      records: Array<{ id: string; parent: string | null; experiment: string }>;
    };
    expect(
      lineage.records.some(
        (record) =>
          record.parent === resume.checkpoint &&
          record.experiment === source.id,
      ),
    ).toBe(true);
    await page
      .getByRole("button", { name: "Refresh Results", exact: true })
      .click();
    await select(resumed.id);
    await expect(
      page.getByLabel("Results workspace", { exact: true }),
    ).toContainText("9.9");
    await expect(page.getByRole("alert")).toHaveCount(0);
    await select(replay.id);
    await page.getByText("Captured run identities", { exact: true }).click();
    const origin = page.getByText(/^Reproduced from /);
    await expect(origin).toContainText(source.id);
    await expect(origin).toContainText(source.digest);
    await page
      .getByRole("button", { name: "Save report", exact: true })
      .click();
    await prompt(/Save immutable report/, "Native action provenance");
    await expect(page.getByLabel("Report ID")).toHaveText(
      /^[a-f0-9]{64}$/,
    );
    cli(["exp", "rename", source.id, "Renamed after reproduction"]);
    expect(show(source.id).digest).not.toBe(source.digest);
    await page
      .getByRole("button", { name: "Refresh Results", exact: true })
      .click();
    await page
      .getByRole("region", { name: "Metric comparison", exact: true })
      .getByRole("button", { name: replay.id.slice(-8), exact: true })
      .click();
    const identities = page.getByText("Captured run identities", {
      exact: true,
    });
    if (!(await origin.isVisible())) await identities.click();
    await expect(origin).toContainText(source.id);
    await expect(origin).toContainText(source.digest);
    await origin.scrollIntoViewIfNeeded();
    await page.screenshot({
      path: path.join(
        process.env.CRAB_RESULTS_EVIDENCE ?? root,
        "native-action-provenance.png",
      ),
      fullPage: true,
    });

    const manualWorkflow = JSON.parse(
      readFileSync(path.join(repo, "crab.yaml"), "utf8"),
    );
    manualWorkflow.plots = [{ "manual.csv": { x: "step", y: "score", template: "linear" } }];
    const manualAssets = path.join(repo, ".crab/plots");
    mkdirSync(manualAssets, { recursive: true });
    writeFileSync(path.join(manualAssets, "linear.json"), JSON.stringify({
      "$schema": "https://vega.github.io/schema/vega-lite/v5.json",
      title: "Recorded manual curve",
      data: { values: "<DVC_METRIC_DATA>" }, mark: "line",
      encoding: {
        x: { field: "<DVC_METRIC_X>", type: "quantitative" },
        y: { field: "<DVC_METRIC_Y>", type: "quantitative" },
        color: "<DVC_METRIC_COLOR>",
      },
    }));
    writeFileSync(path.join(repo, "crab.yaml"), JSON.stringify(manualWorkflow));
    writeFileSync(path.join(repo, "params.json"), JSON.stringify({ factor: 7.5 }));
    writeFileSync(path.join(repo, "metrics.json"), JSON.stringify({ score: 0.75 }));
    writeFileSync(path.join(repo, "manual.csv"), "step,score\n0,0.5\n1,0.75\n");
    const manualModel = JSON.stringify({ trained: "outside Crab" });
    writeFileSync(path.join(repo, "model.json"), manualModel);
    const manual = cli<{ exp_id: string }>(["exp", "save", "--name", "Manual observation"]);
    const saved = show(manual.exp_id);
    expect(saved.snapshot.status).toBe("saved");
    expect(saved.snapshot.stages).toEqual([]);
    expect(saved.snapshot.reproduction).toBeNull();
    expect(saved.snapshot.attachments.some((attachment) => attachment.kind === "log")).toBe(false);
    expect(saved.snapshot.attachments.some((attachment) => attachment.kind === "template" && attachment.state === "present")).toBe(true);
    rmSync(manualAssets, { recursive: true });
    writeFileSync(path.join(repo, "model.json"), "today's model");
    writeFileSync(path.join(repo, "params.json"), JSON.stringify({ factor: 99 }));
    await chooseSelect(page.getByLabel("Open immutable report"), "");
    await page.getByRole("button", { name: "Refresh Results", exact: true }).click();
    const comparisons = page.getByRole("checkbox", { name: /^Compare / });
    for (let index = 0; index < await comparisons.count(); index++) {
      if (await comparisons.nth(index).isChecked()) await comparisons.nth(index).uncheck();
    }
    await page.getByRole("checkbox", { name: "Compare Manual observation", exact: true }).check();
    await page.getByRole("region", { name: "Metric comparison", exact: true })
      .getByRole("button", { name: `Manual observation (${manual.exp_id.slice(-8)})`, exact: true }).click();
    await expect(page.getByRole("region", { name: "Metric comparison", exact: true }))
      .toContainText("Saved workspace observations do not record execution completion");
    await expect(page.getByRole("button", { name: "Apply", exact: true })).toBeEnabled();
    await expect(page.getByRole("button", { name: "Create branch", exact: true })).toBeEnabled();
    for (const name of ["Resume", "Reproduce", "Promote artifact"]) {
      await expect(page.getByRole("button", { name, exact: true })).toBeDisabled();
    }
    await expect(page.getByRole("row").filter({
      has: page.getByRole("checkbox", { name: "Compare Manual observation", exact: true }),
    })).toContainText("0.75");
    const manualChart = page.getByRole("region", { name: "manual.csv", exact: true });
    await expect(manualChart.locator("svg .mark-line path")).toHaveCount(1);
    await expect(manualChart.getByText("Recorded manual curve", { exact: true })).toBeVisible();
    await page.getByRole("button", { name: "Apply", exact: true }).scrollIntoViewIfNeeded();
    const captureSaved = async (name: string, width: number, subject = page.getByRole("button", { name: "Apply", exact: true })) => {
      await page.setViewportSize({ width, height: 1000 });
      const toasts = page.getByRole("button", { name: "Close toast", exact: true });
      await page.mouse.move(0, 0);
      await expect(toasts).toHaveCount(0, { timeout: 30_000 });
      await subject.scrollIntoViewIfNeeded();
      await page.screenshot({
        path: path.join(process.env.CRAB_RESULTS_EVIDENCE ?? root, name),
        fullPage: true,
      });
    };
    await captureSaved("native-saved-observation.png", 1280);
    await captureSaved("native-saved-observation-narrow.png", 900);
    await captureSaved("native-saved-template.png", 1280, manualChart);
    await captureSaved("native-saved-template-narrow.png", 900, manualChart);
    await page.getByRole("button", { name: "Apply", exact: true }).click();
    const savedPreview = page.getByRole("dialog", { name: "Apply captured workspace", exact: true });
    await expect(savedPreview).toContainText("model.json");
    await savedPreview.getByRole("button", { name: "Apply workspace", exact: true }).click();
    await expect(status("apply completed")).toBeVisible({ timeout: 30_000 });
    expect(readFileSync(path.join(repo, "model.json"), "utf8")).toBe(manualModel);
    expect(JSON.parse(readFileSync(path.join(repo, "params.json"), "utf8"))).toEqual({ factor: 7.5 });

    writeFileSync(path.join(repo, ".crab/workflow/exp", `${manual.exp_id}.workspace`, "model.json"), "altered saved model");
    writeFileSync(path.join(repo, "model.json"), "keep current model");
    await page.getByRole("button", { name: "Apply", exact: true }).click();
    const refusal = page.getByRole("alert");
    await expect(refusal).toContainText("do not match the selected result", { timeout: 30_000 });
    await expect(refusal).toContainText("Apply preview refused. Workspace files were not changed.");
    await expect(refusal).toContainText("Restore or retrieve the original experiment snapshot");
    await expect(refusal).toContainText("model.json");
    await expect(refusal).not.toContainText('"timestamp"');
    const diagnostics = refusal.locator("details");
    await expect(diagnostics.locator("pre")).toBeHidden();
    await diagnostics.locator("summary").focus();
    await page.keyboard.press("Enter");
    await expect(diagnostics.locator("pre")).toBeVisible();
    await expect(diagnostics.locator("pre")).toContainText('"code": "CRAB-E0020"');
    await page.keyboard.press("Enter");
    await expect(diagnostics.locator("pre")).toBeHidden();
    await expect(savedPreview).toHaveCount(0);
    expect(readFileSync(path.join(repo, "model.json"), "utf8")).toBe("keep current model");
    await captureSaved("native-saved-integrity-failure.png", 1280);
    await captureSaved("native-saved-integrity-failure-narrow.png", 900);
    await refusal.getByRole("button", { name: "Refresh view", exact: true }).click();
    await expect(refusal).toHaveCount(0);
    await expect(savedPreview).toHaveCount(0);
    expect(readFileSync(path.join(repo, "model.json"), "utf8")).toBe("keep current model");
    await page.getByRole("button", { name: "Apply", exact: true }).click();
    await expect(refusal).toContainText("do not match the selected result");
    expect(readFileSync(path.join(repo, "model.json"), "utf8")).toBe("keep current model");
    writeFileSync(path.join(repo, ".crab/workflow/exp", `${manual.exp_id}.workspace`, "model.json"), manualModel);
    await page.getByRole("button", { name: "Apply", exact: true }).click();
    await expect(savedPreview).toContainText("model.json");
    await savedPreview.getByRole("button", { name: "Apply workspace", exact: true }).click();
    await expect(status("apply completed")).toBeVisible({ timeout: 30_000 });
    await expect(refusal).toHaveCount(0);
    expect(readFileSync(path.join(repo, "model.json"), "utf8")).toBe(manualModel);

    mkdirSync(manualAssets, { recursive: true });
    const oversized = openSync(path.join(manualAssets, "linear.json"), "w");
    try {
      ftruncateSync(oversized, 64 * 1024 * 1024 + 1);
    } finally {
      closeSync(oversized);
    }
    cli(["exp", "save", "--name", "Uncaptured template"]);
    await page.getByRole("button", { name: "Refresh Results", exact: true }).click();
    await page.getByRole("checkbox", { name: "Compare Manual observation", exact: true }).uncheck();
    await page.getByRole("checkbox", { name: "Compare Uncaptured template", exact: true }).check();
    await expect(manualChart).toContainText("Plot evidence was not captured for this run.");
    await expect(manualChart).toContainText("save or run a new experiment");
    const workingResults = page.getByLabel("Results workspace", { exact: true });
    await expect(workingResults).toContainText("Template linear was not retained: result file exceeds the read limit of 67108864 bytes.");
    await expect(workingResults).toContainText("save a new observation with crab exp save");
    await expect(workingResults).not.toContainText("workflow_results_invalid");
    await expect(manualChart.locator("svg")).toHaveCount(0);
    await expect(page.getByRole("region", { name: "Metric comparison", exact: true })).toContainText("0.75");
    await captureSaved("native-saved-template-unavailable.png", 1280, manualChart);
    await captureSaved("native-saved-template-unavailable-narrow.png", 900, manualChart);
    await page.getByRole("button", { name: /^template · templates\/unavailable\// }).click();
    await expect(workingResults.getByText(/Template linear: configuration error/)).toBeVisible();
    await expect(workingResults.getByText(/Template linear: configuration error/)).toContainText("workflow_results_invalid");

    const report = cli<{ id: string }>(["results", "report", "create", manual.exp_id, "--title", "Saved render asset"]);
    cli(["results", "report", "push", report.id]);
    await page.getByRole("checkbox", { name: "Compare Uncaptured template", exact: true }).uncheck();
    renameSync(path.join(repo, ".crab/workflow/results"), path.join(root, "results-before-cold-retrieval"));
    cli(["results", "report", "pull", report.id]);
    await page.getByRole("button", { name: "Refresh Results", exact: true }).click();
    await chooseSelect(page.getByLabel("Open immutable report"), report.id);
    await expect(manualChart.getByText("Recorded manual curve", { exact: true })).toBeVisible();
    await expect(manualChart.locator("svg .mark-line path")).toHaveCount(1);
    await expect(page.getByRole("button", { name: "Apply", exact: true })).toBeDisabled();
    await captureSaved("native-saved-template-cold.png", 1280, manualChart);
    await captureSaved("native-saved-template-cold-narrow.png", 900, manualChart);

    const importedId = manual.exp_id.slice(0, -12) + randomUUID().replaceAll("-", "").slice(-12);
    const importedMetadata: Record<string, unknown> = JSON.parse(readFileSync(
      path.join(repo, ".crab/workflow/exp", `${manual.exp_id}.meta.json`), "utf8",
    ));
    Object.assign(importedMetadata, {
      exp_id: importedId, name: "Imported before Results", status: "success",
      metrics: { "metrics.json": { score: 0.375 } },
      cli_args: ["recorded-private-argument-must-not-be-replayed"],
    });
    writeFileSync(path.join(repo, ".crab/workflow/exp", `${importedId}.meta.json`), JSON.stringify(importedMetadata));
    writeFileSync(path.join(repo, "metrics.json"), JSON.stringify({ score: 999 }));
    cli(["results", "migrate", importedId]);
    expect(show(importedId).snapshot.origin).toBe("experiment_metadata");
    expect(show(importedId).snapshot.parameters).toEqual([]);
    expect(show(importedId).snapshot.stages).toEqual([]);
    await chooseSelect(page.getByLabel("Open immutable report"), "");
    await page.getByRole("button", { name: "Refresh Results", exact: true }).click();
    const importedChoice = page.getByRole("checkbox", { name: "Compare Imported before Results", exact: true });
    const importedRow = page.getByRole("row").filter({ has: importedChoice });
    await expect(importedRow).toContainText("success (imported)");
    await importedChoice.check();
    const importedComparison = page.getByRole("region", { name: "Metric comparison", exact: true });
    await importedComparison.getByRole("button", { name: `Imported before Results (${importedId.slice(-8)})`, exact: true }).click();
    await expect(importedComparison).toContainText("0.375");
    await expect(importedComparison).toContainText("Imported experiment metadata");
    await expect(workingResults.locator("svg .mark-line path")).toHaveCount(0);
    await expect(workingResults).toContainText("Resolved inputs, stage attempts, plots, and original artifact bytes were not captured.");
    const importedActionNotice = workingResults.getByText(/^Imported experiment metadata has no verified execution/);
    await expect(importedActionNotice).toBeVisible();
    for (const name of ["Apply", "Create branch", "Resume", "Reproduce", "Promote artifact"]) {
      await expect(page.getByRole("button", { name, exact: true })).toBeDisabled();
    }
    await expect(page.getByRole("button", { name: "Publish", exact: true })).toBeEnabled();
    await captureSaved("native-imported-metadata.png", 1280, importedComparison);
    await captureSaved("native-imported-metadata-narrow.png", 900, importedComparison);
    const importedReport = cli<{ id: string }>(["results", "report", "create", importedId, "--title", "Imported recorded values"]);
    cli(["results", "report", "push", importedReport.id]);
    await importedChoice.uncheck();
    renameSync(path.join(repo, ".crab/workflow/results"), path.join(root, "results-before-imported-retrieval"));
    cli(["results", "report", "pull", importedReport.id]);
    await page.getByRole("button", { name: "Refresh Results", exact: true }).click();
    await chooseSelect(page.getByLabel("Open immutable report"), importedReport.id);
    await expect(importedComparison).toContainText("0.375");
    await expect(importedComparison).toContainText("Imported experiment metadata");
    await expect(workingResults).not.toContainText("recorded-private-argument-must-not-be-replayed");
    await expect(importedActionNotice).toBeVisible();
    await expect(importedActionNotice).toContainText("Run or save a new experiment");
    await captureSaved("native-imported-metadata-cold.png", 1280, importedComparison);
    await captureSaved("native-imported-metadata-cold-narrow.png", 900, importedComparison);
  } catch (error) {
    const page = await app?.firstWindow();
    if (page) {
      const output = process.env.CRAB_RESULTS_EVIDENCE ?? root;
      await page
        .screenshot({
          path: path.join(output, "native-actions-failure.png"),
          fullPage: true,
        })
        .catch(() => undefined);
      writeFileSync(
        path.join(output, "native-actions-text.txt"),
        await page.locator("body").innerText(),
      );
    }
    throw error;
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
