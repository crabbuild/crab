import { chooseSelect } from "../helpers/select";
import { test, expect, _electron as electron } from "@playwright/test";
import { execFileSync } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import path from "node:path";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import { parse } from "yaml";
import { call, closeFixtureDaemon, packagedExecutable } from "./packaged-app";

test("complex workflows preserve templates, execute dependencies, cache, retry, and scale layout", async () => {
  test.setTimeout(180_000);
  const root = mkdtempSync(path.join(process.env.CARGO_TARGET_DIR ?? tmpdir(), "desktop-complex-workflows-"));
  const repo = path.join(root, "repo");
  const home = path.join(root, "home");
  mkdirSync(repo);
  mkdirSync(home);
  mkdirSync(path.join(repo, "work"));
  execFileSync("git", ["init", "--initial-branch=main"], { cwd: repo });
  const cmd = (script: string) => ({ argv: [process.execPath, "-e", script] });
  const workflow = {
    vars: [{ marker: "verified" }],
    defaults: { env: "empty", retry: { max_attempts: 2, initial_backoff: "0s", on_exit_codes: [7] } },
    stages: {
      prepare: { foreach: ["a", "b"], do: { wdir: "work", cmd: cmd("require('fs').writeFileSync('${item}.txt','${item}'); console.log('template-log')"), outs: ["${item}.txt"], resources: { cpu: 1 }, meta: { owner: "fixture" } } },
      build: { matrix: { os: ["linux", "mac"], arch: ["arm", "x64"] }, cmd: cmd("require('fs').writeFileSync('${item.os}-${item.arch}.txt','${item.os}-${item.arch}')"), outs: ["${item.os}-${item.arch}.txt"] },
      merge: { cmd: cmd("const fs=require('fs');fs.writeFileSync('merged.txt',fs.readFileSync('work/a.txt','utf8')+fs.readFileSync('work/b.txt','utf8'))"), deps: ["work/a.txt", "work/b.txt"], outs: ["merged.txt"] },
      retry: { cmd: cmd("const fs=require('fs');const n=fs.existsSync('attempts')?Number(fs.readFileSync('attempts'))+1:1;fs.writeFileSync('attempts',String(n));if(n===1)process.exit(7);fs.writeFileSync('retried.txt',fs.readFileSync('merged.txt'))"), deps: ["merged.txt"], outs: ["retried.txt"] },
      conditional: { condition: { file_exists: "enable-optional" }, cmd: cmd("require('fs').writeFileSync('optional.txt','unexpected')"), outs: ["optional.txt"] },
    },
    workflows: { release: { stages: { publish: { cmd: cmd("const fs=require('fs');fs.writeFileSync('result.txt',fs.readFileSync('retried.txt','utf8')+'-${marker}');console.log('published')"), deps: ["retried.txt", "linux-arm.txt", "linux-x64.txt", "mac-arm.txt", "mac-x64.txt"], outs: ["result.txt"] } } } },
  };
  writeFileSync(path.join(repo, "crab.yaml"), JSON.stringify(workflow));
  writeFileSync(path.join(repo, "failure.workflow.yaml"), JSON.stringify({ stages: {
    fail: { cmd: cmd("console.error('intentional-failure');process.exit(17)"), outs: ["missing.txt"] },
    ...Object.fromEntries(Array.from({ length: 10 }, (_, i) => [`survivor_${i}`, { cmd: cmd(`setTimeout(()=>require('fs').writeFileSync('survivor_${i}.txt','done'),150)`), outs: [`survivor_${i}.txt`] }])),
    blocked: { cmd: cmd("require('fs').writeFileSync('must-not-exist.txt','bad')"), deps: ["missing.txt"], outs: ["must-not-exist.txt"] },
  } }));
  const largeStages = Object.fromEntries(
    Array.from({ length: 64 }, (_, i) => [
      `stage_${i}`,
      {
        cmd: cmd(`require('fs').writeFileSync('large_${i}.txt','${i}')`),
        outs: [`large_${i}.txt`],
        deps:
          i < 8
            ? []
            : [0, 1, 3].map(
                (offset) =>
                  `large_${(Math.floor(i / 8) - 1) * 8 + ((i + offset) % 8)}.txt`,
              ),
      },
    ]),
  );
  writeFileSync(path.join(repo, "large.workflow.yaml"), JSON.stringify({ stages: largeStages }));
  const application = await electron.launch({ executablePath: packagedExecutable(), args: [`--user-data-dir=${path.join(root, "profile")}`],
    env: { ...process.env, HOME: home, USERPROFILE: home, CRAB_CACHE_DIR: path.join(root, "cache") } });
  try {
    const page = await application.firstWindow();
    const errors: string[] = [];
    page.on("pageerror", error => errors.push(error.message));
    await page.getByRole("button", { name: "Skip", exact: true }).click();
    await page.getByRole("button", { name: "No thanks", exact: true }).click();
    await expect.poll(async () => { try { return (await call(page, "ping", {})).pong; } catch { return undefined; } }).toBeTruthy();
    const handle = await call(page, "open_local", { repo_id: "complex", path: repo });
    await page.evaluate(({ handle, repo }) => window.dispatchEvent(new CustomEvent("crab:repo-opened", { detail: { handle, path: repo } })), { handle, repo });
    const primary = page.getByRole("navigation", { name: "Primary", exact: true });
    const openWorkflows = () => primary.getByRole("button", { name: "Data Workflows", exact: true }).click();
    await openWorkflows();
    const sidebar = page.getByRole("complementary", { name: "Workflows", exact: true });
    const files = sidebar.getByRole("navigation", { name: "Workflow files", exact: true });
    const select = (name: string) => files.getByRole("button").filter({ hasText: name }).click();
    await select("crab.yaml");
    await expect(page.locator(".react-flow__node")).toHaveCount(6);
    await expect(page.getByRole("status").filter({ hasText: "6 stages" })).toContainText("10 executions");
    await chooseSelect(
      page.getByRole("combobox", { name: "Find stage", exact: true }),
      "prepare",
    );
    const inspector = page.getByRole("complementary", { name: "Stage properties", exact: true });
    await expect(inspector).toContainText("Changes apply to every execution");
    const edited = [...workflow.stages.prepare.do.cmd.argv];
    edited[2] += ";console.log('edited-template')";
    await inspector.getByRole("textbox", { name: "Stage command", exact: true }).fill(JSON.stringify(edited));
    await page.getByRole("button", { name: "Run All", exact: true }).click();
    await expect(page.getByText("Completed", { exact: true })).toBeVisible({ timeout: 45_000 });
    expect(readFileSync(path.join(repo, "result.txt"), "utf8")).toBe("ab-verified");
    expect(readFileSync(path.join(repo, "attempts"), "utf8")).toBe("2");
    expect(existsSync(path.join(repo, "optional.txt"))).toBe(false);
    const saved = parse(readFileSync(path.join(repo, "crab.yaml"), "utf8"));
    expect(saved.defaults).toEqual(workflow.defaults);
    expect(saved.stages.prepare.foreach).toEqual(["a", "b"]);
    expect(saved.stages.prepare.do.resources).toEqual({ cpu: 1 });
    expect(saved.stages.prepare.do.meta).toEqual({ owner: "fixture" });
    expect(saved.workflows.release.stages.publish).toBeTruthy();
    const output = page.getByRole("region", { name: "Workflow output", exact: true });
    await expect(output).toContainText("edited-template");
    await expect(output).toContainText("prepare@a: Completed");
    await expect(output).toContainText("prepare@b: Completed");
    await expect(page.locator('.react-flow__node[data-id="prepare"]')).toContainText("done");
    await inspector.getByRole("button", { name: "Close", exact: true }).click();
    await page.getByRole("button", { name: "Fit workflow to view", exact: true }).click();
    await page.screenshot({ path: test.info().outputPath("complex-workflow-completed.png") });
    await page.getByRole("button", { name: "Run All", exact: true }).click();
    await expect(output).toContainText("publish: Cached");
    await expect(page.getByText("Completed", { exact: true })).toBeVisible();
    await expect(output).toContainText("prepare@a: Cached");
    expect(readFileSync(path.join(repo, "attempts"), "utf8")).toBe("2");

    await select("large.workflow.yaml");
    await expect(page.locator(".react-flow__node")).toHaveCount(64);
    const positions = await page.locator(".react-flow__node").evaluateAll(nodes => nodes.map(node => {
      const rect = node.getBoundingClientRect(); return { x: rect.x, y: rect.y, width: rect.width, height: rect.height };
    }));
    for (let i = 0; i < positions.length; i++) for (let j = i + 1; j < positions.length; j++) {
      const a = positions[i], b = positions[j];
      expect(a.x + a.width <= b.x || b.x + b.width <= a.x || a.y + a.height <= b.y || b.y + b.height <= a.y).toBe(true);
    }
    await expect(page.locator(".react-flow__minimap")).toBeVisible();
    await page.screenshot({ path: test.info().outputPath("complex-workflow-64-stage-overview.png") });
    await chooseSelect(
      page.getByRole("combobox", { name: "Find stage", exact: true }),
      "stage_63",
    );
    await expect(inspector.getByRole("textbox", { name: "Stage name", exact: true })).toHaveValue("stage_63");
    await expect.poll(async () => (await page.locator('.react-flow__node[data-id="stage_63"]').boundingBox())?.width ?? 0).toBeGreaterThan(150);
    await expect(page.locator('.react-flow__node[data-id="stage_63"]')).toBeInViewport({ ratio: 1 });
    await page.screenshot({ path: test.info().outputPath("complex-workflow-focused-stage.png") });
    await page.getByRole("button", { name: "Run All", exact: true }).click();
    await expect(page.getByText("Completed", { exact: true })).toBeVisible({ timeout: 45_000 });
    expect(readFileSync(path.join(repo, "large_63.txt"), "utf8")).toBe("63");

    await select("failure.workflow.yaml");
    await page.getByRole("button", { name: "Run All", exact: true }).click();
    await expect(page.getByText("Failed", { exact: true })).toBeVisible();
    await expect(output).toContainText("intentional-failure");
    expect(existsSync(path.join(repo, "must-not-exist.txt"))).toBe(false);
    await page.getByRole("button", { name: "Edit YAML", exact: true }).click();
    await expect(primary.getByRole("button", { name: "Files", exact: true })).toHaveAttribute("aria-pressed", "true");
    await expect(page.locator(".monaco-editor .view-lines")).toContainText("intentional-failure");
    const solo = path.join(root, "solo");
    mkdirSync(solo);
    mkdirSync(path.join(solo, "pipelines"));
    mkdirSync(path.join(solo, "work"));
    execFileSync("git", ["init", "--initial-branch=main"], { cwd: solo });
    writeFileSync(path.join(solo, "pipelines/only.workflow.yaml"), JSON.stringify({ stages: { report: { wdir: "work", cmd: cmd("require('fs').writeFileSync('single.txt','single-file')"), outs: ["single.txt"] } } }));
    const soloHandle = await call(page, "open_local", { repo_id: "solo", path: solo });
    await page.evaluate(({ handle, repo }) => window.dispatchEvent(new CustomEvent("crab:repo-opened", { detail: { handle, path: repo } })), { handle: soloHandle, repo: solo });
    await openWorkflows();
    await expect(page.locator('.react-flow__node[data-id="report"]')).toBeVisible();
    await page.getByRole("button", { name: "Run All", exact: true }).click();
    await expect(page.getByText("Completed", { exact: true })).toBeVisible();
    expect(readFileSync(path.join(solo, "work/single.txt"), "utf8")).toBe("single-file");
    expect(errors).toEqual([]);
  } finally {
    await application.close();
    await closeFixtureDaemon(home);
    rmSync(root, { recursive: true, force: true });
  }
});

