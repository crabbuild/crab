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
}: {
  repo: Repository;
  href: string;
  target: string;
  csrf: string;
  review: Review;
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
        review.current && "current",
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
      </header>
      {review.body && <DiscussionMarkdown>{review.body}</DiscussionMarkdown>}
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
