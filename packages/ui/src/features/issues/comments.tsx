import { ActivityTimeline } from "@/features/activity";
import { Reactions } from "@/components/discussion/reactions";

import { useEffect, useState } from "react";
import { Button } from "@/components/ui/button";
import { Spinner } from "@/components/ui/spinner";

import { endpoint, useRequest, type Repository } from "@/lib/api";

import { ConflictReview, useMutation } from "@/components/discussion/mutations";

import { DiscussionMarkdown } from "@/components/discussion/markdown";
import { Editor } from "@/components/discussion/editor";
import { Failure } from "@/components/discussion/failure";
import { useReturnFocus, useSubmission } from "@/components/discussion/hooks";

import { Comment, Page } from "./model";
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
  const [before, setBefore] = useState<string>();
  const page = useRequest<Page<Comment>>(
    endpoint(repo, `issues/${issue}/comments`, { before }),
  );
  const [items, setItems] = useState<Record<number, Comment>>({});
  useEffect(() => {
    const incoming = page.data;
    if (incoming)
      setItems((old) => {
        const merged = { ...old };
        for (const item of incoming.items) {
          // An in-flight page must not replace a newer successful edit.
          if (
            !merged[item.number] ||
            merged[item.number].version <= item.version
          )
            merged[item.number] = item;
        }
        return merged;
      });
  }, [page.data]);
  const [activityEpoch, setActivityEpoch] = useState(0);
  function upsert(comment: Comment) {
    setActivityEpoch((value) => value + 1);
    setItems((old) =>
      old[comment.number]?.version > comment.version
        ? old
        : { ...old, [comment.number]: comment },
    );
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
          onClick={() => {
            setBefore(undefined);
            page.retry();
          }}
        >
          Refresh discussion
        </Button>
      </div>
      {page.data?.next && (
        <Button
          variant="outline"
          className="load-comments"
          onClick={() => setBefore(String(page.data?.next))}
          disabled={page.loading}
        >
          Load earlier comments
        </Button>
      )}
      {page.loading && (
        <p role="status">
          <Spinner /> Loading discussion…
        </p>
      )}
      {page.error && (
        <div className="notice error" role="alert">
          <p>{page.error}</p>
          <Button variant="outline" onClick={page.retry}>
            Try again
          </Button>
        </div>
      )}
      {!page.loading && !page.error && !Object.keys(items).length && (
        <p className="muted">
          {page.data?.next
            ? "No comments in this range. Load earlier comments."
            : "No comments yet. Start the conversation."}
        </p>
      )}
      {Object.values(items)
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
    <article id={`comment-${comment.number}`} className="discussion-card panel">
      <header>
        <strong>{comment.author}</strong>
        <time dateTime={new Date(comment.created_at).toISOString()}>
          {timestamp(comment.created_at)}
        </time>
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
