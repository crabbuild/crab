import { ReviewDismissal } from "@/features/pull-requests/reviews/dismissal";
import { RelativeTime } from "../time";
import { Badge } from "@/components/ui/badge";
import { Author } from "../author";
import { Reactions } from "../reactions";
import { DiscussionMarkdown } from "../markdown";
import { timestamp } from "../time";
import { Link } from "@/components/navigation/link";
import {
  CheckCircleFillIcon,
  CodeReviewIcon,
  XCircleFillIcon,
} from "@primer/octicons-react";
import { cn } from "@/lib/utils";
import { short } from "@/lib/format";
import type { Repository } from "@/lib/api";
import type { Review } from "./model";

export function ReviewCard({
  repo,
  href,
  csrf,
  target,
  review,
  onSaved,
}: {
  repo: Repository;
  href: string;
  target: string;
  csrf: string;
  review: Review;
  onSaved: () => void;
}) {
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
        <ReviewDismissal
          repo={repo}
          target={target}
          csrf={csrf}
          review={review}
          onSaved={onSaved}
        />
      </header>
      {review.body && <DiscussionMarkdown>{review.body}</DiscussionMarkdown>}
      {review.dismissal && (
        <div className="review-dismissal mx-4 mb-4 rounded-md border p-3 text-sm">
          <p className="mb-2 text-muted-foreground">
            <Author author={review.dismissal.author} /> dismissed this review{" "}
            <RelativeTime value={review.dismissal.created_at} />.
          </p>
          <DiscussionMarkdown>{review.dismissal.body}</DiscussionMarkdown>
        </div>
      )}
      <Reactions
        repo={repo}
        csrf={csrf}
        target={`${target}/reviews/${review.number}`}
        summary={review.reactions}
      />
      <footer className="muted">
        Reviewed commit <code>{short(review.commit_oid)}</code>
      </footer>
    </article>
  );
}
