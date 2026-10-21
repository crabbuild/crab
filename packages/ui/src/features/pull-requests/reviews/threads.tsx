import { Editor } from "@/components/discussion/editor";
import { Failure } from "@/components/discussion/failure";
import { useSubmission } from "@/components/discussion/hooks";
import { useMutation } from "@/components/discussion/mutations";
import { Result } from "@/components/request/result";
import { Button } from "@/components/ui/button";
import { ComparisonView } from "@/features/repository/diff";
import {
  changePanelId,
  type DiffReviewState,
  type InlineDiffThread,
} from "@/features/repository/diff/model";
import { endpoint, type Repository } from "@/lib/api";
import type { CodeThemes } from "@/lib/code-theme";
import type { SelectedLineRange } from "@pierre/diffs";
import { useMemo, useState } from "react";

import type { PullRequest, PullReviewThread } from "../model";
import { usePagedRequest } from "../pagination";
import { ReviewThreadCard } from "./thread-card";

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
};

export function PullReviewThreads({
  repo,
  pull,
  csrf,
  theme,
  codeThemes,
  refresh,
}: {
  repo: Repository;
  pull: PullRequest;
  csrf: string;
  theme: "light" | "dark";
  codeThemes: CodeThemes;
  refresh: () => void;
}) {
  const path = (before?: number) =>
    endpoint(repo, `pulls/${pull.number}/threads`, {
      outdated: "false",
      before: before?.toString(),
    });
  const outdatedPath = (before?: number) =>
    endpoint(repo, `pulls/${pull.number}/threads`, {
      outdated: "true",
      before: before?.toString(),
    });
  const threads = usePagedRequest<PullReviewThread>(path);
  const outdatedThreads = usePagedRequest<PullReviewThread>(outdatedPath);
  const [selection, setSelection] = useState<ThreadSelection>();
  const [body, setBody] = useState("");
  const [suggestion, setSuggestion] = useState("");
  const [suggestionEnabled, setSuggestionEnabled] = useState(false);
  const mutation = useMutation(csrf);
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
    onLineSelected(pathHex, range) {
      if (!range || range.endSide) {
        clearSelection();
        return;
      }
      setSelection({ pathHex, range });
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
  const selectedSide = selection?.range.side === "deletions" ? "old" : "new";
  const selectedStart = selection
    ? Math.min(selection.range.start, selection.range.end)
    : 0;
  const selectedEnd = selection
    ? Math.max(selection.range.start, selection.range.end)
    : 0;
  async function submit(event: React.FormEvent) {
    event.preventDefault();
    if (!selection) return;
    const input = {
      body,
      suggested_text:
        selectedSide === "new" && suggestionEnabled ? suggestion : null,
      base_oid: pull.base_oid,
      head_oid: pull.head_oid,
      path_hex: selection.pathHex,
      side: selectedSide,
      start_line: selectedStart,
      end_line: selectedEnd,
    };
    const created = await mutation.run<PullReviewThread>(path(), "POST", {
      ...input,
      request_id: submission(input),
    });
    if (created) {
      clearSelection();
      threads.retry();
      outdatedThreads.retry();
    }
  }
  return (
    <section className="pull-review-threads" aria-label="Inline review threads">
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
      <ComparisonView
        repo={repo}
        base={pull.base_oid}
        head={pull.head_oid}
        theme={theme}
        codeThemes={codeThemes}
        review={review}
      />
      {selection && pull.state === "open" && !repo.archived && (
        <form
          className="panel discussion-form inline-review-form"
          onSubmit={submit}
          aria-labelledby="inline-review-composer-heading"
        >
          <h3 id="inline-review-composer-heading">Comment on selected lines</h3>
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
              disabled={mutation.pending || !body.trim()}
            >
              {mutation.pending ? "Posting…" : "Comment"}
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
              <Button variant="outline" size="sm" onClick={threads.retry}>
                Refresh
              </Button>
            </div>
            {threads.items.length ? (
              threads.items.map((thread) => (
                <ReviewThreadCard
                  key={thread.number}
                  repo={repo}
                  pull={pull}
                  thread={thread}
                  csrf={csrf}
                  onJumpToDiff={review.onDiffSelected}
                  refresh={() => {
                    threads.retry();
                    outdatedThreads.retry();
                    refresh();
                  }}
                />
              ))
            ) : (
              <p className="muted">No inline review threads yet.</p>
            )}
            {threads.next !== null && (
              <Button variant="outline" size="sm" onClick={threads.loadMore}>
                Load more threads
              </Button>
            )}
          </div>
        )}
      </Result>
      <details className="inline-review-outdated">
        <summary>
          Outdated conversations ({outdatedThreads.items.length})
        </summary>
        <Result state={outdatedThreads} timingLabel="Outdated review threads">
          {() =>
            outdatedThreads.items.length ? (
              <div className="inline-review-list">
                {outdatedThreads.items.map((thread) => (
                  <ReviewThreadCard
                    key={thread.number}
                    repo={repo}
                    pull={pull}
                    thread={thread}
                    csrf={csrf}
                    onJumpToDiff={review.onDiffSelected}
                    refresh={() => {
                      threads.retry();
                      outdatedThreads.retry();
                      refresh();
                    }}
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
