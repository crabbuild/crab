import { Author } from "@/components/discussion/author";
import { endpoint, type Repository } from "@/lib/api";
import { Reactions } from "@/components/discussion/reactions";
import { Editor } from "@/components/discussion/editor";
import { Failure } from "@/components/discussion/failure";
import { useSubmission } from "@/components/discussion/hooks";
import { DiscussionMarkdown } from "@/components/discussion/markdown";
import { useMutation } from "@/components/discussion/mutations";
import { Button } from "@/components/ui/button";
import { Result } from "@/components/request/result";
import { useState } from "react";

import { timestamp } from "@/components/discussion/time";
import type { PullRequest, PullReviewReply, PullReviewThread } from "../model";
import { usePagedRequest } from "../pagination";

export function ReviewReplies({
  repo,
  pull,
  thread,
  csrf,
}: {
  repo: Repository;
  pull: PullRequest;
  thread: PullReviewThread;
  csrf: string;
}) {
  const repliesPath = (before?: number) =>
    endpoint(repo, `pulls/${pull.number}/threads/${thread.number}/replies`, {
      before: before?.toString(),
    });
  const replies = usePagedRequest<PullReviewReply>(repliesPath);
  const [body, setBody] = useState("");
  const mutation = useMutation(csrf);
  const submission = useSubmission();
  async function submit(event: React.FormEvent) {
    event.preventDefault();
    const input = { body };
    const created = await mutation.run<PullReviewReply>(repliesPath(), "POST", {
      ...input,
      request_id: submission(input),
    });
    if (created) {
      setBody("");
      replies.retry();
    }
  }
  return (
    <div className="review-replies">
      <div className="review-replies-heading">
        <strong>Replies</strong>
        {replies.items.length > 0 && (
          <span className="muted">{replies.items.length}</span>
        )}
      </div>
      <Result state={replies} timingLabel="Thread replies">
        {() =>
          replies.items.map((reply) => (
            <ReviewReplyItem
              key={reply.number}
              repo={repo}
              pull={pull}
              thread={thread}
              reply={reply}
              csrf={csrf}
              refresh={replies.retry}
            />
          ))
        }
      </Result>
      {replies.next !== null && (
        <Button variant="outline" size="sm" onClick={replies.loadMore}>
          Load more replies
        </Button>
      )}
      {thread.can_reply && !repo.archived && (
        <form className="review-reply-form" onSubmit={submit}>
          <Editor
            id={`reply-${thread.number}`}
            label="Reply"
            value={body}
            onChange={setBody}
            disabled={mutation.pending}
            required
          />
          <Failure message={mutation.error} />
          <Button
            variant="outline"
            size="sm"
            type="submit"
            disabled={mutation.pending || !body.trim()}
          >
            {mutation.pending ? "Replying…" : "Reply"}
          </Button>
        </form>
      )}
    </div>
  );
}

function ReviewReplyItem({
  repo,
  pull,
  thread,
  reply,
  csrf,
  refresh,
}: {
  repo: Repository;
  pull: PullRequest;
  thread: PullReviewThread;
  reply: PullReviewReply;
  csrf: string;
  refresh: () => void;
}) {
  const mutation = useMutation(csrf);
  const [editing, setEditing] = useState(false);
  const [body, setBody] = useState(reply.body);
  async function save(event: React.FormEvent) {
    event.preventDefault();
    const updated = await mutation.run<PullReviewReply>(
      endpoint(
        repo,
        `pulls/${pull.number}/threads/${thread.number}/replies/${reply.number}`,
      ),
      "PATCH",
      { version: reply.version, body },
    );
    if (updated) {
      setBody(updated.body);
      setEditing(false);
      refresh();
    }
  }
  return (
    <div className="review-reply">
      <strong>
        <Author author={reply.author} />
      </strong>
      <span className="muted">replied {timestamp(reply.created_at)}</span>
      {editing ? (
        <form className="review-reply-edit" onSubmit={save}>
          <Editor
            id={`reply-${thread.number}-${reply.number}-edit`}
            label="Edit reply"
            value={body}
            onChange={setBody}
            disabled={mutation.pending}
            required
            autoFocus
          />
          <div className="discussion-actions">
            <Button
              size="sm"
              variant="default"
              type="submit"
              disabled={mutation.pending || !body.trim()}
            >
              {mutation.pending ? "Saving…" : "Save"}
            </Button>
            <Button
              variant="outline"
              size="sm"
              type="button"
              disabled={mutation.pending}
              onClick={() => {
                setBody(reply.body);
                setEditing(false);
              }}
            >
              Cancel
            </Button>
          </div>
        </form>
      ) : (
        <>
          <DiscussionMarkdown>{body}</DiscussionMarkdown>
          {reply.can_edit && !repo.archived && (
            <Button
              variant="outline"
              size="sm"
              disabled={mutation.pending}
              onClick={() => setEditing(true)}
            >
              Edit
            </Button>
          )}
        </>
      )}
      <Reactions
        repo={repo}
        csrf={csrf}
        target={`pulls/${pull.number}/threads/${thread.number}/replies/${reply.number}`}
        summary={reply.reactions}
      />
      <Failure message={mutation.error} />
    </div>
  );
}
