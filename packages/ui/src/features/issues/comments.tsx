import { Author } from "@/components/discussion/author";
import { ActivityTimeline } from "@/features/activity";
import { Reactions } from "@/components/discussion/reactions";

import { useState } from "react";
import {
  HistoryControls,
  useDiscussionHistory,
} from "@/components/discussion/history";
import {
  useContributionFocus,
  useDiscussionPermalink,
} from "@/components/discussion/permalink";
import { RequestWidget } from "@/components/request/result";
import { Link } from "@/components/navigation/link";
import { repoHref } from "@/lib/repository-route";
import { Button } from "@/components/ui/button";

import { endpoint, type Repository } from "@/lib/api";

import { ConflictReview, useMutation } from "@/components/discussion/mutations";

import { DiscussionMarkdown } from "@/components/discussion/markdown";
import { Editor } from "@/components/discussion/editor";
import { Failure } from "@/components/discussion/failure";
import { useReturnFocus, useSubmission } from "@/components/discussion/hooks";

import { Comment } from "./model";
import { timestamp } from "@/components/discussion/time";

export function Comments({
  repo,
  issue,
  version,
  csrf,
}: {
  repo: Repository;
  issue: number;
  version: string;
  csrf: string;
}) {
  const focus = useDiscussionPermalink();
  const page = useDiscussionHistory<Comment>(
    repo,
    `issues/${issue}/comments`,
    focus?.kind === "issuecomment" ? focus.number : undefined,
  );
  const ready =
    focus?.kind === "issuecomment" &&
    !page.loading &&
    !page.linked.loading &&
    page.items.some((item) => item.number === focus.number);
  useContributionFocus(
    focus?.kind === "issuecomment" ? focus.id : undefined,
    ready,
  );
  const [activityEpoch, setActivityEpoch] = useState(0);
  function upsert(comment: Comment) {
    setActivityEpoch((value) => value + 1);
    page.upsert([comment]);
  }
  return (
    <section className="issue-comments" aria-label="Discussion">
      <ActivityTimeline
        key={`${version}:${activityEpoch}`}
        repo={repo}
        target={`issues/${issue}`}
      />
      <div className="section-heading">
        <h3>Discussion</h3>
        <Button
          variant="outline"
          size="sm"
          disabled={page.loading}
          onClick={page.refresh}
        >
          Refresh discussion
        </Button>
      </div>
      <RequestWidget timing={page.timing} label="Issue comments">
        <HistoryControls label="comments" history={page} />
        {!page.loading && !page.error && !page.items.length && (
          <p className="muted">
            {page.data?.next
              ? "No comments in this range. Load earlier comments."
              : "No comments yet. Start the conversation."}
          </p>
        )}
        {page.items
          .sort((a, b) => a.number - b.number)
          .map((comment) => (
            <CommentCard
              key={comment.number}
              comment={comment}
              repo={repo}
              issue={issue}
              csrf={csrf}
              onSaved={upsert}
            />
          ))}
      </RequestWidget>
      {!repo.archived && (
        <NewComment repo={repo} issue={issue} csrf={csrf} onCreated={upsert} />
      )}
    </section>
  );
}

function NewComment({
  repo,
  issue,
  csrf,
  onCreated,
}: {
  repo: Repository;
  issue: number;
  csrf: string;
  onCreated: (comment: Comment) => void;
}) {
  const [body, setBody] = useState("");
  const mutation = useMutation(csrf);
  const submission = useSubmission();
  const editor = useReturnFocus<HTMLTextAreaElement>(mutation.pending);
  const [posted, setPosted] = useState(false);
  const [completedSubmission, setCompletedSubmission] = useState(0);
  return (
    <form
      className="discussion-compose new-comment"
      onSubmit={async (event) => {
        event.preventDefault();
        const input = { body };
        const comment = await mutation.run<Comment>(
          endpoint(repo, `issues/${issue}/comments`),
          "POST",
          { ...input, request_id: submission(input) },
        );
        if (comment) {
          setPosted(true);
          setCompletedSubmission(comment.number);
          onCreated(comment);
          setBody("");
          submission({ body: "" });
        }
      }}
    >
      <h3>Add a comment</h3>
      <Editor
        key={completedSubmission}
        ref={editor}
        id="new-comment"
        label="Comment"
        value={body}
        onChange={(value) => {
          setBody(value);
          setPosted(false);
        }}
        disabled={mutation.pending}
        required
      />
      <Failure message={mutation.error} />
      <div role="status">{posted ? "Comment posted." : ""}</div>
      <div className="discussion-actions">
        <Button
          type="submit"
          variant="default"
          disabled={mutation.pending || !body.trim()}
        >
          {mutation.pending ? "Posting…" : "Comment"}
        </Button>
      </div>
    </form>
  );
}

