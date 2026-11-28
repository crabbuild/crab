import { chooseSelect } from "../helpers/select";
import { expect, type ElectronApplication, type Page } from "@playwright/test";
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import {
  readlinkSync,
  readdirSync,
  realpathSync,
  writeFileSync,
} from "node:fs";
import path from "node:path";
import { isNativeAgentCommand } from "./packaged-app";
import type { resultsStorageProxy } from "./results-storage-proxy";

const execute = promisify(execFile);

async function windowsProcessQuery(script: string): Promise<unknown> {
  const command =
    "$ErrorActionPreference = 'Stop'; [Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false); " +
    script;
  const { stdout } = await execute(
    "powershell.exe",
    [
      "-NoProfile",
      "-NonInteractive",
      "-EncodedCommand",
      Buffer.from(command, "utf16le").toString("base64"),
    ],
    { encoding: "utf8", timeout: 5000, windowsHide: true },
  );
  return JSON.parse(stdout);
}

async function handles(pid: number): Promise<number> {
  if (!Number.isSafeInteger(pid) || pid <= 0)
    throw new Error(
      "Results resource qualification requires a process identity",
    );
  if (process.platform === "win32") {
    // Kernel handles include more than files. Compare each process only with
    // its own warmed baseline, using a fresh OS measurement at every sample.
    const count = await windowsProcessQuery(
      `$process = Get-Process -Id ${pid}; $process.Refresh(); ConvertTo-Json -InputObject $process.HandleCount -Compress`,
    );
    if (typeof count !== "number" || !Number.isSafeInteger(count) || count <= 0)
      throw new Error("Windows did not return a positive process handle count");
    return count;
  }
  if (process.platform === "linux")
    return readdirSync(`/proc/${pid}/fd`).length;
  if (process.platform !== "darwin")
    throw new Error("Results descriptor qualification requires Linux or macOS");
  const { stdout } = await execute(
    "lsof",
    ["-a", "-p", String(pid), "-F", "f"],
    {
      encoding: "utf8",
      timeout: 5000,
    },
  );
  return new Set(stdout.split("\n").filter((field) => /^f\d+$/.test(field)))
    .size;
}

async function nativeProcess(parent: number): Promise<number> {
  if (!Number.isSafeInteger(parent) || parent <= 0)
    throw new Error("The packaged application has no process identity");
  if (process.platform === "win32") {
    const children = await windowsProcessQuery(
      `$children = @(Get-CimInstance Win32_Process -Filter 'ParentProcessId = ${parent}' | Select-Object ProcessId, ExecutablePath); ConvertTo-Json -InputObject $children -Compress`,
    );
    if (!Array.isArray(children))
      throw new Error(
        "Windows did not return the application's child processes",
      );
    const rows: unknown[] = children;
    for (const child of rows) {
      if (
        !child ||
        typeof child !== "object" ||
        !("ProcessId" in child) ||
        typeof child.ProcessId !== "number" ||
        !Number.isSafeInteger(child.ProcessId) ||
        child.ProcessId <= 0 ||
        !("ExecutablePath" in child)
      )
        throw new Error("Windows returned an invalid child process identity");
      // CIM can omit paths for processes the caller cannot inspect. The actor
      // must still match its canonical executable; a PID or name alone cannot.
      if (
        typeof child.ExecutablePath === "string" &&
        isNativeAgentCommand(realpathSync(child.ExecutablePath))
      )
        return child.ProcessId;
    }
    throw new Error("The packaged application has no matching native actor");
  }
  const { stdout } = await execute("ps", ["-axo", "pid=,ppid="], {
    encoding: "utf8",
  });
  for (const row of stdout.split("\n")) {
    const fields = /^\s*(\d+)\s+(\d+)\s*$/.exec(row);
    if (!fields || Number(fields[2]) !== parent) continue;
    const pid = Number(fields[1]);
    const executable =
      process.platform === "linux"
        ? readlinkSync(`/proc/${pid}/exe`)
        : (
            await execute("ps", ["-p", String(pid), "-o", "comm="], {
              encoding: "utf8",
            })
          ).stdout.trim();
    if (isNativeAgentCommand(executable)) return pid;
  }
  throw new Error("The packaged application has no matching native actor");
}

export async function checkResultsLifecycle({
  app,
  page,
  reportId,
  storage,
  output,
  checkReport,
}: {
  app: ElectronApplication;
  page: Page;
  reportId: string;
  storage: Awaited<ReturnType<typeof resultsStorageProxy>>;
  output: string;
  checkReport: (page: Page, id: string) => Promise<void>;
}) {
  const main = await app.evaluate(() => process.pid);
  if (!main)
    throw new Error("The packaged application has no process identity");
  const agent = await nativeProcess(main);
  const debuggerSession = await page.context().newCDPSession(page);
  const workspace = page.getByLabel("Results workspace", { exact: true });
  const samples: Array<{
    cycle: number;
    heap_bytes: number;
    fd_counts: { main: number; agent: number };
    readers: typeof storage.readers;
  }> = [];
  const cycle = async (reload: boolean) => {
    await page.getByRole("tab", { name: "Editor", exact: true }).click();
    await expect(workspace).toHaveCount(0);
    if (reload) await page.reload();
    await page.getByRole("tab", { name: "Results", exact: true }).click();
    await chooseSelect(page.getByLabel("Open immutable report"), reportId);
    await checkReport(page, reportId);
  };
  try {
    for (let index = 0; index < 5; index++) await cycle(index === 4);
    for (let index = 0; index <= 20; index++) {
      if (index) await cycle(index % 5 === 0);
      let sample: (typeof samples)[number] | undefined;
      // Visible charts can precede a late listing read. Measure an unchanged
      // idle interval, yielding to the fixture proxy during OS queries.
      await expect
        .poll(async () => {
          const before = storage.readers;
          if (before.active) return false;
          expect(await nativeProcess(main)).toBe(agent);
          await debuggerSession.send("HeapProfiler.collectGarbage");
          const heap = await debuggerSession.send("Runtime.getHeapUsage");
          const [mainCount, agentCount] = await Promise.all([
            handles(main),
            handles(agent),
          ]);
          const readers = storage.readers;
          if (readers.active || readers.started !== before.started)
            return false;
          sample = {
            cycle: index,
            heap_bytes: heap.usedSize,
            fd_counts: { main: mainCount, agent: agentCount },
            readers,
          };
          return true;
        })
        .toBe(true);
      if (!sample) throw new Error("No idle Results resource sample");
      samples.push(sample);
      expect(sample.readers.active).toBe(0);
      expect(sample.readers.started).toBe(sample.readers.finished);
      expect(sample.readers.started).toBeGreaterThan(
        index ? samples[index - 1].readers.started : 0,
      );
      expect(sample.heap_bytes - samples[0].heap_bytes).toBeLessThanOrEqual(
        16 * 1024 * 1024,
      );
      for (const owner of ["main", "agent"] as const) {
        expect(sample.fd_counts[owner]).toBeGreaterThan(0);
        expect(
          sample.fd_counts[owner] - samples[0].fd_counts[owner],
        ).toBeLessThanOrEqual(8);
      }
    }
  } finally {
    writeFileSync(
      path.join(output, "native-resource-envelope.json"),
      JSON.stringify(
        {
          schema: 1,
          report: reportId,
          cycles: 20,
          warmup_cycles: 5,
          platform: process.platform,
          architecture: process.arch,
          pids: { main, agent },
          reader_source: "completed_proxy_response_lifetimes",
          samples,
        },
        null,
        2,
      ),
    );
    await debuggerSession.detach();
  }
}
