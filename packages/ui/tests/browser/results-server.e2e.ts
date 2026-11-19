import { expect, test } from "@playwright/test";
import { readFileSync } from "node:fs";
import path from "node:path";
import type {
  Report,
  ResultsRead,
  ResultsResponse,
} from "../../src/lib/results-contracts";
import type {
  AccessUpdate,
  MembershipState,
  Session,
} from "../../src/lib/contracts";
import type { Repository } from "../../src/lib/api";
import { checkServerLifecycle } from "./results-lifecycle";

test.use({ channel: process.env.CI ? undefined : "chrome" });
test("real browser publishes captured runs, reloads reports and loses revoked access", async ({
  page,
}, info) => {
  test.skip(
    info.project.name !== "results-http",
    "Requires the real server qualification harness",
  );
  const fixture: Report = JSON.parse(
    readFileSync(
      path.join(process.env.CRAB_RESULTS_EVIDENCE!, "report.json"),
      "utf8",
    ),
  );
  const errors: string[] = [];
  page.on("pageerror", (error) => errors.push(error.message));
  await page.goto("/team/private");
  await page.getByRole("button", { name: "Continue to sign in" }).click();
  await page
    .getByRole("navigation", { name: "Repository", exact: true })
    .getByRole("link", { name: "Results", exact: true })
    .click();
  await expect(
    page.getByRole("region", { name: "Results workspace" }),
  ).toBeVisible();
  const selections = page.getByRole("checkbox", { name: /^Compare / });
  await expect(selections).toHaveCount(2);
  await selections.nth(0).check();
  await selections.nth(1).check();
  await expect(page.locator("g.mark-line path")).toHaveCount(4);
  const comparison = page.getByRole("region", { name: "Metric comparison" });
  await expect(comparison.getByText("0.1", { exact: true })).toBeVisible();
  await expect(comparison.getByText("0.2", { exact: true })).toBeVisible();
  await expect(page.getByRole("img", { name: /prediction.svg/ })).toHaveCount(
    2,
  );
  await expect(
    page.getByRole("button", { name: "Save report", exact: true }),
  ).toBeEnabled();
  page.once("dialog", (dialog) => dialog.accept("Browser round trip"));
  const publication = page.waitForResponse(
    (response) =>
      new URL(response.url()).pathname.endsWith("/results/reports") &&
      response.request().method() === "POST",
    { timeout: 15_000 },
  );
  await page.getByRole("button", { name: "Save report", exact: true }).click();
  expect(
    (await publication).status(),
    "Report publication excludes model payloads",
  ).toBe(200);
  await expect(
    page.getByRole("heading", { name: "Browser round trip", exact: true }),
  ).toBeVisible();
  const report = await page.getByLabel("Open immutable report").inputValue();
  expect(report).toMatch(/^[a-f0-9]{64}$/);
  await page.reload();
  await expect(
    page.getByRole("heading", { name: "Browser round trip", exact: true }),
  ).toBeVisible();
  await expect(page.locator("g.mark-line path")).toHaveCount(4);
  await page.goto(`/team/private/results?report=${report}`);
  await expect(
    page.getByRole("heading", { name: "Browser round trip", exact: true }),
  ).toBeVisible();
  await page
    .getByRole("button", { name: "output · model.txt · present", exact: true })
    .click();
  await expect(
    page.getByText(
      "This report retains an immutable link. Retrieve the run or artifact version for payload bytes.",
      { exact: true },
    ),
  ).toBeVisible();

  const address = (repository: string, request: ResultsRead) =>
    `/api/repos/team/${repository}/results?${new URLSearchParams({ request: JSON.stringify(request) })}`;
  const captured: ResultsResponse = await (
    await page.request.get(
      address("private", {
        kind: "report",
        args: { id: report },
      }),
    )
  ).json();
  if (captured.kind !== "report")
    throw new Error("Expected actual published report");
  const selected = captured.data.report.comparison.rows[0].run;
  const curve = selected.snapshot.attachments.find(
    (attachment) =>
      attachment.path === "curve.csv" && attachment.kind === "plot",
  );
  if (!curve?.hash) throw new Error("Actual report has no retained curve");
  const control: { origin: string } = JSON.parse(
    readFileSync(
      path.join(
        process.env.CRAB_RESULTS_EVIDENCE!,
        "browser-storage-control.json",
      ),
      "utf8",
    ),
  );
  expect(control.origin).toMatch(/^http:\/\/127\.0\.0\.1:\d+$/);
  const command = async (operation: string) => {
    expect(
      (await fetch(`${control.origin}/${operation}`, { method: "POST" }))
        .status,
    ).toBe(204);
  };
  const state = async (): Promise<{
    held: number;
    observed: { method: string; status: number } | null;
    readers: { active: number };
  }> => (await fetch(control.origin)).json();
  const viewport = await page.evaluate(() => ({
    width: innerWidth,
    height: innerHeight,
  }));
  const evidenceResponse = () =>
    page.waitForResponse(
      (response) => {
        if (!new URL(response.url()).pathname.endsWith("/results"))
          return false;
        const query = new URL(response.url()).searchParams.get("request");
        if (!query) return false;
        const request = JSON.parse(query) as ResultsRead;
        return (
          request.kind === "evidence" &&
          request.args.path === curve.path &&
          request.args.report === report
        );
      },
      { timeout: 120_000 },
    );
  try {
    await command(`hold/${curve.hash}`);
    const preview = page.getByRole("button", {
      name: "plot · curve.csv · present",
      exact: true,
    });
    const result = evidenceResponse();
    await preview.click();
    await expect.poll(async () => (await state()).held).toBe(1);
    expect((await state()).observed).toEqual({ method: "GET", status: 200 });
    expect((await state()).readers.active).toBeGreaterThan(0);
    await expect(page.getByRole("alert")).toHaveCount(0);
    await command("disconnect");
    const failed = await result;
    const failure: { error: { message: string } } = await failed.json();
    const messages = new Map([
      [
        503,
        /^(Repository permissions are temporarily unavailable\.|Results evidence could not be verified or retrieved; retry or check repository storage)$/,
      ],
      [504, /^The read request timed out\. Retry the read\.$/],
    ]);
    const message = messages.get(failed.status());
    expect(
      message,
      "Only owned unavailability or read-timeout responses are valid",
    ).toBeDefined();
    expect(failure.error.message).toMatch(message!);
    const error = page.getByRole("alert");
    await expect(error).toContainText(failure.error.message);
    await expect(page.getByLabel("Open immutable report")).toHaveValue(report);
    await expect(comparison).toContainText("0.1");
    await expect(comparison).toContainText("0.2");
    await expect(page.locator("g.mark-line path")).toHaveCount(4);
    for (const [width, height, suffix] of [
      [1280, 1000, "wide"],
      [390, 844, "narrow"],
    ] as const) {
      await page.setViewportSize({ width, height });
      await error.scrollIntoViewIfNeeded();
      await expect(
        error.getByRole("button", { name: "Refresh view", exact: true }),
      ).toBeInViewport({ ratio: 1 });
      await page.screenshot({
        path: path.join(
          process.env.CRAB_RESULTS_EVIDENCE!,
          `browser-storage-disconnected-${suffix}.png`,
        ),
        fullPage: true,
      });
    }
    await command("resume");
    await preview.click();
    await expect(error).toHaveCount(0);
    await expect(page.locator("pre")).toContainText("step,loss");
    await expect(page.locator("g.mark-line path")).toHaveCount(4);
    await page.screenshot({
      path: path.join(
        process.env.CRAB_RESULTS_EVIDENCE!,
        "browser-storage-recovered.png",
      ),
      fullPage: true,
    });

    await command(`hold/${curve.hash}`);
    const timeout = evidenceResponse();
    await preview.click();
    await expect.poll(async () => (await state()).held).toBe(1);
    expect((await state()).observed).toEqual({ method: "GET", status: 200 });
    expect((await state()).readers.active).toBeGreaterThan(0);
    const timedOut = await timeout;
    expect(timedOut.status()).toBe(504);
    expect(await timedOut.json()).toEqual({
      error: { message: "The read request timed out. Retry the read." },
    });
    await expect(error).toContainText(
      "The read request timed out. Retry the read.",
    );
    await expect(comparison).toContainText("0.1");
    await expect(comparison).toContainText("0.2");
    await expect(page.locator("g.mark-line path")).toHaveCount(4);
    for (const [width, height, suffix] of [
      [1280, 1000, "wide"],
      [390, 844, "narrow"],
    ] as const) {
      await page.setViewportSize({ width, height });
      await error.scrollIntoViewIfNeeded();
      await expect(
        error.getByRole("button", { name: "Refresh view", exact: true }),
      ).toBeInViewport({ ratio: 1 });
      await page.screenshot({
        path: path.join(
          process.env.CRAB_RESULTS_EVIDENCE!,
          `browser-read-timeout-${suffix}.png`,
        ),
        fullPage: true,
      });
    }
    await command("resume");
    await preview.click();
    await expect(error).toHaveCount(0);
    await expect(page.locator("pre")).toContainText("step,loss");
    await expect(page.locator("g.mark-line path")).toHaveCount(4);
    await page.screenshot({
      path: path.join(
        process.env.CRAB_RESULTS_EVIDENCE!,
        "browser-read-timeout-recovered.png",
      ),
      fullPage: true,
    });
  } finally {
    await command("resume");
    await page.setViewportSize(viewport);
  }
  await checkServerLifecycle(
    page,
    report,
    control.origin,
    process.env.CRAB_RESULTS_EVIDENCE!,
  );

  const provenance: { id: string; report: Report } = JSON.parse(
    readFileSync(
      path.join(process.env.CRAB_RESULTS_EVIDENCE!, "reproduction-report.json"),
      "utf8",
    ),
  );
  const child = provenance.report.comparison.rows.find(
    (row) => row.run.snapshot.reproduction?.source_result,
  )!.run;
  const source = child.snapshot.reproduction!.source_result!;
  await page.goto(`/team/private/results?report=${provenance.id}`);
  await page
    .getByRole("region", { name: "Metric comparison", exact: true })
    .getByRole("button", {
      name: child.snapshot.experiment_id.slice(-8),
      exact: true,
    })
    .click();
  await page.getByText("Captured run identities", { exact: true }).click();
  const origin = page.getByText(/^Reproduced from /);
  await expect(origin).toContainText(source.id);
  await expect(origin).toContainText(source.digest);
  await origin.scrollIntoViewIfNeeded();
  await page.screenshot({
    path: path.join(
      process.env.CRAB_RESULTS_EVIDENCE!,
      "browser-http-provenance-wide.png",
    ),
    fullPage: true,
  });
  await page.setViewportSize({ width: 390, height: 844 });
  await origin.scrollIntoViewIfNeeded();
  await expect(origin).toBeVisible();
  expect(
    await origin.evaluate(
      (element) => element.getBoundingClientRect().right <= window.innerWidth,
    ),
  ).toBe(true);
  await page.screenshot({
    path: path.join(
      process.env.CRAB_RESULTS_EVIDENCE!,
      "browser-http-provenance-narrow.png",
    ),
    fullPage: true,
  });

  const imported: { id: string; report: Report } = JSON.parse(
    readFileSync(
      path.join(process.env.CRAB_RESULTS_EVIDENCE!, "metadata-report.json"),
      "utf8",
    ),
  );
  await page.goto(`/team/private/results?report=${imported.id}`);
  const importedComparison = page.getByRole("region", {
    name: "Metric comparison",
    exact: true,
  });
  await expect(importedComparison).toContainText("0.4");
  await expect(importedComparison).toContainText("0.6");
  await expect(importedComparison).toContainText(
    "Imported experiment metadata",
  );
  await expect(page.locator("g.mark-line path")).toHaveCount(0);
  const importedActionNotice = page.getByText(
    /^Imported experiment metadata has no verified execution/,
  );
  await expect(importedActionNotice).toBeVisible();
  await expect(importedActionNotice).toContainText(
    "Run or save a new experiment",
  );
  await expect(
    page.getByRole("button", { name: "Apply", exact: true }),
  ).toHaveAttribute(
    "title",
    /^Imported experiment metadata has no verified execution/,
  );
  const importedCapture = async (
    name: string,
    width: number,
    height: number,
    subject = importedComparison,
  ) => {
    await page.setViewportSize({ width, height });
    await subject.scrollIntoViewIfNeeded();
    await page.screenshot({
      path: path.join(process.env.CRAB_RESULTS_EVIDENCE!, name),
      fullPage: true,
    });
  };
  await importedCapture("browser-imported-report.png", 1280, 1000);
  await importedCapture("browser-imported-report-narrow.png", 390, 844);
  await page.goto("/team/other/results");
  const importedChoice = page.getByRole("checkbox", {
    name: /^Compare Before Results success /,
  });
  await expect(importedChoice).toHaveCount(1);
  const importedRow = page.getByRole("row").filter({ has: importedChoice });
  await expect(importedRow).toContainText("success (imported)");
  await importedChoice.check();
  await expect(importedComparison).toContainText(
    "Imported experiment metadata",
  );
  await expect(importedActionNotice).toBeVisible();
  await expect(
    page.getByRole("button", { name: "Apply", exact: true }),
  ).toHaveAttribute(
    "title",
    /^Imported experiment metadata has no verified execution/,
  );
  await importedCapture(
    "browser-imported-catalog.png",
    1280,
    1000,
    importedRow,
  );
  await importedCapture(
    "browser-imported-catalog-narrow.png",
    390,
    844,
    importedRow,
  );
  await page.goto(`/team/private/results?report=${report}`);

  const requests: ResultsRead[] = [
    { kind: "report", args: { id: report } },
    {
      kind: "evidence",
      args: {
        run: {
          id: fixture.comparison.rows[0].run.snapshot.experiment_id,
          digest: fixture.comparison.rows[0].run.digest,
        },
        path: "metrics.json",
        report,
      },
    },
  ];
  // Alice is an administrator of both repositories. An immutable ID never
  // grants access through another repository's namespace.
  const catalog: { repositories: Repository[] } = await (
    await page.request.get("/api/repos")
  ).json();
  expect(catalog.repositories.map((repo) => repo.name).sort()).toEqual([
    "other",
    "private",
  ]);
  for (const request of requests) {
    expect((await page.request.get(address("private", request))).status()).toBe(
      200,
    );
    expect((await page.request.get(address("other", request))).status()).toBe(
      404,
    );
  }
  const session: Session = await (
    await page.request.get("/api/session")
  ).json();
  const members = "/api/repos/team/private/members";
  const current: MembershipState = await (
    await page.request.get(members)
  ).json();
  const update: AccessUpdate = {
    expected_revision: current.revision,
    members: current.members
      .filter((member) => member.subject !== "alice-id")
      .map((member) => ({ ...member, access: "admin" })),
    groups: current.groups,
  };
  const repository = catalog.repositories.find(
    (repo) => repo.name === "private",
  )!;
  const revoked = await page.request.put(members, {
    headers: {
      Origin: new URL(page.url()).origin,
      "X-CSRF-Token": session.csrf!,
      "X-Crab-Repository-Id": repository.id,
    },
    data: update,
  });
  expect(revoked.status()).toBe(200);
  for (const request of requests)
    expect((await page.request.get(address("private", request))).status()).toBe(
      404,
    );
  const retainedSession: Session = await (
    await page.request.get("/api/session")
  ).json();
  expect(retainedSession.authenticated).toBe(true);
  expect(retainedSession.user).toEqual(session.user);
  await page.reload();
  await expect(
    page.getByRole("heading", { name: "Repository not found", exact: true }),
  ).toBeVisible();
  await expect(
    page.getByRole("region", { name: "Results workspace" }),
  ).toHaveCount(0);
  expect(errors).toEqual([]);
});
