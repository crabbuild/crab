import { Link } from "@/components/navigation/link";
import { repoHref } from "@/lib/repository-route";
import { Editor } from "@/components/discussion/editor";
import { Failure } from "@/components/discussion/failure";
import { useSubmission } from "@/components/discussion/hooks";
import { useMutation } from "@/components/discussion/mutations";
import { useContributionFocus } from "@/components/discussion/permalink";
import { Result } from "@/components/request/result";
import { Button } from "@/components/ui/button";
import {
  ChangeComparison,
  usePullComparison,
} from "@/features/repository/diff";
import {
  changePanelId,
  type DiffReviewState,
  type InlineDiffThread,
} from "@/features/repository/diff/model";
import { endpoint, useLocation, useRequest, type Repository } from "@/lib/api";
import type { CodeThemes } from "@/lib/code-theme";
import type { SelectedLineRange } from "@pierre/diffs";
import { useMemo, useState } from "react";

import type { PullRequest, PullReviewThread } from "../model";
import { usePagedRequest } from "../pagination";
import { branch } from "../presentation";
import { ReviewThreadCard } from "./thread-card";
import { useViewedFiles } from "./viewed";
import type { PendingReview } from "./pending";
import { SuggestionCommit, useSuggestionBatch } from "./suggestion";

function isOutdatedThread(thread: InlineDiffThread) {
  return thread.outdated || thread.vanished;
}

function isOpenThread(thread: InlineDiffThread) {
  return !thread.resolved && !isOutdatedThread(thread);
}

function isResolvedThread(thread: InlineDiffThread) {
  return thread.resolved && !isOutdatedThread(thread);
}

type ThreadSelection = {
  pathHex: string;
  range: SelectedLineRange;
  base: string;
  head: string;
};

