import { Disclosure } from "@/components/ui/collapsible";
import { Textarea } from "@/components/ui/textarea";
import { Checkbox } from "@/components/ui/checkbox";
import { useCommentVisibility } from "@/components/discussion/conversation/comment-visibility";
import { useConversationLock } from "@/components/discussion/conversation/lock";
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
  type DiffReviewFileSummary,
  type DiffReviewState,
  type DiffTarget,
  type InlineDiffThread,
} from "@/features/repository/diff/model";
import {
  endpoint,
  navigate,
  useLocation,
  useRequest,
  type Change,
  type Repository,
} from "@/lib/api";
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

type ThreadSelection = {
  file: Change;
  range: SelectedLineRange;
  base: string;
  head: string;
};

type ReviewNavigation = {
  base_oid: string;
  head_oid: string;
  summary: {
    open: number;
    resolved: number;
    outdated: number;
    outdated_unresolved: number;
    previous: number | null;
    next: number | null;
  };
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
  const visibility = useCommentVisibility();
  const canPost = useConversationLock()?.lock?.can_post !== false;
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
  const focus = /#review-(thread|diff)-([1-9]\d*)$/.exec(location);
  const number = Number(focus?.[2]);
  const focusNumber = Number.isSafeInteger(number) ? number : undefined;
  const currentMatch = threads.items.find(
    (thread) => thread.number === focusNumber,
  );
  const outdatedMatch = outdatedThreads.items.find(
    (thread) => thread.number === focusNumber,
  );
  // A permalink can target a thread beyond both initial pages.
  const linkedThread = useRequest<PullReviewThread>(
    focusNumber !== undefined &&
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
  const selected = currentMatch ?? outdatedMatch ?? linkedThread.data;
  const [manualJump, setManualJump] = useState<{
    location: string;
    target: DiffTarget;
  }>();
  const jumpTarget =
    manualJump?.location === location ? manualJump.target : undefined;
  const navigation = useRequest<ReviewNavigation>(
    endpoint(repo, `pulls/${pull.number}/threads/navigation`, {
      around: (jumpTarget?.thread ?? focusNumber)?.toString(),
    }),
  );
  function refreshThreads() {
    threads.retry();
    outdatedThreads.retry();
    linkedThread.retry();
    navigation.retry();
    refresh();
  }
  const suggestions = useSuggestionBatch(repo, pull, csrf, refreshThreads);
  const savedComparison = pull.merge
    ? `pulls/${pull.number}/merged-comparison`
    : undefined;
  const comparisonRepo = pull.merge ? repo : (pull.head_repository ?? repo);
  const comparison = usePullComparison(
    comparisonRepo,
    pull.base_oid,
    pull.head_oid,
    pull.head_repository ? repo.id : undefined,
    savedComparison,
  );
  const preview = comparison.data;
  const comparisonChanged =
    navigation.data &&
    preview &&
    (navigation.data.base_oid !== preview.base ||
      navigation.data.head_oid !== preview.commit);
  const summary = comparisonChanged ? undefined : navigation.data?.summary;
  const directTarget = useMemo(
    () =>
      focus?.[1] === "diff" &&
      selected &&
      !isOutdatedThread(selected) &&
      selected.base_oid === preview?.base &&
      selected.head_oid === preview?.commit
        ? { pathHex: selected.path_hex, thread: selected.number }
        : undefined,
    [focus?.[1], preview?.base, preview?.commit, selected],
  );
  const focusConversation =
    focus?.[1] === "thread" ||
    (selected &&
      (isOutdatedThread(selected) || (!preview && !comparison.loading)));
  useContributionFocus(
    focusNumber && focusConversation
      ? `review-thread-${focusNumber}`
      : undefined,
    selected !== undefined,
  );
  const viewed = useViewedFiles(repo, pull.number, csrf, preview?.changes);
  const [selection, setSelection] = useState<ThreadSelection>();
  const [body, setBody] = useState("");
  const [suggestion, setSuggestion] = useState("");
  const [suggestionEnabled, setSuggestionEnabled] = useState(false);
  const mutation = useMutation(csrf, repo);
  const submission = useSubmission();
  const fileSummaries = useMemo(() => {
    const summaries = new Map<string, DiffReviewFileSummary>();
    for (const thread of [...threads.items, ...outdatedThreads.items]) {
      const file = summaries.get(thread.path_hex) ?? {
        total: 0,
        open: 0,
        resolved: 0,
        outdated: 0,
      };
      file.total += 1;
      if (isOutdatedThread(thread)) file.outdated += 1;
      else if (thread.resolved) file.resolved += 1;
      else file.open += 1;
      summaries.set(thread.path_hex, file);
    }
    return summaries;
  }, [outdatedThreads.items, threads.items]);
  const incompleteFiles =
    unresolvedOnly ||
    threads.loading ||
    outdatedThreads.loading ||
    Boolean(threads.error || outdatedThreads.error) ||
    threads.next !== null ||
    outdatedThreads.next !== null;
  const fileSummaryVersion = useMemo(
    () => JSON.stringify([incompleteFiles, [...fileSummaries]]),
    [fileSummaries, incompleteFiles],
  );
  function navigateToThread(target: number) {
    setManualJump(undefined);
    navigate(`${location.split("#")[0]}#review-diff-${target}`);
  }
  function clearSelection() {
    setSelection(undefined);
    setBody("");
    setSuggestion("");
    setSuggestionEnabled(false);
  }
  function jumpToDiff(thread: InlineDiffThread) {
    setManualJump({
      location,
      target: { pathHex: thread.path_hex, thread: thread.number },
    });
  }
  const review: DiffReviewState = {
    preferenceScope: `${repo.id}-${pull.number}`,
    threads: currentItems,
    target: jumpTarget ?? directTarget,
    isViewed: viewed.isViewed,
    renderFileActions: viewed.renderFileActions,
    onLineSelected(file, range) {
      if (!range || range.endSide || !preview || pull.merge_pending) {
        clearSelection();
        return;
      }
      mutation.reset();
      setSelection({
        file,
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
    fileSummary: (pathHex) => fileSummaries.get(pathHex) ?? null,
    fileSummaryPartial: incompleteFiles,
    fileSummaryVersion,
    renderAnnotation(thread) {
      const target = `pulls/${pull.number}/threads/${thread.number}`;
      const minimized = visibility.current(target, thread.minimization)?.reason;
      const preview = minimized
        ? "Comment hidden"
        : thread.body.replace(/\s+/g, " ").trim();
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
      path_hex: selection.file.path_hex,
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
    if (!canPost) return;
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
          <h2>Inline review</h2>
          <p className="muted">
            Select a line or range in the diff to leave feedback. Your review
            stays attached to the exact version you inspected.
          </p>
        </div>
      </header>
      <nav
        className="mb-4 flex flex-wrap items-center justify-between gap-2"
        aria-label="Review conversation navigation"
      >
        <div role="status" className="text-sm text-muted-foreground">
          {summary
            ? `${summary.open} open · ${summary.resolved} resolved · ${summary.outdated} outdated`
            : navigation.error
              ? "Conversation counts unavailable."
              : comparisonChanged
                ? "The comparison changed. Refresh to continue."
                : "Loading conversation counts…"}
        </div>
        <div className="flex flex-wrap gap-2">
          {(navigation.error || comparisonChanged) && (
            <Button variant="outline" size="sm" onClick={refreshThreads}>
              Refresh conversations
            </Button>
          )}
          <Button
            variant="outline"
            size="sm"
            disabled={summary?.previous == null}
            onClick={() =>
              summary?.previous != null && navigateToThread(summary.previous)
            }
          >
            Previous unresolved
          </Button>
          <Button
            variant="outline"
            size="sm"
            disabled={summary?.next == null}
            onClick={() =>
              summary?.next != null && navigateToThread(summary.next)
            }
          >
            Next unresolved
          </Button>
        </div>
      </nav>
      {viewed.status}
      <ChangeComparison
        state={comparison}
        repo={comparisonRepo}
        base={preview?.base}
        rev={preview?.commit ?? ""}
        theme={theme}
        codeThemes={codeThemes}
        review={review}
        fileAction={savedComparison && `${savedComparison}/file`}
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
              {selectedSide === "old" ? "Old" : "New"}{" "}
              {selectedStart === selectedEnd
                ? `line ${selectedStart}`
                : `lines ${selectedStart}–${selectedEnd}`}{" "}
              · <code className="wrap-anywhere">{selection.file.path}</code>
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
                  <Checkbox
                    checked={suggestionEnabled}
                    disabled={mutation.pending}
                    onCheckedChange={(checked) =>
                      setSuggestionEnabled(checked === true)
                    }
                  />{" "}
                  Include suggested replacement
                </label>
                {suggestionEnabled && (
                  <Textarea
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
                  !canPost ||
                  mutation.pending ||
                  pendingReview.pending ||
                  !body.trim()
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
                  {incompleteFiles
                    ? "File indicators count conversations loaded below."
                    : "Outdated threads remain visible for context."}
                </p>
              </div>
              <div className="discussion-actions">
                {suggestions.enabled && suggestions.selected.length > 0 && (
                  <Button
                    size="sm"
                    disabled={suggestions.pending}
                    onClick={() => suggestions.review()}
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
                  onJumpToDiff={jumpToDiff}
                  refresh={refreshThreads}
                  suggestions={suggestions}
                  allowBatch
                />
              ))
            ) : (
              <p className="muted">
                {unresolvedOnly
                  ? "No unresolved threads on this revision."
                  : "No inline review threads on this revision."}
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
      <Disclosure
        className="inline-review-outdated"
        expanded={
          unresolvedOnly ||
          Boolean(outdatedMatch) ||
          linkedOutdated ||
          undefined
        }
        summary={
          <>
            Outdated conversations
            {summary &&
              ` (${unresolvedOnly ? summary.outdated_unresolved : summary.outdated})`}
          </>
        }
      >
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
                    onJumpToDiff={jumpToDiff}
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
      </Disclosure>
    </section>
  );
}
