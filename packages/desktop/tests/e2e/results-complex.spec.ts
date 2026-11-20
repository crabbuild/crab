import { chooseSelect } from "../helpers/select";
import AxeBuilder from "@axe-core/playwright";
import {
  test,
  expect,
  _electron as electron,
  type ElectronApplication,
  type Page,
} from "@playwright/test";
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import {
  appendFileSync,
  cpSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import path from "node:path";
import { randomUUID } from "node:crypto";
import { call, closeFixtureDaemon, packagedExecutable } from "./packaged-app";
import { PROTOCOL_VERSION } from "../../electron/daemon/protocol";

const execute = promisify(execFile);

test("complex trained Results remain interactive through native restart and cold retrieval", async () => {
  test.skip(
    !process.env.CRAB_SDK_TEST_S3_BUCKET,
    "Requires owned writable object storage",
  );
  test.setTimeout(360_000);
  const root = mkdtempSync(
    path.join(process.env.CARGO_TARGET_DIR!, "native-complex-"),
  );
  const output =
    process.env.CRAB_RESULTS_EVIDENCE ??
    test.info().outputPath("native-complex");
  const repo = path.join(root, "repo"),
    cold = path.join(root, "cold"),
    home = path.join(root, "home");
  mkdirSync(home);
  mkdirSync(output, { recursive: true });
  const source = path.resolve(
    __dirname,
    "../../../../packages/web/public/examples/ml-results-lab",
  );
  cpSync(source, repo, {
    recursive: true,
    filter: (file) => !file.split(path.sep).includes("__pycache__"),
  });
  const env = {
    ...process.env,
    HOME: home,
    USERPROFILE: home,
    CRAB_CACHE_DIR: path.join(root, "cache"),
    GIT_CONFIG_GLOBAL: path.join(root, "gitconfig"),
    GIT_CONFIG_NOSYSTEM: "1",
  };
  writeFileSync(env.GIT_CONFIG_GLOBAL, "");
  const python = process.env.CRAB_SDK_TEST_PYTHON;
  if (!python)
    throw new Error(
      "Native complex fixture requires a pinned Python interpreter",
    );
  const workflow = path.join(repo, "crab.yaml");
  // Pin the existing example commands to the fixture interpreter so Results
  // records exact runtime bytes on every native platform.
  writeFileSync(
    workflow,
    readFileSync(workflow, "utf8").replace(
      /^    cmd: python3 (.+)$/gm,
      (_line, args: string) =>
        "    cmd:\n      argv: " + JSON.stringify([python, ...args.split(" ")]),
    ),
  );
  const resources =
    process.platform === "darwin"
      ? path.resolve(path.dirname(packagedExecutable()), "../Resources")
      : path.join(path.dirname(packagedExecutable()), "resources");
  const crab = path.join(
    resources,
    "agent",
    process.platform === "win32" ? "crab.exe" : "crab",
  );
  const git = async (cwd: string, ...args: string[]) =>
    execute(
      "git",
      [
        "-c",
        "user.name=Native Results lab",
        "-c",
        "user.email=results@example.invalid",
        "-c",
        "commit.gpgsign=false",
        ...args,
      ],
      { cwd, env },
    );
  const cli = async (cwd: string, ...args: string[]) =>
    JSON.parse(
      (
        await execute(crab, [...args, "--json"], {
          cwd,
          env,
          maxBuffer: 32 * 1024 * 1024,
        })
      ).stdout,
    ).data;
  await git(repo, "init", "--initial-branch=main");
  const remote = `crab://${process.env.CRAB_SDK_TEST_S3_BUCKET}/native-complex/${randomUUID()}`;
  await cli(repo, "init", remote);
  await git(repo, "add", ".");
  await git(repo, "commit", "-m", "Add complex Results lab");
  let app: ElectronApplication | undefined;
  const screenshots: string[] = [];
  const lifecycle = path.join(output, "native-lifecycle.jsonl");
  const record = (phase: string, pid?: number) =>
    appendFileSync(
      lifecycle,
      JSON.stringify({ phase, pid, at: Date.now() }) + "\n",
    );
  const launch = async () => {
    record("launch.start");
    app = await electron.launch({
      executablePath: packagedExecutable(),
      args: [`--user-data-dir=${path.join(root, "profile")}`],
      env,
    });
    record("launch.connected", app.process().pid);
    const child = app.process();
    child.once("exit", () => record("child.exit", child.pid));
    child.once("close", () => record("child.close", child.pid));
    await app.evaluate(({ app }, file) => {
      const fs = process.getBuiltinModule(
        "node:fs",
      ) as typeof import("node:fs");
      const record = (phase: string) =>
        fs.appendFileSync(
          file,
          JSON.stringify({ phase, pid: process.pid, at: Date.now() }) + "\n",
        );
      for (const event of ["before-quit", "will-quit"] as const)
        app.prependListener(event, () => record(event));
      process.once("exit", () => record("main.node.exit"));
    }, lifecycle);
    record("firstWindow.start", app.process().pid);
    const page = await app.firstWindow();
    record("firstWindow.end", app.process().pid);
    await page.setViewportSize({ width: 1600, height: 1100 });
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
  const open = async (page: Page, directory: string) => {
    const handle = await call(page, "open_local", {
      repo_id: directory,
      path: directory,
    });
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
  };
  const close = async (phase: string) => {
    const pid = app!.process().pid;
    record(phase + ".close.start", pid);
    await app!.close();
    record(phase + ".close.end", pid);
    app = undefined;
  };
  const shot = async (
    page: Page,
    name: string,
    subject?: ReturnType<Page["locator"]>,
  ) => {
    if (subject)
      await subject.evaluate((element) =>
        element.scrollIntoView({ block: "start" }),
      );
    await page.screenshot({
      path: path.join(output, name + ".png"),
      animations: "disabled",
    });
    screenshots.push(name + ".png");
  };
  const prompt = async (page: Page, name: RegExp, value: string) => {
    const dialog = page.getByRole("dialog", { name });
    await dialog.getByRole("textbox").fill(value);
    await dialog.getByRole("button", { name: "OK", exact: true }).click();
  };
  const errors: string[] = [];
  let proof: Record<string, unknown> | undefined;
  try {
    let page = await launch();
    page.on("pageerror", (error) => errors.push(error.message));
    await page.getByRole("button", { name: "Skip", exact: true }).click();
    await page.getByRole("button", { name: "No thanks", exact: true }).click();
    await open(page, repo);
    const configurations = [
      { capacity: "linear", l2: "0.001" },
      { capacity: "linear", l2: "0.1" },
      { capacity: "interactions", l2: "0.001" },
      { capacity: "interactions", l2: "0.1" },
    ];
    // Queue through the native contract; the user-facing Start action executes
    // the actual two-dimensional sweep with the bundled CLI.
    const queued = await call(page, "workflow_exp_enqueue", {
      repo_id: repo,
      overrides: configurations.map((c) => ({
        "train.capacity": c.capacity,
        "train.l2": c.l2,
      })),
    });
    expect(queued.queued).toBe(4);
    const ids = queued.ids as string[];
    expect(ids).toHaveLength(4);
    await page.getByRole("tab", { name: "Queue", exact: true }).click();
    await expect(page.getByText("Pending", { exact: true })).toHaveCount(4);
    await shot(page, "native-queue");
    await page.getByRole("button", { name: /Start \(jobs:/ }).click();
    await expect(page.getByText("Done", { exact: true })).toHaveCount(4, {
      timeout: 120_000,
    });
    for (const [i, c] of configurations.entries())
      await cli(repo, "exp", "rename", ids[i], `${c.capacity} · L2 ${c.l2}`);
    await page.getByRole("tab", { name: "Results", exact: true }).click();
    await page.getByRole("button", { name: "Columns", exact: true }).click();
    const keep = new Set([
      "param · params.json/train/capacity",
      "param · params.json/train/l2",
      "metric · metrics/classification.json/f1",
      "metric · metrics/classification.json/brier",
      "metric · metrics/forecast.json/mae",
      "metric · metrics/forecast.json/rmse",
    ]);
    for (const checkbox of await page
      .getByRole("checkbox", { name: /^(metric|param) · / })
      .all()) {
      const label = (await checkbox.locator("..").textContent())!.trim();
      await checkbox.setChecked(keep.has(label));
    }
    await page.keyboard.press("Escape");
    const selections = page.getByRole("checkbox", { name: /^Compare / });
    await expect(selections).toHaveCount(4);
    for (const selection of await selections.all()) await selection.check();
    await chooseSelect(page.getByLabel("Baseline run"), ids[0]);
    const baseline = page.getByRole("combobox", {
      name: "Baseline run",
      exact: true,
    });
    await baseline.focus();
    await baseline.press("ArrowDown");
    await expect(page.getByRole("listbox")).toBeVisible();
    await page.keyboard.press("Escape");
    await expect(baseline).toBeFocused();
    const workspace = page.getByLabel("Results workspace", { exact: true });
    for (const [metric, direction] of [
      ["metrics/classification.json/f1", "maximize"],
      ["metrics/classification.json/brier", "minimize"],
      ["metrics/forecast.json/mae", "minimize"],
      ["metrics/forecast.json/rmse", "minimize"],
    ])
      await chooseSelect(
        workspace.getByLabel("Objective " + metric),
        direction,
      );
    for (const id of ids) {
      await workspace
        .getByRole("region", { name: "Metric comparison", exact: true })
        .getByRole("button", { name: new RegExp(id.slice(-8)) })
        .click();
      const hash = workspace.getByLabel("Dataset hash");
      if (!(await hash.isVisible()))
        await workspace
          .locator("summary")
          .filter({ hasText: "Dataset and split identity" })
          .click();
      const digest = await workspace
        .locator("datalist option")
        .filter({ hasText: "data/benchmark.json" })
        .first()
        .getAttribute("value");
      expect(digest).toMatch(/^[0-9a-f]{64}$/);
      await hash.fill(digest!);
      await workspace
        .getByLabel("Dataset split")
        .fill("classification:test[1400:1800];forecast:test[105:126]");
      await workspace.getByLabel("Evaluator identity").fill("results-lab-v1");
      await workspace
        .getByRole("button", { name: "Set identity", exact: true })
        .click();
    }
    await expect(
      workspace
        .getByRole("region", { name: "Metric comparison", exact: true })
        .locator(".text-success, .text-warning"),
    ).toHaveCount(12);
    await expect(workspace.locator(".crab-results-chart > svg")).toHaveCount(5);
    await expect(workspace.getByRole("alert")).toHaveCount(0);
    await expect(workspace.locator("iframe, frame")).toHaveCount(0);
    const accessibility = await new AxeBuilder({ page })
      .setLegacyMode()
      .include('[aria-label="Results workspace"]')
      .withTags(["wcag2a", "wcag2aa"])
      .analyze();
    expect(
      accessibility.violations.map(({ id, nodes }) => ({
        id,
        nodes: nodes.map((node) => node.failureSummary),
      })),
    ).toEqual([]);
    await workspace.locator(":scope > .overflow-auto").evaluate((e) => {
      e.scrollTop = 0;
    });
    await shot(page, "native-comparison");
    const section = (title: string) =>
      workspace.locator("section").filter({
        has: page.getByRole("heading", { name: new RegExp("^" + title) }),
      });
    for (const [title, name] of [
      ["Learning", "native-learning"],
      ["Classification", "native-classification"],
      ["Cohorts", "native-cohorts"],
      ["Demand", "native-forecast"],
      ["Decision lab", "native-decision"],
    ])
      await shot(page, name, section(title));
    for (const title of [
      "Classification",
      "Cohorts",
      "Demand",
      "Decision lab",
    ]) {
      const chart = section(title);
      await expect
        .poll(() =>
          chart.evaluate(
            (element) =>
              element
                .querySelector(".crab-results-chart > svg")!
                .getBoundingClientRect().width <=
              element.clientWidth + 1,
          ),
        )
        .toBe(true);
    }
    const decision = section("Decision lab");
    const counts = async () =>
      (await decision.locator(".mark-text.role-mark text").allTextContents())
        .map(Number)
        .filter(Number.isFinite);
    expect((await counts()).reduce((a, b) => a + b, 0)).toBe(1600);
    const original = await counts();
    await decision.locator('input[type="range"]').fill("0.8");
    await expect.poll(counts).not.toEqual(original);
    await decision.locator("select").selectOption("Established");
    await expect
      .poll(async () => (await counts()).reduce((a, b) => a + b, 0))
      .toBe(532);
    await shot(page, "native-decision-filtered", decision);
    await decision.locator("svg").scrollIntoViewIfNeeded();
    const boxes = await Promise.all(
      (await decision.locator("g.mark-rect.role-mark").all()).map((mark) =>
        mark.boundingBox(),
      ),
    );
    const bars = boxes.find((box) => box && box.width > 0 && box.height > 0);
    expect(bars).toBeDefined();
    await page.mouse.move(
      bars!.x + bars!.width * 0.15,
      bars!.y + bars!.height * 0.4,
    );
    await page.mouse.down();
    await page.mouse.move(
      bars!.x + bars!.width * 0.6,
      bars!.y + bars!.height * 0.75,
      { steps: 12 },
    );
    await page.mouse.up();
    await expect
      .poll(async () => (await counts()).reduce((a, b) => a + b, 0))
      .toBeLessThan(532);
    const brushed = (await counts()).reduce((a, b) => a + b, 0);
    expect(brushed).toBeGreaterThan(0);
    await shot(page, "native-decision-brushed", decision);
    const forecast = section("Demand");
    await expect(forecast.locator(".mark-symbol.role-mark path")).toHaveCount(
      504,
    );
    await forecast.locator("select").selectOption("test");
    await expect(forecast.locator(".mark-symbol.role-mark path")).toHaveCount(
      84,
    );
    await shot(page, "native-forecast-test", forecast);
    const learning = section("Learning");
    await learning
      .getByRole("button", { name: "Zoom in", exact: true })
      .click();
    await expect(
      learning.getByRole("spinbutton", { name: /X minimum/ }),
    ).toHaveValue("20");
    await shot(page, "native-learning-zoom", learning);
    await close("working comparison");
    page = await launch();
    page.on("pageerror", (e) => errors.push(e.message));
    await open(page, repo);
    await expect(
      page.getByRole("checkbox", { name: /^Compare /, checked: true }),
    ).toHaveCount(4);
    await expect(page.getByLabel(/^X minimum /)).toHaveValue("20");
    await page
      .getByRole("button", { name: "Save report", exact: true })
      .click();
    await prompt(page, /Save immutable report/, "Native Results lab");
    await expect(page.getByLabel("Report ID")).toHaveText(/^[0-9a-f]{64}$/);
    const report = await page.getByLabel("Report ID").innerText();
    await shot(page, "native-saved-report");
    await page
      .getByRole("button", { name: "Publish report", exact: true })
      .click();
    await expect(
      page.getByRole("status").filter({ hasText: "Report published" }),
    ).toBeVisible({ timeout: 90_000 });
    await close("published report");
    await git(root, "clone", repo, cold);
    await git(cold, "remote", "set-url", "origin", remote);
    rmSync(path.join(cold, "crab.yaml"));
    rmSync(path.join(cold, "templates"), { recursive: true });
    page = await launch();
    page.on("pageerror", (e) => errors.push(e.message));
    await open(page, cold);
    await page
      .getByRole("button", { name: "Retrieve report", exact: true })
      .click();
    await prompt(page, /Open immutable report/, report);
    await expect(page.getByLabel("Report ID")).toHaveText(report);
    await expect(page.locator(".crab-results-chart > svg")).toHaveCount(5);
    await expect(page.getByRole("alert")).toHaveCount(0);
    await shot(page, "native-cold-report");
    const recovered = page.getByLabel("Results workspace", { exact: true });
    const recoveredDecision = recovered
      .locator("section")
      .filter({ has: page.getByRole("heading", { name: /^Decision lab/ }) });
    await recoveredDecision.locator('input[type="range"]').fill("0.7");
    await recoveredDecision.locator("select").selectOption("Cross-border");
    await expect
      .poll(async () =>
        (
          await recoveredDecision
            .locator(".mark-text.role-mark text")
            .allTextContents()
        )
          .map(Number)
          .reduce((a, b) => a + b, 0),
      )
      .toBe(536);
    await shot(page, "native-cold-interactive", recoveredDecision);
    expect(errors).toEqual([]);
    await close("retrieved report");
    proof = {
      status: "passed",
      runs: 4,
      ids,
      report,
      charts: 5,
      brushed,
      forecast_test_points: 84,
      screenshots,
      errors,
      root,
    };
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
  writeFileSync(
    path.join(output, "native-complex-proof.json"),
    JSON.stringify(proof, null, 2) + "\n",
  );
});