export function PullReviewThreads({
  repo,
  pull,
  csrf,
  theme,
  codeThemes,
  refresh,
  pendingReview,
  submittingReview,
  unresolvedOnly,
}: {
  repo: Repository;
  pull: PullRequest;
  csrf: string;
  theme: "light" | "dark";
  codeThemes: CodeThemes;
  refresh: () => void;
  pendingReview: PendingReview;
  submittingReview: boolean;
  unresolvedOnly: boolean;
}) {
  const path = (before?: number) =>
    endpoint(repo, `pulls/${pull.number}/threads`, {
      outdated: "false",
      resolved: unresolvedOnly ? "false" : undefined,
      before: before?.toString(),
    });
  const outdatedPath = (before?: number) =>
    endpoint(repo, `pulls/${pull.number}/threads`, {
      outdated: "true",
      resolved: unresolvedOnly ? "false" : undefined,
      before: before?.toString(),
    });
  const threads = usePagedRequest<PullReviewThread>(path);
  const outdatedThreads = usePagedRequest<PullReviewThread>(outdatedPath);
  const location = useLocation();
  const focus = /#(review-thread-([1-9]\d*))$/.exec(location);
  const focusNumber = Number(focus?.[2]);
  const currentMatch = threads.items.some(
    (thread) => thread.number === focusNumber,
  );
  const outdatedMatch = outdatedThreads.items.some(
    (thread) => thread.number === focusNumber,
  );
  // A permalink can target a thread beyond both initial pages.
  const linkedThread = useRequest<PullReviewThread>(
    Number.isSafeInteger(focusNumber) &&
      !currentMatch &&
      !outdatedMatch &&
      !threads.loading &&
      !outdatedThreads.loading
      ? endpoint(repo, `pulls/${pull.number}/threads/${focusNumber}`)
      : null,
  );
  const linkedOutdated = linkedThread.data
    ? isOutdatedThread(linkedThread.data)
    : false;
  const currentItems =
    linkedThread.data && !linkedOutdated
      ? [linkedThread.data, ...threads.items]
      : threads.items;
  const outdatedItems =
    linkedThread.data && linkedOutdated
      ? [linkedThread.data, ...outdatedThreads.items]
      : outdatedThreads.items;
  useContributionFocus(
    focus?.[1],
    currentMatch || outdatedMatch || linkedThread.data !== undefined,
  );
  function refreshThreads() {
    threads.retry();
    outdatedThreads.retry();
    linkedThread.retry();
    refresh();
  }
  const suggestions = useSuggestionBatch(
    repo,
    pull,
    csrf,
    currentItems,
    refreshThreads,
  );
  const headRepo = pull.head_repository ?? repo;
  const comparison = usePullComparison(
    headRepo,
    pull.base_oid,
    pull.head_oid,
    pull.head_repository ? repo.id : undefined,
  );
  const preview = comparison.data;
  const viewed = useViewedFiles(repo, pull.number, csrf, preview?.changes);
  const [selection, setSelection] = useState<ThreadSelection>();
  const [body, setBody] = useState("");
  const [suggestion, setSuggestion] = useState("");
  const [suggestionEnabled, setSuggestionEnabled] = useState(false);
  const mutation = useMutation(csrf, repo);
  const submission = useSubmission();
  const reviewMetrics = useMemo(() => {
    const all = [...threads.items, ...outdatedThreads.items];
    return {
      open: all.filter(isOpenThread).length,
      resolved: all.filter(isResolvedThread).length,
      outdated: all.filter(isOutdatedThread).length,
    };
  }, [outdatedThreads.items, threads.items]);
  const fileSummaryVersion = useMemo(
    () =>
      [...threads.items, ...outdatedThreads.items]
        .map(
          (thread) =>
            `${thread.number}:${thread.resolved ? "r" : "o"}:${isOutdatedThread(thread) ? "x" : "c"}`,
        )
        .join("|"),
    [outdatedThreads.items, threads.items],
  );
  function clearSelection() {
    setSelection(undefined);
    setBody("");
    setSuggestion("");
    setSuggestionEnabled(false);
  }
  const review: DiffReviewState = {
    threads: threads.items,
    renderFileActions: viewed.renderFileActions,
    onLineSelected(pathHex, range) {
      if (!range || range.endSide || !preview || pull.merge_pending) {
        clearSelection();
        return;
      }
      mutation.reset();
      setSelection({
        pathHex,
        range,
        base: preview.base,
        head: preview.commit,
      });
      setBody("");
      setSuggestion("");
      setSuggestionEnabled(false);
    },
    onThreadSelected(thread) {
      const node = document.getElementById(`review-thread-${thread.number}`);
      node?.scrollIntoView({
        behavior: window.matchMedia("(prefers-reduced-motion: reduce)").matches
          ? "auto"
          : "smooth",
        block: "center",
      });
    },
    onDiffSelected(thread) {
      const panel = document.getElementById(changePanelId(thread.path_hex));
      panel?.scrollIntoView({
        behavior: window.matchMedia("(prefers-reduced-motion: reduce)").matches
          ? "auto"
          : "smooth",
        block: "center",
      });
      panel
        ?.querySelector<HTMLButtonElement>(
          `[data-thread-number="${thread.number}"]`,
        )
        ?.focus({ preventScroll: true });
    },
    fileSummary(pathHex) {
      const all = [...threads.items, ...outdatedThreads.items].filter(
        (thread) => thread.path_hex === pathHex,
      );
      if (!all.length) return null;
      return {
        total: all.length,
        open: all.filter(isOpenThread).length,
        resolved: all.filter(isResolvedThread).length,
        outdated: all.filter(isOutdatedThread).length,
      };
    },
    fileSummaryVersion,
    renderAnnotation(thread) {
      const preview = thread.body.replace(/\s+/g, " ").trim();
      return (
        <span className="inline-review-annotation-content">
          <span className="inline-review-annotation-number">
            #{thread.number}
          </span>
          <span className="inline-review-annotation-status">
            {thread.resolved ? "Resolved" : "Review comment"}
          </span>
          <span className="inline-review-annotation-preview">
            {preview || "No comment text"}
          </span>
        </span>
      );
    },
  };
  const selectedSide: "old" | "new" =
    selection?.range.side === "deletions" ? "old" : "new";
  const selectedStart = selection
    ? Math.min(selection.range.start, selection.range.end)
    : 0;
  const selectedEnd = selection
    ? Math.max(selection.range.start, selection.range.end)
    : 0;
  async function submit(event: React.FormEvent) {
    event.preventDefault();
    if (!selection || !body.trim()) return;
    const input = {
      body,
      suggested_text:
        selectedSide === "new" && suggestionEnabled ? suggestion : null,
      base_oid: selection.base,
      head_oid: selection.head,
      path_hex: selection.pathHex,
      side: selectedSide,
      start_line: selectedStart,
      end_line: selectedEnd,
    };
    const submitter = (event.nativeEvent as SubmitEvent).submitter;
    if (submitter?.getAttribute("value") === "pending-review") {
      const requestId = submission({
        ...input,
        version: pendingReview.draft?.version,
      });
      if (
        await pendingReview.change(
          {
            action: "save",
            comment: { ...input, request_id: requestId },
          },
          requestId,
        )
      )
        clearSelection();
      return;
    }
    const created = await mutation.run<PullReviewThread>(path(), "POST", {
      ...input,
      request_id: submission(input),
    });
    if (created) {
      clearSelection();
      refreshThreads();
    }
  }
  return (
    <section className="pull-review-threads" aria-label="Inline review threads">
      {unresolvedOnly && (
        <div className="section-heading">
          <h3>Unresolved review conversations</h3>
          <Link
            className="underline underline-offset-4"
            href={repoHref(repo, {
              view: "pulls",
              pull: String(pull.number),
              pull_tab: "files",
            })}
          >
            Show all conversations
          </Link>
        </div>
      )}
      <header className="inline-review-header">
        <div>
          <span className="inline-review-kicker">Review workspace</span>
          <h2>Inline review</h2>
          <p className="muted">
            Select a line or range in the diff to leave feedback. Your review
            stays attached to the exact version you inspected.
          </p>
        </div>
        <div
          className="inline-review-metrics"
          aria-label={`${reviewMetrics.open} open, ${reviewMetrics.resolved} resolved, ${reviewMetrics.outdated} outdated`}
        >
          <span className="inline-review-metric open">
            <strong>{reviewMetrics.open}</strong>
            <span>open</span>
          </span>
          <span className="inline-review-metric resolved">
            <strong>{reviewMetrics.resolved}</strong>
            <span>resolved</span>
          </span>
          <span className="inline-review-metric outdated">
            <strong>{reviewMetrics.outdated}</strong>
            <span>outdated</span>
          </span>
        </div>
      </header>
      {viewed.status}
      <ChangeComparison
        state={comparison}
        repo={headRepo}
        base={preview?.base}
        rev={preview?.commit ?? ""}
        theme={theme}
        codeThemes={codeThemes}
        review={review}
      />
      {linkedThread.error && (
        <Failure
          message={`Could not open the linked thread. ${linkedThread.error}`}
        />
      )}
      {selection &&
        pull.state === "open" &&
        !pull.merge_pending &&
        !repo.archived && (
          <form
            className="panel discussion-form inline-review-form"
            onSubmit={submit}
            aria-labelledby="inline-review-composer-heading"
          >
            <h3 id="inline-review-composer-heading">
              Comment on selected lines
            </h3>
            <p className="muted">
              {selectedSide === "old" ? "Old" : "New"} lines {selectedStart}–
              {selectedEnd} · <code>{selection.pathHex}</code>
            </p>
            <Editor
              id="inline-review-body"
              label="Comment"
              value={body}
              onChange={setBody}
              disabled={mutation.pending}
              required
            />
            {selectedSide === "new" && (
              <div className="inline-review-suggestion">
                <label>
                  <input
                    type="checkbox"
                    checked={suggestionEnabled}
                    disabled={mutation.pending}
                    onChange={(event) =>
                      setSuggestionEnabled(event.target.checked)
                    }
                  />{" "}
                  Include suggested replacement
                </label>
                {suggestionEnabled && (
                  <textarea
                    aria-label="Suggested replacement"
                    value={suggestion}
                    rows={4}
                    maxLength={65536}
                    disabled={mutation.pending}
                    onChange={(event) => setSuggestion(event.target.value)}
                  />
                )}
              </div>
            )}
            <Failure message={mutation.error} />
            <div className="discussion-actions">
              <Button
                type="submit"
                variant="default"
                disabled={
                  mutation.pending || pendingReview.pending || !body.trim()
                }
              >
                {mutation.pending ? "Posting…" : "Comment"}
              </Button>
              <Button
                type="submit"
                value="pending-review"
                variant="outline"
                disabled={
                  mutation.pending ||
                  pendingReview.pending ||
                  !pendingReview.ready ||
                  submittingReview ||
                  !body.trim()
                }
              >
                {pendingReview.pending ? "Saving…" : "Add to review"}
              </Button>
              <Button variant="outline" type="button" onClick={clearSelection}>
                Cancel
              </Button>
            </div>
          </form>
        )}
      <Result state={threads} timingLabel="Review threads">
        {() => (
          <div className="inline-review-list">
            <div className="section-heading">
              <div>
                <h3 id="inline-review-heading">Inline review threads</h3>
                <p className="muted">
                  {reviewMetrics.open
                    ? `${reviewMetrics.open} open ${reviewMetrics.open === 1 ? "conversation" : "conversations"} on this revision.`
                    : "No open conversations on this revision."}{" "}
                  Outdated threads remain visible for context.
                </p>
              </div>
              <div className="discussion-actions">
                {suggestions.enabled && suggestions.selected.length > 0 && (
                  <Button
                    size="sm"
                    disabled={suggestions.pending}
                    onClick={() => suggestions.review(suggestions.selected)}
                  >
                    {suggestions.pending
                      ? "Committing…"
                      : `Commit suggestions (${suggestions.selected.length})`}
                  </Button>
                )}
                <Button variant="outline" size="sm" onClick={refreshThreads}>
                  Refresh
                </Button>
              </div>
            </div>
            <SuggestionCommit
              suggestions={suggestions}
              branch={branch(pull.head_ref)}
            />
            {currentItems.length ? (
              currentItems.map((thread) => (
                <ReviewThreadCard
                  key={thread.number}
                  repo={repo}
                  pull={pull}
                  thread={thread}
                  csrf={csrf}
                  linked={thread.number === focusNumber}
                  onJumpToDiff={review.onDiffSelected}
                  refresh={refreshThreads}
                  suggestions={suggestions}
                />
              ))
            ) : (
              <p className="muted">
                {unresolvedOnly
                  ? "No unresolved threads on this revision."
                  : "No inline review threads yet."}
              </p>
            )}
            {threads.next !== null && (
              <Button variant="outline" size="sm" onClick={threads.loadMore}>
                Load more threads
              </Button>
            )}
          </div>
        )}
      </Result>
      <details
        className="inline-review-outdated"
        open={unresolvedOnly || outdatedMatch || linkedOutdated || undefined}
      >
        <summary>Outdated conversations ({outdatedItems.length})</summary>
        <Result state={outdatedThreads} timingLabel="Outdated review threads">
          {() =>
            outdatedItems.length ? (
              <div className="inline-review-list">
                {outdatedItems.map((thread) => (
                  <ReviewThreadCard
                    key={thread.number}
                    repo={repo}
                    pull={pull}
                    thread={thread}
                    csrf={csrf}
                    linked={thread.number === focusNumber}
                    onJumpToDiff={review.onDiffSelected}
                    refresh={refreshThreads}
                  />
                ))}
                {outdatedThreads.next !== null && (
                  <Button
                    variant="outline"
                    size="sm"
                    onClick={outdatedThreads.loadMore}
                  >
                    Load more outdated conversations
                  </Button>
                )}
              </div>
            ) : (
              <p className="muted">No outdated conversations.</p>
            )
          }
        </Result>
      </details>
    </section>
  );
}
