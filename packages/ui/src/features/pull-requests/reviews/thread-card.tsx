import { Disclosure } from "@/components/ui/collapsible";
import { EditHistory } from "@/components/discussion/conversation/edit-history";
import {
  CommentVisibility,
  useCommentVisibility,
} from "@/components/discussion/conversation/comment-visibility";
import { useConversationLock } from "@/components/discussion/conversation/lock";
import { Author } from "@/components/discussion/author";
import { endpoint, useRequest, type Repository } from "@/lib/api";
import { Reactions } from "@/components/discussion/reactions";
import { Editor } from "@/components/discussion/editor";
import { Failure } from "@/components/discussion/failure";
import { DiscussionMarkdown } from "@/components/discussion/markdown";
import { useMutation } from "@/components/discussion/mutations";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { Link } from "@/components/navigation/link";
import { Result } from "@/components/request/result";
import { repoHref } from "@/lib/repository-route";
import { useState } from "react";

import type { PullRequest, PullReviewThread } from "../model";
import { branch } from "../presentation";
import { ReviewReplies } from "./replies";
import {
  canApplySuggestion,
  SuggestionCommit,
  type SuggestionBatch,
} from "./suggestion";

function initials(value: string) {
  const parts = value.trim().split(/\s+/).filter(Boolean);
  const last = parts[parts.length - 1];
  return (
    (parts.length > 1
      ? `${parts[0][0]}${last?.[0]}`
      : parts[0]?.slice(0, 2)
    )?.toUpperCase() || "?"
  );
}

export function LinkedReviewThread({
  repo,
  pull,
  csrf,
  number,
  reply,
  refresh,
  suggestions,
}: {
  repo: Repository;
  pull: PullRequest;
  csrf: string;
  number: number;
  reply?: number;
  refresh: () => void;
  suggestions: SuggestionBatch;
}) {
  const thread = useRequest<PullReviewThread>(
    endpoint(repo, `pulls/${pull.number}/threads/${number}`),
  );
  return (
    <section
      className="inline-review-list"
      aria-label="Linked review conversation"
    >
      <Button asChild variant="outline" size="sm" className="w-fit">
        <Link
          href={repoHref(repo, {
            view: "pulls",
            pull: String(pull.number),
            pull_tab: "files",
          })}
        >
          Back to files changed
        </Link>
      </Button>
      <SuggestionCommit
        suggestions={suggestions}
        branch={branch(pull.head_ref)}
        onSaved={() => {
          thread.retry();
          refresh();
        }}
      />
      <Result state={thread} timingLabel="Review conversation">
        {(data) => (
          <ReviewThreadCard
            repo={repo}
            pull={pull}
            thread={data}
            csrf={csrf}
            refresh={thread.retry}
            linked
            suggestions={suggestions}
            reply={reply}
          />
        )}
      </Result>
    </section>
  );
}