test("workflow cancellation stops the stage process and releases the execution lock", async () => {
  test.setTimeout(60_000);
  const root = mkdtempSync(path.join(process.env.CARGO_TARGET_DIR ?? tmpdir(), "desktop-workflow-cancel-"));
  const repo = path.join(root, "repo"), home = path.join(root, "home");
  mkdirSync(repo);
  mkdirSync(home);
  execFileSync("git", ["init", "--initial-branch=main"], { cwd: repo });
  const definition = (script: string) => JSON.stringify({ stages: { wait: { cmd: { argv: [process.execPath, "-e", script] }, outs: ["result.txt"] } } });
  writeFileSync(path.join(repo, "crab.yaml"), definition("require('fs').writeFileSync('stage.pid',String(process.pid));console.log('waiting-for-cancel');setInterval(()=>{},1000)"));
  const application = await electron.launch({ executablePath: packagedExecutable(), args: [`--user-data-dir=${path.join(root, "profile")}`],
    env: { ...process.env, HOME: home, USERPROFILE: home, CRAB_CACHE_DIR: path.join(root, "cache") } });
  let stagePid: number | undefined;
  try {
    const page = await application.firstWindow();
    await page.getByRole("button", { name: "Skip", exact: true }).click();
    await page.getByRole("button", { name: "No thanks", exact: true }).click();
    await expect.poll(async () => { try { return (await call(page, "ping", {})).pong; } catch { return undefined; } }).toBeTruthy();
    const handle = await call(page, "open_local", { repo_id: "cancel", path: repo });
    await page.evaluate(({ handle, repo }) => window.dispatchEvent(new CustomEvent("crab:repo-opened", { detail: { handle, path: repo } })), { handle, repo });
    await page.getByRole("navigation", { name: "Primary", exact: true }).getByRole("button", { name: "Data Workflows", exact: true }).click();
    const early = await page.evaluate(async () => {
      const run_id = crypto.randomUUID();
      const running = window.crab.rpc("workflow_run", { repo_id: "cancel", path: "crab.yaml", run_id }).then(() => ({ completed: true }), (error: { code: number }) => ({ code: error.code }));
      const acknowledgement = await window.crab.rpc("workflow_cancel", { repo_id: "cancel", run_id });
      return { acknowledgement, result: await running };
    });
    expect(early).toMatchObject({ acknowledgement: { cancelled: true }, result: { code: 1010 } });
    await page.getByRole("button", { name: "Run All", exact: true }).click();
    const output = page.getByRole("region", { name: "Workflow output", exact: true });
    await expect(output).toContainText("waiting-for-cancel");
    stagePid = Number(readFileSync(path.join(repo, "stage.pid"), "utf8"));
    await page.getByRole("button", { name: "Cancel", exact: true }).click();
    await expect(page.getByText("Cancelled", { exact: true })).toBeVisible({ timeout: 12_000 });
    await expect.poll(() => { try { process.kill(stagePid!, 0); return false; } catch (error) { if ((error as NodeJS.ErrnoException).code !== "ESRCH") throw error; return true; } }, { timeout: 12_000 }).toBe(true);
    stagePid = undefined;
    writeFileSync(path.join(repo, "crab.yaml"), definition("require('fs').writeFileSync('result.txt','recovered')"));
    await page.getByRole("button", { name: "Refresh workflows", exact: true }).click();
    await page.getByRole("button", { name: "Run All", exact: true }).click();
    await expect(page.getByText("Completed", { exact: true })).toBeVisible();
    expect(readFileSync(path.join(repo, "result.txt"), "utf8")).toBe("recovered");
  } finally {
    if (stagePid) { try { process.kill(stagePid, "SIGKILL"); } catch (error) { if ((error as NodeJS.ErrnoException).code !== "ESRCH") throw error; } }
    await application.close();
    await closeFixtureDaemon(home);
    rmSync(root, { recursive: true, force: true });
  }
});

