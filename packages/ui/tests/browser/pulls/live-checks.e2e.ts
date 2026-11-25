import { expect, test } from "@playwright/test";
import { mockPullRequest } from "./fixture";

test("live head checks retain the selected log and pause in a hidden tab", async ({
  page,
}) => {
  const fixture = await mockPullRequest(page);
  const root = "/api/repos/team/project";
  const head = fixture.pull().head_oid;
  let version = 1;
  let reads = 0;
  let detailReads = 0;
  let access = true;
  let failure = 404;
  const run = () => ({
    id: 7,
    head_sha: head,
    name: "ci/live",
    status: version === 2 ? "completed" : "in_progress",
    conclusion: version === 2 ? "success" : null,
    provider: "crab",
    author: "CI reporter",
    version,
  });
  await page.route(
    (url) => url.pathname === `${root}/pulls/1`,
    (route) =>
      route.fulfill(
        access
          ? {
              json: {
                ...fixture.pull(),
                merge_requirements: {
                  ...fixture.pull().merge_requirements,
                  checks: [
                    {
                      context: "ci/live",
                      head_sha: head,
                      provider: "crab_check_run",
                      run_id: 7,
                      state: version === 2 ? "success" : "pending",
                    },
                  ],
                },
              },
            }
          : {
              status: failure,
              json: {
                error: {
                  message:
                    failure === 404
                      ? "Pull request unavailable"
                      : "Temporary failure",
                },
              },
            },
      ),
  );
  await page.route(
    (url) => url.pathname === `${root}/commits/${head}/check-runs`,
    (route) => {
      reads += 1;
      expect(route.request().headers()["x-crab-repository-id"]).toBe(
        fixture.lifecycle.repositoryId,
      );
      return route.fulfill({ json: { sha: head, items: [run()], next: null } });
    },
  );
  await page.route(
    (url) => url.pathname === `${root}/commits/${head}/check-runs/7`,
    (route) => {
      detailReads += 1;
      return route.fulfill({
        json: {
          ...run(),
          output: {
            title: "CI output",
            summary: "Build and test",
            annotations: [],
            steps: [
              {
                name: "Test project",
                status: run().status,
                conclusion: run().conclusion,
                log: ["Starting job", "Finished job", "Rerunning job"][
                  version - 1
                ],
              },
            ],
          },
        },
      });
    },
  );
  await page.clock.install();
  await page.goto("/team/project/pull/1/checks?check=7");
  await expect(page.getByText("Required checks are waiting")).toBeVisible();
  const step = page.getByRole("button", { name: /Test project/ });
  await step.click();
  await expect(page.getByText("Starting job")).toBeVisible();
  await page.clock.fastForward(5_000);
  await expect.poll(() => reads).toBeGreaterThan(1);
  expect(detailReads).toBe(1);

  async function visibility(hidden: boolean) {
    await page.evaluate((hidden) => {
      Object.defineProperty(document, "hidden", {
        configurable: true,
        value: hidden,
      });
      document.dispatchEvent(new Event("visibilitychange"));
    }, hidden);
  }
  await visibility(true);
  const hiddenReads = reads;
  version = 2;
  await page.clock.fastForward(20_000);
  expect(reads).toBe(hiddenReads);
  await expect(page.getByText("Starting job")).toBeVisible();

  await visibility(false);
  await expect(page.getByText("All required checks have passed")).toBeVisible();
  await expect(page.getByText("Finished job")).toBeVisible();
  await expect(step).toHaveAttribute("aria-expanded", "true");
  await expect(step).toBeFocused();
  expect(detailReads).toBe(2);

  version = 3;
  await page.clock.fastForward(15_000);
  await expect(page.getByText("Required checks are waiting")).toBeVisible();
  await expect(page.getByText("Rerunning job")).toBeVisible();
  await expect(step).toHaveAttribute("aria-expanded", "true");
  expect(detailReads).toBe(3);

  access = false;
  await page.clock.fastForward(10_000);
  await expect(page.getByRole("alert")).toContainText(
    "Pull request unavailable",
  );
  await expect(page.getByText("Rerunning job")).toHaveCount(0);
  failure = 503;
  await page.getByRole("button", { name: "Try again" }).click();
  await expect(page.getByRole("alert")).toContainText("Temporary failure");
  await expect(page.getByText("Rerunning job")).toHaveCount(0);
});

test("a queued Checks tab follows the current candidate as it is rebuilt", async ({
  page,
}) => {
  const fixture = await mockPullRequest(page);
  const pull = fixture.pull();
  const root = "/api/repos/team/project";
  const first = "d".repeat(40);
  const second = "e".repeat(40);
  let commit: string | undefined;
  const inspected: string[] = [];
  await page.route(
    (url) => url.pathname === `${root}/pulls/1`,
    (route) =>
      route.fulfill({
        json: { ...pull, merge_queued: true, merge_queue_required: true },
      }),
  );
  await page.route(
    (url) => url.pathname === `${root}/pulls/1/merge-candidate`,
    (route) =>
      route.fulfill({
        json: {
          preview: null,
          candidate: commit
            ? {
                commit,
                base_oid: pull.base_oid,
                head_oid: pull.head_oid,
                ref_name: "refs/crab/merge-queue/1",
                current: true,
                mirror_ref: null,
                can_remove: false,
                required_checks: [
                  {
                    context: "ci/candidate",
                    provider: "github_actions",
                    head_sha: commit,
                    state: commit === first ? "pending" : "success",
                  },
                ],
              }
            : null,
        },
      }),
  );
  await page.route(
    (url) => url.pathname.endsWith("/check-runs"),
    (route) => {
      const oid = new URL(route.request().url()).pathname.split("/").at(-2)!;
      inspected.push(oid);
      return route.fulfill({ json: { sha: oid, items: [], next: null } });
    },
  );
  await page.clock.install();
  await page.goto("/team/project/pull/1/checks?candidate=1");
  await expect(page.getByText(/reaches a build slot/)).toBeVisible();
  expect(inspected).toEqual([]);

  commit = first;
  await page.clock.fastForward(5_000);
  const summary = page.locator(".required-checks");
  await expect(summary.getByTitle(first)).toBeVisible();
  await expect(summary).toContainText("Required checks are waiting");
  await expect.poll(() => inspected).toContain(first);

  commit = second;
  await page.clock.fastForward(5_000);
  await expect(summary.getByTitle(second)).toBeVisible();
  await expect(summary).toContainText("All required checks have passed");
  await expect(summary.getByTitle(first)).toHaveCount(0);
  await expect.poll(() => inspected).toContain(second);
  expect(inspected).not.toContain(pull.head_oid);
});
