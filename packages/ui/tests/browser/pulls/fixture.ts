import { expect, type Locator, type Page } from "@playwright/test";
import { emptyReactions, emptyReviewRequests } from "../fixtures";

const base = "a".repeat(40);
const head = "b".repeat(40);
const pathHex = "README.md"
  .split("")
  .map((character) => character.charCodeAt(0).toString(16).padStart(2, "0"))
  .join("");

export async function mockPullRequest(page: Page) {
  const reactionStates = new Map<string, ReturnType<typeof emptyReactions>>();
  const reactionWrites: string[] = [];
  function reactions(target: string) {
    let summary = reactionStates.get(target);
    if (!summary) {
      summary = emptyReactions();
      reactionStates.set(target, summary);
    }
    return summary;
  }
  async function react(scope: Locator) {
    await scope
      .getByRole("button", { name: "Add reaction", exact: true })
      .click();
    await page
      .getByRole("menuitemcheckbox", { name: "Heart", exact: true })
      .click();
    await expect(
      scope.getByRole("button", { name: "Heart: 1 reaction", exact: true }),
    ).toHaveAttribute("aria-pressed", "true");
  }
  const lifecycle: {
    state: "open" | "closed" | "merged";
    branchesAvailable: boolean;
  } = { state: "open", branchesAvailable: true };
  let created = false;
  let mergePending = false;
  let mergeRequest = "";
  let mergeMethod: "fast_forward" | "merge_commit" = "merge_commit";
  let mergeMessage = "";
  let checkState: "success" | null = null;
  const comments: Array<Record<string, unknown>> = [];
  const reviews: Array<Record<string, unknown>> = [];
  const threads: Array<Record<string, unknown>> = [
    {
      number: 1,
      pull: 2,
      path: "README.md",
      path_hex: pathHex,
      side: "new",
      start_line: 1,
      end_line: 1,
      author: "Bob",
      body: "Consider a clearer opening.",
      reactions: reactions("pulls/2/threads/1"),
      suggested_text: "Applied content",
      base_oid: base,
      head_oid: head,
      old_blob_oid: "c".repeat(40),
      new_blob_oid: "d".repeat(40),
      resolved: false,
      outdated: false,
      vanished: false,
      resolver: null,
      current: true,
      version: 1,
      created_at: 1_700_000_060_000,
      updated_at: 1_700_000_060_000,
      can_edit: true,
      can_reply: true,
      can_resolve: true,
    },
  ];
  const replies: Array<Record<string, unknown>> = [];
  const pull = () => {
    const approvals = lifecycle.branchesAvailable && reviews.length ? 1 : 0;
    return {
      draft: false,
      number: created ? 2 : 1,
      title: created ? "Document the feature" : "Improve the README",
      body: created ? "This explains the **new behavior**." : "Please review.",
      reactions: reactions(`pulls/${created ? 2 : 1}`),
      state: lifecycle.state,
      author: "Alice",
      base_ref: "refs/heads/main",
      base_oid: base,
      head_ref: "refs/heads/feature/docs",
      head_oid: head,
      original_base_oid: base,
      original_head_oid: head,
      version: lifecycle.state === "open" ? 1 : 2,
      created_at: 1_700_000_000_000,
      updated_at: 1_700_000_000_000,
      can_edit: true,
      can_manage: !mergePending,
      can_decide: true,
      can_merge:
        lifecycle.state === "open" &&
        (mergePending ||
          (lifecycle.branchesAvailable &&
            approvals >= 1 &&
            checkState === "success")),
      branches_available:
        lifecycle.state === "merged" || lifecycle.branchesAvailable,
      merge_requirements: {
        protected: true,
        required_approvals: 1,
        approvals,
        changes_requested: 0,
        checks_satisfied: checkState === "success",
        checks: [
          {
            context: "ci/test",
            state: checkState,
            description:
              checkState === "success" ? "Tests passed in 42s" : null,
            target_url:
              checkState === "success" ? "https://ci.example.test/42" : null,
            author: checkState === "success" ? "CI service" : null,
            updated_at: checkState === "success" ? 1_700_000_040_000 : null,
            run_id: checkState === "success" ? 7 : null,
          },
        ],
        satisfied: approvals >= 1 && checkState === "success",
      },
      merge:
        lifecycle.state === "merged"
          ? {
              author: "Local operator",
              method: mergeMethod,
              commit_oid: head,
              message: mergeMessage,
              created_at: 1_700_000_200_000,
            }
          : null,
      merge_pending: mergePending
        ? {
            request_id: mergeRequest,
            author: "Local operator",
            method: mergeMethod,
            pull_version: 1,
            base_oid: base,
            head_oid: head,
            message: mergeMessage,
            created_at: 1_700_000_150_000,
          }
        : null,
    };
  };
  await page.route("**/api/**", async (route) => {
    const request = route.request();
    const url = new URL(request.url());
    const path = url.pathname;
    if (path.endsWith("/discussion-search"))
      return route.fulfill({
        json: {
          start: 0,
          end: (url.searchParams.get("query") ?? "").length,
          items: [],
          error: null,
        },
      });
    if (path.endsWith("/review-requests"))
      return route.fulfill({ json: emptyReviewRequests() });
    if (path.endsWith("/links"))
      return route.fulfill({
        json: { items: [], next: null, can_manage: false },
      });
    if (path.endsWith("/activity"))
      return route.fulfill({ json: { items: [], next: null } });
    if (/\/(issues|pulls)\/\d+\/milestone$/.test(path))
      return route.fulfill({
        json: { milestone: null, version: 0, can_manage: true },
      });
    if (path === "/api/session")
      return route.fulfill({
        json: { authenticated: true, mode: "local", user: null, csrf: null },
      });
    if (path === "/api/repos")
      return route.fulfill({
        json: {
          repositories: [
            {
              owner: "team",
              name: "project",
              description: "A repository for our team.",
              access: "write",
              archive_version: 0,
              archived: false,
              protection_version: 0,
              protected_branches: [
                {
                  branch: "main",
                  required_approvals: 1,
                  required_checks: ["ci/test"],
                },
              ],
            },
          ],
        },
      });
    if (path.endsWith("/refs"))
      return route.fulfill({
        json: {
          head: { name: "refs/heads/main", oid: base },
          unborn_head: null,
          refs: [
            { name: "refs/heads/main", oid: base },
            { name: "refs/heads/feature/docs", oid: head },
          ],
          generation: 2,
        },
      });
    if (path.endsWith("/changes"))
      return route.fulfill({
        json: {
          base,
          commit: head,
          changes: [
            {
              path: "README.md",
              path_hex: pathHex,
              kind: "Modified",
              old: {
                path: "README.md",
                path_hex: pathHex,
                kind: "Blob",
                oid: "c".repeat(40),
                mode: "100644",
              },
              new: {
                path: "README.md",
                path_hex: pathHex,
                kind: "Blob",
                oid: "d".repeat(40),
                mode: "100644",
              },
            },
          ],
        },
      });
    if (path.endsWith("/diff"))
      return route.fulfill({
        json: {
          base,
          commit: head,
          path: "README.md",
          old: {
            oid: "c".repeat(40),
            size: 11,
            mode: "100644",
            classification: "OrdinaryGit",
            text: "Old content\n",
          },
          new: {
            oid: "d".repeat(40),
            size: 11,
            mode: "100644",
            classification: "OrdinaryGit",
            text: "New content\n",
          },
        },
      });
    if (path === "/api/repos/team/project/file") {
      expect(url.searchParams.get("rev")).toBe(head);
      expect(url.searchParams.get("path_hex")).toBe(pathHex);
      return route.fulfill({
        json: {
          oid: "d".repeat(40),
          size: 12,
          mode: "100644",
          classification: "OrdinaryGit",
          text: "New content\n",
          text_truncated: false,
        },
      });
    }
    if (
      path === "/api/repos/team/project/contents" &&
      request.method() === "PATCH"
    ) {
      const input = request.postDataJSON();
      expect(input.branch).toBe("refs/heads/feature/docs");
      expect(input.expected_head).toBe(head);
      expect(input.expected_blob).toBe("d".repeat(40));
      expect(input.path_hex).toBe(pathHex);
      expect(input.content).toBe("Applied content\n");
      expect(input.message).toBe("Apply suggestion from review thread #1");
      threads[0].outdated = true;
      return route.fulfill({
        json: { commit: "e".repeat(40), path_hex: pathHex },
      });
    }
    if (path === "/api/repos/team/project/commits") {
      expect(url.searchParams.get("rev")).toBe(head);
      expect(url.searchParams.get("base")).toBe(base);
      expect(url.searchParams.get("limit")).toBe("50");
      return route.fulfill({
        json: {
          items: [
            {
              oid: head,
              tree: "e".repeat(40),
              parents: [base],
              author: "Alice",
              author_seconds: 1_700_000_040,
              message: "Document remote browsing\n\nExplain the workflow.",
            },
          ],
          next: null,
        },
      });
    }
    if (path === `/api/repos/team/project/commits/${head}/check-runs`) {
      expect(url.searchParams.get("limit")).toBe("50");
      return route.fulfill({
        json: {
          sha: head,
          items: [
            {
              id: 7,
              head_sha: head,
              name: "ci/test",
              status: "completed",
              conclusion: "success",
              details_url: "https://ci.example.test/runs/7",
              output_title: "All tests passed",
              author: "CI service",
              version: 3,
              started_at: 1_700_000_020_000,
              completed_at: 1_700_000_040_000,
              created_at: 1_700_000_010_000,
              updated_at: 1_700_000_040_000,
            },
          ],
          next: null,
        },
      });
    }
    if (path === `/api/repos/team/project/commits/${head}/check-runs/7`)
      return route.fulfill({
        json: {
          id: 7,
          head_sha: head,
          name: "ci/test",
          status: "completed",
          conclusion: "success",
          details_url: "https://ci.example.test/runs/7",
          output_title: "All tests passed",
          author: "CI service",
          version: 3,
          started_at: 1_700_000_020_000,
          completed_at: 1_700_000_040_000,
          created_at: 1_700_000_010_000,
          updated_at: 1_700_000_040_000,
          output: {
            title: "All tests passed",
            summary: "The **required test suite** passed.",
            text: "No failures were reported.",
            annotations: [
              {
                path: "src/lib.rs",
                start_line: 42,
                end_line: 44,
                level: "warning",
                title: "Slow assertion",
                message: "This assertion took longer than expected.",
              },
            ],
            steps: [
              {
                name: "Build and test",
                status: "completed",
                conclusion: "success",
                log: "44 passed; 0 failed\n<script>alert(1)</script>\n",
              },
            ],
          },
        },
      });
    if (path === "/api/repos/team/project/pulls") {
      if (request.method() === "POST") {
        created = true;
        return route.fulfill({ status: 201, json: pull() });
      }
      const query = url.searchParams.get("q")?.toLowerCase();
      const item = pull();
      const matches =
        !query ||
        [item.title, item.body, item.author].some((value) =>
          value.toLowerCase().includes(query),
        );
      return route.fulfill({
        json: {
          items: matches
            ? [
                {
                  ...item,
                  kind: "pull",
                  comment_count: comments.length,
                  reaction_count: 0,
                },
              ]
            : [],
          next: null,
          counts: {
            open: matches && item.state === "open" ? 1 : 0,
            closed: matches && item.state !== "open" ? 1 : 0,
            merged: matches && item.state === "merged" ? 1 : 0,
          },
          matching: matches ? 1 : 0,
          navigation: query
            ? { kind: "results" }
            : {
                kind: "state",
                selected: "open",
                open: "is:open",
                closed: "is:closed",
              },
        },
      });
    }
    if (path.includes("/reactions/")) {
      expect(request.method()).toBe("PUT");
      const target = path.split("/reactions/")[1];
      const summary = reactionStates.get(target);
      expect(summary).toBeDefined();
      const input = request.postDataJSON() as {
        kind: keyof ReturnType<typeof emptyReactions>["groups"];
        active: boolean;
      };
      summary!.groups[input.kind] = {
        count: input.active ? 1 : 0,
        reacted: input.active,
      };
      summary!.revision++;
      reactionWrites.push(target);
      return route.fulfill({ json: summary });
    }
    if (/\/pulls\/\d+$/.test(path)) {
      if (request.method() === "PATCH") {
        lifecycle.state = "closed";
        return route.fulfill({ json: pull() });
      }
      return route.fulfill({
        headers: { "Server-Timing": "handler;dur=12.5" },
        json: pull(),
      });
    }
    if (/\/pulls\/\d+\/comments$/.test(path)) {
      if (request.method() === "POST") {
        comments.push({
          number: 1,
          author: "Alice",
          body: "Verified in the browser.",
          reactions: reactions("pulls/2/comments/1"),
          version: 1,
          created_at: 1_700_000_100_000,
          updated_at: 1_700_000_100_000,
          can_edit: true,
        });
        return route.fulfill({ status: 201, json: comments[0] });
      }
      return route.fulfill({ json: { items: comments, next: null } });
    }
    if (/\/pulls\/\d+\/threads$/.test(path)) {
      if (request.method() === "POST") {
        const input = request.postDataJSON();
        const thread = {
          number: threads.length + 1,
          reactions: reactions(`pulls/2/threads/${threads.length + 1}`),
          pull: 2,
          path: "README.md",
          path_hex: input.path_hex,
          side: input.side,
          start_line: input.start_line,
          end_line: input.end_line,
          author: "Alice",
          body: input.body,
          suggested_text: input.suggested_text,
          base_oid: input.base_oid,
          head_oid: input.head_oid,
          old_blob_oid: "c".repeat(40),
          new_blob_oid: "d".repeat(40),
          resolved: false,
          outdated: false,
          vanished: false,
          resolver: null,
          current: true,
          version: 1,
          created_at: 1_700_000_070_000,
          updated_at: 1_700_000_070_000,
          can_edit: true,
          can_reply: true,
          can_resolve: true,
        };
        threads.push(thread);
        return route.fulfill({ status: 201, json: thread });
      }
      const outdated = url.searchParams.get("outdated") === "true";
      return route.fulfill({
        json: {
          items: threads.filter(
            (thread) => Boolean(thread.outdated) === outdated,
          ),
          next: null,
        },
      });
    }
    if (/\/pulls\/\d+\/threads\/\d+\/replies$/.test(path)) {
      if (request.method() === "POST") {
        const input = request.postDataJSON();
        const reply = {
          number: replies.length + 1,
          reactions: reactions(
            `pulls/2/threads/${Number(path.match(/threads\/(\d+)\/replies$/)?.[1])}/replies/${replies.length + 1}`,
          ),
          pull: 2,
          thread: Number(path.match(/threads\/(\d+)\/replies$/)?.[1] ?? 1),
          author: "Alice",
          body: input.body,
          version: 1,
          created_at: 1_700_000_080_000,
          updated_at: 1_700_000_080_000,
          can_edit: true,
        };
        replies.push(reply);
        return route.fulfill({ status: 201, json: reply });
      }
      const thread = Number(path.match(/threads\/(\d+)\/replies$/)?.[1] ?? 1);
      return route.fulfill({
        json: {
          items: replies.filter((reply) => reply.thread === thread),
          next: null,
        },
      });
    }
    if (/\/pulls\/\d+\/threads\/\d+$/.test(path)) {
      const number = Number(path.match(/threads\/(\d+)$/)?.[1] ?? 1);
      const thread = threads.find((item) => item.number === number);
      if (!thread) return route.fulfill({ status: 404 });
      if (request.method() === "PATCH") {
        const input = request.postDataJSON();
        if (typeof input.resolved === "boolean")
          thread.resolved = input.resolved;
        thread.version = Number(thread.version) + 1;
        return route.fulfill({ json: thread });
      }
      return route.fulfill({ json: thread });
    }
    if (/\/pulls\/\d+\/reviews$/.test(path)) {
      if (request.method() === "POST") {
        checkState = "success";
        reviews.push({
          number: 1,
          author: "Bob",
          body: "Ready to merge.",
          reactions: reactions("pulls/2/reviews/1"),
          state: "approved",
          commit_oid: head,
          current: lifecycle.branchesAvailable,
          version: 1,
          created_at: 1_700_000_050_000,
          updated_at: 1_700_000_050_000,
          can_edit: true,
        });
        return route.fulfill({ status: 201, json: reviews[0] });
      }
      return route.fulfill({
        json: {
          items: reviews.map((review) => ({
            ...review,
            current: lifecycle.branchesAvailable,
          })),
          next: null,
        },
      });
    }
    if (/\/pulls\/\d+\/merge$/.test(path)) {
      const input = request.postDataJSON();
      if (!mergePending) {
        mergePending = true;
        mergeRequest = input.request_id;
        mergeMethod = input.method;
        mergeMessage = input.message;
        expect(input.method).toBe("merge_commit");
        expect(input.message).toBe("Merge pull request #2 from feature/docs");
        return route.fulfill({
          status: 503,
          json: {
            error: {
              message:
                "The merge may have completed. Reload the pull request before retrying the same submission",
            },
          },
        });
      }
      expect(input.request_id).toBe(mergeRequest);
      mergePending = false;
      lifecycle.state = "merged";
      return route.fulfill({ json: pull() });
    }
    return route.fulfill({
      status: 404,
      json: { error: { message: "Fixture route unavailable" } },
    });
  });

  return { react, reactionWrites, lifecycle };
}