export function ReviewThreadCard({
  repo,
  pull,
  thread,
  csrf,
  onJumpToDiff,
  refresh,
  suggestions,
  allowBatch,
  linked,
  reply,
}: {
  repo: Repository;
  pull: PullRequest;
  thread: PullReviewThread;
  csrf: string;
  onJumpToDiff?: (thread: PullReviewThread) => void;
  refresh: () => void;
  suggestions?: SuggestionBatch;
  allowBatch?: boolean;
  linked?: boolean;
  reply?: number;
}) {
  const mutation = useMutation(csrf, repo);
  const [editing, setEditing] = useState(false);
  const [editBody, setEditBody] = useState(thread.body);
  const lineLocation =
    thread.start_line === thread.end_line
      ? `line ${thread.start_line}`
      : `lines ${thread.start_line}–${thread.end_line}`;
  const selected = suggestions?.selected.includes(thread.number);
  const target = `pulls/${pull.number}/threads/${thread.number}`;
  const minimized = useCommentVisibility().current(
    target,
    thread.minimization,
  )?.reason;
  const canPost = useConversationLock()?.lock?.can_post !== false;
  async function saveEdit(event: React.FormEvent) {
    event.preventDefault();
    if (!canPost) return;
    const updated = await mutation.run<PullReviewThread>(
      endpoint(repo, `pulls/${pull.number}/threads/${thread.number}`),
      "PATCH",
      { version: thread.version, body: editBody },
    );
    if (updated) {
      setEditing(false);
      refresh();
    }
  }
  async function toggleResolved() {
    const updated = await mutation.run<PullReviewThread>(
      endpoint(repo, `pulls/${pull.number}/threads/${thread.number}`),
      "PATCH",
      { version: thread.version, resolved: !thread.resolved },
    );
    if (updated) refresh();
  }
  return (
    <article
      id={`review-thread-${thread.number}`}
      tabIndex={-1}
      className={`panel review-thread-card${thread.outdated ? " outdated" : ""}${thread.resolved ? " resolved" : ""}`}
    >
      <Disclosure
        className="review-thread-details"
        expanded={linked || !thread.resolved || undefined}
        summaryClassName="review-thread-summary"
        summary={
          <>
            <span className="review-thread-summary-main">
              <span className="review-author-avatar" aria-hidden="true">
                {initials(thread.author.profile?.name ?? thread.author.name)}
              </span>
              <span className="review-thread-summary-copy">
                <span className="review-thread-author-line">
                  <strong>
                    {thread.author.profile?.handle ?? thread.author.name}
                  </strong>{" "}
                  <span className="muted">left a review comment</span>
                </span>
                <span className="review-thread-location">
                  <code>{thread.path}</code>
                  <span>{lineLocation}</span>
                  <span>{thread.side === "old" ? "old file" : "new file"}</span>
                </span>
              </span>
            </span>
            <span className="review-thread-statuses">
              {!minimized &&
                thread.suggested_text !== null &&
                !thread.outdated && (
                  <span className="review-thread-badge suggestion">
                    Suggestion
                  </span>
                )}
              {thread.outdated && (
                <span className="review-thread-badge">Outdated</span>
              )}
              {thread.resolved && (
                <span className="review-thread-badge resolved">Resolved</span>
              )}
            </span>
          </>
        }
      >
        <div className="review-thread-content">
          <CommentVisibility
            repo={repo}
            csrf={csrf}
            target={target}
            minimization={thread.minimization}
            onChanged={refresh}
          >
            {thread.has_edit_history && (
              <EditHistory repo={repo} csrf={csrf} target={target} />
            )}
            <div className="review-thread-body">
              {editing ? (
                <form className="review-thread-edit" onSubmit={saveEdit}>
                  <Editor
                    id={`thread-${thread.number}-edit`}
                    label="Edit comment"
                    value={editBody}
                    onChange={setEditBody}
                    disabled={mutation.pending}
                    required
                    autoFocus
                  />
                  <div className="discussion-actions">
                    <Button
                      size="sm"
                      variant="default"
                      type="submit"
                      disabled={
                        !canPost || mutation.pending || !editBody.trim()
                      }
                    >
                      {mutation.pending ? "Saving…" : "Save"}
                    </Button>
                    <Button
                      variant="outline"
                      size="sm"
                      type="button"
                      disabled={mutation.pending}
                      onClick={() => {
                        setEditBody(thread.body);
                        setEditing(false);
                      }}
                    >
                      Cancel
                    </Button>
                  </div>
                </form>
              ) : (
                <DiscussionMarkdown>{thread.body}</DiscussionMarkdown>
              )}
            </div>
            <Reactions
              repo={repo}
              csrf={csrf}
              target={`pulls/${pull.number}/threads/${thread.number}`}
              summary={thread.reactions}
            />
            {thread.suggested_text !== null && (
              <div className="review-suggestion">
                <pre>{thread.suggested_text || "Remove selected lines"}</pre>
                {suggestions?.enabled && canApplySuggestion(thread) && (
                  <div className="discussion-actions">
                    <Button
                      size="sm"
                      disabled={suggestions.pending}
                      onClick={() => suggestions.review(thread)}
                    >
                      Apply suggestion
                    </Button>
                    {allowBatch && (
                      <label className="inline-flex items-center gap-2 text-sm">
                        <Checkbox
                          checked={selected}
                          aria-label={`Select suggestion from thread #${thread.number}`}
                          disabled={
                            suggestions.pending ||
                            !!suggestions.retry ||
                            (suggestions.atLimit && !selected)
                          }
                          onCheckedChange={() => suggestions.toggle(thread)}
                        />
                        Include in batch
                      </label>
                    )}
                  </div>
                )}
              </div>
            )}
            <div className="discussion-actions review-thread-actions">
              {thread.author.profile && <Author author={thread.author} />}
              {thread.can_edit && canPost && !repo.archived && !editing && (
                <Button
                  variant="outline"
                  size="sm"
                  disabled={mutation.pending}
                  onClick={() => {
                    setEditBody(thread.body);
                    setEditing(true);
                  }}
                >
                  Edit
                </Button>
              )}
            </div>
          </CommentVisibility>
          <div className="discussion-actions review-thread-actions">
            {onJumpToDiff && !thread.outdated && !thread.vanished && (
              <Button
                variant="outline"
                size="sm"
                onClick={() => onJumpToDiff(thread)}
                disabled={mutation.pending}
              >
                Jump to diff
              </Button>
            )}
            {thread.can_resolve &&
              !repo.archived &&
              !(thread.resolved && pull.merge_pending) && (
                <Button
                  variant="outline"
                  size="sm"
                  disabled={mutation.pending}
                  onClick={toggleResolved}
                >
                  {thread.resolved ? "Reopen" : "Resolve"}
                </Button>
              )}
          </div>
          {thread.outdated && (
            <p className="muted review-thread-warning">
              This comment is attached to an earlier version of the file. The
              path or anchored lines may no longer exist.
            </p>
          )}
          <ReviewReplies
            repo={repo}
            pull={pull}
            thread={thread}
            csrf={csrf}
            selected={reply}
          />
          <Failure message={mutation.error} />
        </div>
      </Disclosure>
    </article>
  );
}
