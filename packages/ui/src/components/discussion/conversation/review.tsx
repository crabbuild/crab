import { useState } from "react";
import { EditDiscussion } from "./edit";
import { EditHistory } from "./edit-history";
import { CommentVisibility } from "./comment-visibility";
import { useConversationLock } from "./lock";
import { ReviewDismissal } from "@/features/pull-requests/reviews/dismissal";
import { RelativeTime } from "../time";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Author } from "../author";
import { Reactions } from "../reactions";
import { DiscussionMarkdown } from "../markdown";
import { useReturnFocus } from "../hooks";
import { useMutation } from "../mutations";
import { timestamp } from "../time";
import { Link } from "@/components/navigation/link";
import {
  CheckCircleFillIcon,
  CodeReviewIcon,
  XCircleFillIcon,
} from "@primer/octicons-react";
import { cn } from "@/lib/utils";
import { short } from "@/lib/format";
import { endpoint, type Repository } from "@/lib/api";
import type { Review } from "./model";

export function ReviewCard({
  repo,
  href,
  csrf,
  target,
  review,
  onEdited,
  onDismissed,
  onVisibilityChanged,
}: {
  repo: Repository;
  href: string;
  target: string;
  csrf: string;
  review: Review;
  onEdited: (saved: Review) => void;
  onDismissed: () => void;
  onVisibilityChanged: () => void;
}) {
  const [editing, setEditing] = useState(false);
  const canPost = useConversationLock()?.lock?.can_post !== false;
  const canEdit = review.can_edit && canPost && !repo.archived;
  const mutation = useMutation(csrf, repo);
  const edit = useReturnFocus<HTMLButtonElement>(editing);
  const path = `${target}/reviews/${review.number}`;
  const action =
    review.state === "approved"
      ? "approved these changes"
      : review.state === "changes_requested"
        ? "requested changes"
        : "left a review";
  const Icon =
    review.state === "approved"
      ? CheckCircleFillIcon
      : review.state === "changes_requested"
        ? XCircleFillIcon
        : CodeReviewIcon;
  return (
    <article
      id={`pullrequestreview-${review.number}`}
      tabIndex={-1}
      className={cn(
        "panel review-event conversation-contribution",
        review.state,
        review.current && review.state !== "dismissed" && "current",
      )}
    >
      <header>
        <span className="review-icon" aria-hidden="true">
          <Icon />
        </span>
        <strong>
          <Author author={review.author} />
        </strong>
        <span> {action}</span>
        {review.state === "dismissed" && (
          <Badge variant="secondary">Dismissed</Badge>
        )}
        {review.current !== true && (
          <span className="review-outdated">
            {review.current === null ? "Head unavailable" : "Outdated"}
          </span>
        )}
        <Link
          className="muted"
          aria-label={`Review #${review.number} permalink`}
          href={`${href}#pullrequestreview-${review.number}`}
        >
          <time dateTime={new Date(review.created_at).toISOString()}>
            {timestamp(review.created_at)}
          </time>
        </Link>
        {review.has_edit_history && (
          <EditHistory repo={repo} csrf={csrf} target={path} />
        )}
        <ReviewDismissal
          repo={repo}
          target={target}
          csrf={csrf}
          review={review}
          onSaved={onDismissed}
        />
      </header>

      {review.dismissal && (
        <div className="review-dismissal mx-4 mb-4 rounded-md border p-3 text-sm">
          <p className="mb-2 text-muted-foreground">
            <Author author={review.dismissal.author} /> dismissed this review{" "}
            <RelativeTime value={review.dismissal.created_at} />.
          </p>
          <DiscussionMarkdown>{review.dismissal.body}</DiscussionMarkdown>
        </div>
      )}
      <CommentVisibility
        repo={repo}
        csrf={csrf}
        target={path}
        minimization={review.minimization}
        onChanged={onVisibilityChanged}
        retainDraft={editing}
      >
        {editing ? (
          <div className="comment-edit">
            <EditDiscussion
              source={review}
              url={endpoint(repo, path)}
              repositoryId={repo.id}
              id={`edit-review-${review.number}`}
              bodyLabel="Review summary"
              bodyRequired={
                review.state === "commented" ||
                review.state === "changes_requested"
              }
              canEdit={canEdit}
              mutation={mutation}
              onCancel={() => setEditing(false)}
              onSaved={(saved) => {
                onEdited(saved);
                setEditing(false);
              }}
              onReviewed={onEdited}
            />
          </div>
        ) : (
          <>
            {canEdit && (
              <Button
                ref={edit}
                variant="outline"
                size="sm"
                className="mx-4 mt-3"
                onClick={() => {
                  mutation.reset();
                  setEditing(true);
                }}
              >
                Edit review summary
              </Button>
            )}
            {review.body && (
              <DiscussionMarkdown>{review.body}</DiscussionMarkdown>
            )}
            <Reactions
              repo={repo}
              csrf={csrf}
              target={path}
              summary={review.reactions}
            />
          </>
        )}
      </CommentVisibility>
      <footer className="muted">
        Reviewed commit <code>{short(review.commit_oid)}</code>
      </footer>
    </article>
  );
}
