import { Author } from "@/components/discussion/author";
import { type Repository } from "@/lib/api";
import { Link } from "@/components/navigation/link";
import { repoHref } from "@/lib/repository-route";
import { Reactions } from "@/components/discussion/reactions";
import { DiscussionMarkdown } from "@/components/discussion/markdown";
import { short } from "@/lib/format";
import { cn } from "@/lib/utils";
import {
  CheckCircleFillIcon,
  CodeReviewIcon,
  XCircleFillIcon,
} from "@primer/octicons-react";

import { timestamp } from "@/components/discussion/time";
import type { PullComment, PullReview } from "../model";

export function PullTimeline({
  repo,
  pull,
  csrf,
  comments,
  reviews,
}: {
  repo: Repository;
  pull: number;
  csrf: string;
  comments: PullComment[];
  reviews: PullReview[];
}) {
  const events = [
    ...comments.map((comment) => ({
      kind: "comment" as const,
      value: comment,
    })),
    ...reviews.map((review) => ({ kind: "review" as const, value: review })),
  ].sort((left, right) => left.value.created_at - right.value.created_at);
  return (
    <div className="discussion-thread pull-timeline">
      {events.map((event) =>
        event.kind === "comment" ? (
          <article
            id={`issuecomment-${event.value.number}`}
            tabIndex={-1}
            className="panel discussion-card conversation-contribution"
            key={`comment-${event.value.number}`}
          >
            <header>
              <strong>
                <Author author={event.value.author} />
              </strong>
              <Link
                className="muted"
                aria-label={`Comment #${event.value.number} permalink`}
                href={`${repoHref(repo, { view: "pulls", pull: String(pull) })}#issuecomment-${event.value.number}`}
              >
                commented{" "}
                <time dateTime={new Date(event.value.created_at).toISOString()}>
                  {timestamp(event.value.created_at)}
                </time>
              </Link>
            </header>
            <DiscussionMarkdown>{event.value.body}</DiscussionMarkdown>
            <Reactions
              repo={repo}
              csrf={csrf}
              target={`pulls/${pull}/comments/${event.value.number}`}
              summary={event.value.reactions}
            />
          </article>
        ) : (
          <ReviewEvent
            key={`review-${event.value.number}`}
            repo={repo}
            pull={pull}
            csrf={csrf}
            review={event.value}
          />
        ),
      )}
    </div>
  );
}

function ReviewEvent({
  repo,
  pull,
  csrf,
  review,
}: {
  repo: Repository;
  pull: number;
  csrf: string;
  review: PullReview;
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
          href={`${repoHref(repo, { view: "pulls", pull: String(pull) })}#pullrequestreview-${review.number}`}
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
        target={`pulls/${pull}/reviews/${review.number}`}
        summary={review.reactions}
      />
      <footer className="muted">
        Reviewed commit <code>{short(review.commit_oid)}</code>
      </footer>
    </article>
  );
}