test("workflow studio resizes, retains a dismissible output dock, and runs locally with an unavailable S3 remote", async () => {
  test.setTimeout(90_000);
  const root = mkdtempSync(path.join(process.env.CARGO_TARGET_DIR ?? tmpdir(), "desktop-workflow-studio-"));
  const repo = path.join(root, "repo"), home = path.join(root, "home");
  mkdirSync(repo);
  mkdirSync(home);
  mkdirSync(path.join(repo, ".crab"));
  execFileSync("git", ["init", "--initial-branch=main"], { cwd: repo });
  writeFileSync(path.join(repo, ".crab/local.toml"), '[remote]\nurl = "crab://workflow-fixture/local-run"\n');
  writeFileSync(path.join(repo, "crab.yaml"), "stages:\n  example:\n    cmd: echo hello\n");
  writeFileSync(path.join(repo, "other.workflow.yaml"), JSON.stringify({ stages: { other: {
    cmd: { argv: [process.execPath, "-e", "require('fs').writeFileSync('result.txt','local');console.log('local-result')"] }, outs: ["result.txt"],
  } } }));
  writeFileSync(path.join(repo, "wait.workflow.yaml"), JSON.stringify({ stages: { wait: {
    cmd: { argv: [process.execPath, "-e", "const fs=require('fs');console.log('waiting-for-release');setInterval(()=>{if(fs.existsSync('release')){console.error('intentional-studio-failure');process.exit(17)}},20)"] },
  } } }));
  const requests: string[] = [];
  const server = createServer((request, response) => {
    requests.push(request.url ?? "");
    response.writeHead(403).end("storage intentionally unavailable");
  });
  await new Promise<void>(resolve => server.listen(0, "127.0.0.1", resolve));
  const address = server.address();
  if (!address || typeof address === "string") throw new Error("Missing S3 fixture listener");
  let application: Awaited<ReturnType<typeof electron.launch>> | undefined;
  try {
    application = await electron.launch({
      executablePath: packagedExecutable(),
      args: [`--user-data-dir=${path.join(root, "profile")}`],
      env: {
        ...process.env,
        HOME: home,
        USERPROFILE: home,
        CRAB_CACHE_DIR: path.join(root, "cache"),
        AWS_ACCESS_KEY_ID: "fixture",
        AWS_SECRET_ACCESS_KEY: "fixture",
        AWS_REGION: "us-east-1",
        AWS_EC2_METADATA_DISABLED: "true",
        AWS_ENDPOINT_URL_S3: `http://127.0.0.1:${address.port}`,
        AWS_ALLOW_HTTP: "true",
        AWS_VIRTUAL_HOSTED_STYLE_REQUEST: "false",
      },
    });
    const page = await application.firstWindow();
    await page.getByRole("button", { name: "Skip", exact: true }).click();
    await page.getByRole("button", { name: "No thanks", exact: true }).click();
    await expect.poll(async () => { try { return (await call(page, "ping", {})).pong; } catch { return undefined; } }).toBeTruthy();
    const handle = await call(page, "open_local", { repo_id: "studio", path: repo });
    await page.evaluate(({ handle, repo }) => window.dispatchEvent(new CustomEvent("crab:repo-opened", { detail: { handle, path: repo } })), { handle, repo });
    await page.getByRole("navigation", { name: "Primary", exact: true }).getByRole("button", { name: "Data Workflows", exact: true }).click();
    const sidebar = page.getByRole("complementary", { name: "Workflows", exact: true });
    const listHandle = page.getByRole("separator", { name: "Resize workflow list", exact: true });
    const beforeWidth = (await sidebar.boundingBox())!.width;
    const divider = (await listHandle.boundingBox())!;
    await page.mouse.move(divider.x + divider.width / 2, divider.y + divider.height / 2);
    await page.mouse.down();
    await page.mouse.move(divider.x + 60, divider.y + divider.height / 2, { steps: 8 });
    await page.mouse.up();
    await expect.poll(async () => (await sidebar.boundingBox())!.width).toBeGreaterThan(beforeWidth + 30);
    const output = page.getByRole("region", { name: "Workflow output", exact: true });
    const toggle = page.getByRole("button", { name: "Toggle workflow output", exact: true });
    await expect(output).toBeHidden();
    await toggle.click();
    await expect(output).toContainText("Run a workflow to see output here.");
    const outputHandle = page.getByRole("separator", { name: "Resize workflow output", exact: true });
    const beforeHeight = (await output.boundingBox())!.height;
    await outputHandle.focus();
    await outputHandle.press("ArrowUp");
    await expect.poll(async () => (await output.boundingBox())!.height).toBeGreaterThan(beforeHeight);
    const expandedHeight = (await output.boundingBox())!.height;
    await output.getByRole("button", { name: "Hide workflow output", exact: true }).click();
    await expect(output).toBeHidden();
    await toggle.click();
    expect(Math.abs((await output.boundingBox())!.height - expandedHeight)).toBeLessThan(3);
    await page.getByRole("button", { name: "Status", exact: true }).click();
    await expect(output).toBeVisible();
    await expect(page.getByRole("row").filter({ hasText: "example" })).toBeVisible();
    await expect(page.getByRole("row").filter({ hasText: "other.other" })).toBeHidden();
    await page.getByRole("button", { name: "Editor", exact: true }).click();
    await output.getByRole("button", { name: "Hide workflow output", exact: true }).click();
    await page.getByRole("button", { name: "Run All", exact: true }).click();
    await expect(output).toContainText("hello");
    await expect(output).toContainText("example: Completed");
    await expect(toggle).toHaveAttribute("aria-expanded", "true");
    await output.getByRole("button", { name: "Hide workflow output", exact: true }).click();
    await toggle.click();
    await expect(output).toContainText("hello");
    await sidebar.getByRole("button").filter({ hasText: "other.workflow.yaml" }).click();
    await page.getByRole("button", { name: "Status", exact: true }).click();
    await expect(page.getByRole("row").filter({ hasText: "other" })).toBeVisible();
    await page.getByRole("button", { name: "Run All", exact: true }).last().click();
    await expect(output).toContainText("local-result");
    expect(readFileSync(path.join(repo, "result.txt"), "utf8")).toBe("local");
    await sidebar.getByRole("button").filter({ hasText: "wait.workflow.yaml" }).click();
    await page.getByRole("button", { name: "Editor", exact: true }).click();
    await page.getByRole("button", { name: "Run All", exact: true }).click();
    await expect(output).toContainText("waiting-for-release");
    await output.getByRole("button", { name: "Hide workflow output", exact: true }).click();
    writeFileSync(path.join(repo, "release"), "continue");
    await expect(page.getByText("Failed", { exact: true })).toBeVisible();
    await expect(output).toBeHidden();
    await toggle.click();
    await expect(output).toContainText("intentional-studio-failure");
    expect(requests).toEqual([]);
  } finally {
    if (application) { await application.close(); await closeFixtureDaemon(home); }
    await new Promise<void>((resolve, reject) =>
      server.close((error) => (error ? reject(error) : resolve())),
    );
    rmSync(root, { recursive: true, force: true });
  }
});
