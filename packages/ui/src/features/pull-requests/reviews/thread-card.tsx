import { endpoint, type Repository } from "@/lib/api";
import { Reactions } from "@/components/discussion/reactions";
import { Editor } from "@/components/discussion/editor";
import { Failure } from "@/components/discussion/failure";
import { DiscussionMarkdown } from "@/components/discussion/markdown";
import { useMutation } from "@/components/discussion/mutations";
import { Button } from "@/components/ui/button";
import { useState } from "react";

import type { PullRequest, PullReviewThread } from "../model";
import { ReviewReplies } from "./replies";
import { SuggestionAction } from "./suggestion";

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

export function ReviewThreadCard({
  repo,
  pull,
  thread,
  csrf,
  onJumpToDiff,
  refresh,
}: {
  repo: Repository;
  pull: PullRequest;
  thread: PullReviewThread;
  csrf: string;
  onJumpToDiff?: (thread: PullReviewThread) => void;
  refresh: () => void;
}) {
  const mutation = useMutation(csrf);
  const [editing, setEditing] = useState(false);
  const [editBody, setEditBody] = useState(thread.body);
  const lineLocation =
    thread.start_line === thread.end_line
      ? `line ${thread.start_line}`
      : `lines ${thread.start_line}–${thread.end_line}`;
  async function saveEdit(event: React.FormEvent) {
    event.preventDefault();
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
      className={`panel review-thread-card${thread.outdated ? " outdated" : ""}${thread.resolved ? " resolved" : ""}`}
    >
      <details
        className="review-thread-details"
        open={!thread.resolved || undefined}
      >
        <summary className="review-thread-summary">
          <span className="review-thread-summary-main">
            <span className="review-author-avatar" aria-hidden="true">
              {initials(thread.author)}
            </span>
            <span className="review-thread-summary-copy">
              <span className="review-thread-author-line">
                <strong>{thread.author}</strong>
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
            {thread.suggested_text !== null && !thread.outdated && (
              <span className="review-thread-badge suggestion">Suggestion</span>
            )}
            {thread.outdated && (
              <span className="review-thread-badge">Outdated</span>
            )}
            {thread.resolved && (
              <span className="review-thread-badge resolved">Resolved</span>
            )}
          </span>
        </summary>
        <div className="review-thread-content">
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
                    disabled={mutation.pending || !editBody.trim()}
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
          {thread.suggested_text !== null &&
            !thread.outdated &&
            !thread.vanished &&
            !thread.resolved &&
            thread.side === "new" &&
            repo.access === "write" && (
              <SuggestionAction
                repo={repo}
                pull={pull}
                thread={thread}
                csrf={csrf}
                refresh={refresh}
              />
            )}
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
            {thread.can_edit && !repo.archived && !editing && (
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
            {thread.can_resolve && (
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
          <ReviewReplies repo={repo} pull={pull} thread={thread} csrf={csrf} />
          <Failure message={mutation.error} />
        </div>
      </details>
    </article>
  );
}
