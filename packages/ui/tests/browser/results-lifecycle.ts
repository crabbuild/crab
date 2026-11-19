import { expect, type Page } from "@playwright/test";
import { writeFileSync } from "node:fs";
import path from "node:path";

export async function checkServerLifecycle(
  page: Page,
  report: string,
  control: string,
  output: string,
) {
  const state = async (): Promise<{
    readers: {
      active: number;
      peak: number;
      started: number;
      finished: number;
    };
    fd_counts: { server: number };
  }> => {
    const response = await fetch(control);
    expect(response.status).toBe(200);
    return response.json();
  };
  const debuggerSession = await page.context().newCDPSession(page);
  const samples: Array<
    Awaited<ReturnType<typeof state>> & { cycle: number; heap_bytes: number }
  > = [];
  const cycle = async (reload: boolean) => {
    await page.goto("/team/other/results");
    await expect(
      page.getByRole("region", { name: "Results workspace", exact: true }),
    ).toBeVisible();
    await expect(page.locator("g.mark-line path")).toHaveCount(0);
    await page.goto(`/team/private/results?report=${report}`);
    if (reload) await page.reload();
    await expect(page.locator("g.mark-line path")).toHaveCount(4);
    const comparison = page.getByRole("region", {
      name: "Metric comparison",
      exact: true,
    });
    await expect(comparison).toContainText("0.1");
    await expect(comparison).toContainText("0.2");
  };
  try {
    for (let index = 0; index < 5; index++) await cycle(index === 4);
    for (let index = 0; index <= 20; index++) {
      if (index) await cycle(index % 5 === 0);
      let sample: (typeof samples)[number] | undefined;
      // Rendering and storage completion have separate lifetimes. Reject a
      // measurement if any new read starts while collecting heap and handles.
      await expect
        .poll(async () => {
          const before = await state();
          if (before.readers.active) return false;
          await debuggerSession.send("HeapProfiler.collectGarbage");
          const heap = await debuggerSession.send("Runtime.getHeapUsage");
          const current = await state();
          if (
            current.readers.active ||
            current.readers.started !== before.readers.started
          )
            return false;
          sample = {
            cycle: index,
            heap_bytes: heap.usedSize,
            readers: current.readers,
            fd_counts: current.fd_counts,
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
      expect(sample.fd_counts.server).toBeGreaterThan(0);
      expect(
        sample.fd_counts.server - samples[0].fd_counts.server,
      ).toBeLessThanOrEqual(8);
      expect(sample.heap_bytes - samples[0].heap_bytes).toBeLessThanOrEqual(
        16 * 1024 * 1024,
      );
    }
  } finally {
    writeFileSync(
      path.join(output, "http-resource-envelope.json"),
      JSON.stringify(
        {
          schema: 1,
          report,
          cycles: 20,
          warmup_cycles: 5,
          platform: process.platform,
          architecture: process.arch,
          reader_source: "storage_observer_response_streams",
          samples,
        },
        null,
        2,
      ),
    );
    await debuggerSession.detach();
  }
}
