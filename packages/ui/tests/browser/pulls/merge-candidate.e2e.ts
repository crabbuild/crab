import { expect, test } from "@playwright/test";
import { mockPullRequest } from "./fixture";

test("candidate checks use the prepared merge SHA and recover the original preparation", async ({
  page,
}) => {
  const fixture = await mockPullRequest(page);
  const pull = fixture.pull();
  const commit = "c".repeat(40);
  const root = "/api/repos/team/project";
  const input = {
    request_id: "019b21da-55c0-7000-8000-000000000125",
    version: pull.version,
    base_oid: pull.base_oid,
    head_oid: pull.head_oid,
    previous_oid: null,
  };
  const mirror = `refs/heads/crab/merge-queue/00000000-0000-4000-8000-000000000120/1/${commit}`;
  const submissions: unknown[] = [];
  let ready = false;
  let current = true;
  await page.route(
    (url) => url.pathname === `${root}/pulls/1/merge-candidate`,
    async (route) => {
      const request = route.request();
      if (request.method() === "POST") {
        submissions.push(request.postDataJSON());
        ready = true;
        if (submissions.length === 1) {
          await route.fulfill({
            status: 200,
            contentType: "application/json",
            body: "",
          });
        } else await route.fulfill({ json: { commit } });
        return;
      }
      await route.fulfill({
        json: {
          preview: ready ? { ...input, previous_oid: commit } : input,
          candidate: ready
            ? {
                commit,
                base_oid: pull.base_oid,
                head_oid: pull.head_oid,
                ref_name: "refs/crab/merge-queue/1",
                current,
                mirror_ref: mirror,
                can_remove: true,
              }
            : null,
        },
      });
    },
  );
  await page.route(
    (url) => url.pathname === `${root}/commits/${commit}/check-runs`,
    (route) => route.fulfill({ json: { sha: commit, items: [], next: null } }),
  );

  await page.goto("/team/project/pull/1/checks");
  const candidate = page.getByRole("region", { name: "Merge candidate" });
  await candidate
    .getByRole("button", { name: "Prepare merge candidate" })
    .click();
  await expect(candidate.getByRole("alert")).toContainText(
    "The reply was lost",
  );
  await candidate.getByRole("button", { name: "Retry preparation" }).click();
  await expect(candidate).toContainText(
    "git fetch origin refs/crab/merge-queue/1",
  );
  expect(submissions).toEqual([input, input]);
  const candidateChecks = page.waitForRequest(
    (request) =>
      new URL(request.url()).pathname ===
      `${root}/commits/${commit}/check-runs`,
  );
  await candidate.getByRole("link", { name: "Candidate checks" }).click();
  await candidateChecks;
  const refspec = page.getByRole("textbox", {
    name: "GitHub Actions mirror refspec",
  });
  await expect(refspec).toHaveValue(`${commit}:${mirror}`);
  await expect(
    page.getByRole("heading", { name: "No checks have been reported" }),
  ).toBeVisible();
  current = false;
  await candidate.getByRole("button", { name: "Refresh checks" }).click();
  await expect(candidate.getByRole("status")).toContainText(
    "Prepare a new candidate",
  );
  await expect(refspec).toBeHidden();
});

test("a tested merge retries the same commit and submission after an empty reply", async ({
  page,
}) => {
  const fixture = await mockPullRequest(page);
  const pull = fixture.pull();
  const commit = "c".repeat(40);
  const root = "/api/repos/team/project/pulls/1";
  const comparison = {
    version: pull.version,
    base_oid: pull.base_oid,
    head_oid: pull.head_oid,
  };
  const submissions: unknown[] = [];
  const merged = () => ({
    ...fixture.pull(),
    merge: {
      author: pull.author,
      method: "merge_commit",
      commit_oid: commit,
      tested_candidate_oid: commit,
      message: "",
      created_at: pull.updated_at,
    },
  });
  await page.route(
    (url) => url.pathname === root,
    (route) =>
      fixture.lifecycle.state === "merged"
        ? route.fulfill({ json: merged() })
        : route.fallback(),
  );
  await page.route(
    (url) => url.pathname === `${root}/merge-candidate`,
    (route) =>
      route.fulfill({
        json: {
          preview: { ...comparison, previous_oid: commit },
          candidate: {
            ...comparison,
            commit,
            ref_name: "refs/crab/merge-queue/1",
            current: true,
            mirror_ref: null,
            can_remove: true,
            promotion: "ready",
            required_checks: [
              {
                context: "ci/test",
                state: "success",
                provider: "crab_status",
                author: "Crab CI",
                head_sha: commit,
              },
            ],
          },
        },
      }),
  );
  await page.route(
    (url) => url.pathname === `${root}/merge`,
    (route) => {
      submissions.push(route.request().postDataJSON());
      fixture.lifecycle.state = "merged";
      return submissions.length === 1
        ? route.fulfill({
            status: 200,
            contentType: "application/json",
            body: "",
          })
        : route.fulfill({ json: merged() });
    },
  );

  await page.goto("/team/project/pull/1/checks?candidate=1");
  const candidate = page.getByRole("region", { name: "Merge candidate" });
  await expect(page.getByText("All required checks have passed")).toBeVisible();
  await expect(page.locator(".required-checks-summary code")).toHaveAttribute(
    "title",
    commit,
  );
  await candidate
    .getByRole("button", { name: "Merge tested candidate" })
    .click();
  await expect(candidate.getByRole("status")).toContainText(commit.slice(0, 7));
  await candidate.getByRole("button", { name: "Confirm tested merge" }).click();
  await expect(candidate.getByRole("alert")).toContainText(
    "The reply was lost",
  );
  await candidate.getByRole("button", { name: "Retry tested merge" }).click();

  await expect(
    page.getByRole("region", { name: "Merge status" }),
  ).toContainText("Published the tested merge candidate.");
  expect(submissions).toHaveLength(2);
  expect(submissions[0]).toEqual({
    ...comparison,
    tested_candidate_oid: commit,
    method: "merge_commit",
    request_id: expect.any(String),
  });
  expect(submissions[1]).toEqual(submissions[0]);
});

test("candidate checks link to the automatically dispatched run", async ({
  page,
}) => {
  const fixture = await mockPullRequest(page);
  const pull = fixture.pull();
  const root = "/api/repos/team/project/pulls/1";
  const commit = "d".repeat(40);
  const url = "https://github.com/example/mirror/actions/runs/456789";
  let started = false;
  await page.route(
    (address) => address.pathname === `${root}/merge-candidate`,
    (route) =>
      route.fulfill({
        json: {
          preview: null,
          candidate: {
            commit,
            base_oid: pull.base_oid,
            head_oid: pull.head_oid,
            ref_name: "refs/crab/merge-queue/1",
            current: true,
            mirror_ref: null,
            can_remove: false,
            dispatches: [
              {
                id: "ci",
                name: "ci/test",
                state: started ? "started" : "starting",
                url: started ? url : null,
              },
            ],
          },
        },
      }),
  );
  await page.goto("/team/project/pull/1/checks");
  const candidate = page.getByRole("region", { name: "Merge candidate" });
  await candidate.getByRole("link", { name: "Candidate checks" }).click();
  await expect(candidate).toContainText(
    "Waiting for GitHub to confirm the run.",
  );
  started = true;
  await page.reload();
  await expect(
    candidate.getByRole("link", { name: "Open GitHub Actions run" }),
  ).toHaveAttribute("href", url);
});
