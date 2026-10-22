import { useState } from "react";
import { Author } from "../author";
import { Reactions } from "../reactions";
import { timestamp } from "../time";
import { useReturnFocus } from "../hooks";
import { ConflictReview, useMutation } from "../mutations";
import { DiscussionMarkdown } from "../markdown";
import { Editor } from "../editor";
import { Failure } from "../failure";
import { Link } from "@/components/navigation/link";
import { Button } from "@/components/ui/button";
import { endpoint, type Repository } from "@/lib/api";
import type { Comment } from "./model";

export function CommentCard({
  comment,
  repo,
  target,
  href,
  csrf,
  onSaved,
}: {
  comment: Comment;
  repo: Repository;
  target: string;
  href: string;
  csrf: string;
  onSaved: (comment: Comment) => void;
}) {
  const [draft, setDraft] = useState<{ body: string; version: number }>();
  const mutation = useMutation(csrf, repo);
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
          href={`${href}#issuecomment-${comment.number}`}
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
              endpoint(repo, `${target}/comments/${comment.number}`),
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
              url={endpoint(repo, `${target}/comments/${comment.number}`)}
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
            target={`${target}/comments/${comment.number}`}
            summary={comment.reactions}
            csrf={csrf}
          />
        </>
      )}
    </article>
  );
}