function CommentCard({
  comment,
  repo,
  issue,
  csrf,
  onSaved,
}: {
  comment: Comment;
  repo: Repository;
  issue: number;
  csrf: string;
  onSaved: (comment: Comment) => void;
}) {
  const [draft, setDraft] = useState<{ body: string; version: number }>();
  const mutation = useMutation(csrf);
  const editTrigger = useReturnFocus<HTMLButtonElement>(Boolean(draft));
  const save = useReturnFocus<HTMLButtonElement>(mutation.conflict);
  return (
    <article
      id={`issuecomment-${comment.number}`}
      tabIndex={-1}
      className="discussion-card panel conversation-contribution"
    >
      <header>
        <strong>
          <Author author={comment.author} />
        </strong>
        <Link
          className="muted"
          aria-label={`Comment #${comment.number} permalink`}
          href={`${repoHref(repo, { view: "issues", issue: String(issue) })}#issuecomment-${comment.number}`}
        >
          <time dateTime={new Date(comment.created_at).toISOString()}>
            {timestamp(comment.created_at)}
          </time>
        </Link>
        {comment.version > 1 && <span className="muted">edited</span>}
        {comment.can_edit && !repo.archived && !draft && (
          <Button
            variant="outline"
            ref={editTrigger}
            size="sm"
            onClick={() => {
              mutation.reset();
              setDraft({ body: comment.body, version: comment.version });
            }}
          >
            Edit comment
          </Button>
        )}
      </header>
      {draft ? (
        <form
          className="comment-edit"
          onSubmit={async (event) => {
            event.preventDefault();
            if (mutation.conflict) return;
            const updated = await mutation.run<Comment>(
              endpoint(repo, `issues/${issue}/comments/${comment.number}`),
              "PATCH",
              draft,
            );
            if (updated) {
              onSaved(updated);
              setDraft(undefined);
            }
          }}
        >
          <Editor
            autoFocus
            id={`edit-comment-${comment.number}`}
            label="Edit comment"
            value={draft.body}
            onChange={(body) => setDraft({ ...draft, body })}
            disabled={mutation.pending}
            required
          />
          {mutation.conflict ? (
            <ConflictReview<Comment>
              url={endpoint(repo, `issues/${issue}/comments/${comment.number}`)}
              onResolve={(latest, choice) => {
                setDraft({
                  body: choice === "saved" ? latest.body : draft.body,
                  version: latest.version,
                });
                onSaved(latest);
                mutation.reset();
              }}
            />
          ) : (
            <Failure message={mutation.error} />
          )}
          <div className="discussion-actions">
            <Button
              ref={save}
              type="submit"
              variant="default"
              disabled={
                mutation.pending || mutation.conflict || !draft.body.trim()
              }
            >
              Save comment
            </Button>
            <Button
              variant="outline"
              type="button"
              onClick={() => setDraft(undefined)}
              disabled={mutation.pending}
            >
              Cancel edit
            </Button>
          </div>
        </form>
      ) : (
        <>
          <DiscussionMarkdown>{comment.body}</DiscussionMarkdown>
          <Reactions
            repo={repo}
            target={`issues/${issue}/comments/${comment.number}`}
            summary={comment.reactions}
            csrf={csrf}
          />
        </>
      )}
    </article>
  );
}
